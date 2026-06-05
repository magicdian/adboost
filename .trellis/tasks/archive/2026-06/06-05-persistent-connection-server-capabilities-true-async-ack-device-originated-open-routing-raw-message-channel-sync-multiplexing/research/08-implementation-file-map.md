# Research: Implementation File Map for the 5 Persistent-Connection PRs

- **Query**: Map the EXACT files, symbols, and line ranges each of the 5 implementation PRs will touch (feature lives in XPENG-only `persistent.rs` + immediate support files), to curate `implement.jsonl` context.
- **Scope**: internal (source read at HEAD `be78d76`)
- **Date**: 2026-06-05

## Source-of-truth line anchors (verified by read, not guessed)

`adb_client/src/message_devices/usb/persistent.rs` = **826 lines**. Confirmed anchors:

| Symbol | Lines |
|---|---|
| `const SESSION_CHANNEL_SIZE: usize = 64` | 31 |
| `pub struct PersistentUsbConnection { writer, sessions, reader_handle, shutdown }` | 37-46 |
| `pub fn new(transport, private_key_path)` | 52-99 |
| `pub fn new_from_ids(vendor_id, product_id, private_key_path)` | 102-109 |
| `fn do_connect(transport, private_key)` | 112-171 |
| hardcoded `banner` string (the lie) | **127** (single line, 18 features incl `delayed_ack`, `shell_v2`, 5× `sendrecv_v2*`) |
| CNXN build+send (`try_new(Cnxn..banner)` + `write_message`) | 128-134 (inside `do_connect`) |
| `fn do_auth` | 173-207 |
| `fn reader_loop(transport, sessions, shutdown)` | 210-267 |
| reader read+timeout match | 220-236 |
| reader route-by-`arg1` demux (the switch) | 238-264 (`target_id = msg.header().arg1()`; OKAY→`ack_tx`, else→`data_tx`) |
| `pub fn open_session(&self, cmd) -> Result<MultiplexedSession>` | 275-343 |
| channel creation (`SESSION_CHANNEL_SIZE`) | 280-281 |
| register-before-OPEN | 283-287 |
| OPEN build+write (`arg0=local_id, arg1=0`) | 289-305 |
| wait first OKAY on ack channel | 307-321 (`remote_id = response.header().arg0()`) |
| **initial OKAY (`ready_msg`, empty payload)** | 323-330 |
| `pub fn shell_exec(&self, cmd) -> Result<(String, Option<u8>)>` | 345-366 |
| `pub fn is_alive` | 369-375 |
| `impl Drop for PersistentUsbConnection` | 378-390 |
| `pub struct MultiplexedSession { local_id, remote_id, writer, data_rx, ack_rx, sessions, read_buf, read_pos, closed }` | 397-409 |
| `pub struct SessionChannels { data_tx, ack_tx }` | 411-415 |
| `MultiplexedSession::into_split` (+ dummy-rx forget trick) | 433-494 |
| `struct SessionCleanup` + its `Drop` | 497-523 |
| `pub struct SessionReadHalf` | 525-535 |
| `pub struct SessionWriteHalf` | 537-545 |
| `impl Read for SessionReadHalf` (read half A) | 547-612 (OKAY sent at 573-585) |
| `impl Write for SessionWriteHalf` (write half A) | 614-671 (WRTE 626-639, wait OKAY 641-665) |
| `impl Read for MultiplexedSession` (read half B) | 673-742 (OKAY sent at 701-714) |
| `impl Write for MultiplexedSession` (write half B) | 744-802 (WRTE 757-770, wait OKAY 772-796) |
| `impl Drop for MultiplexedSession` | 804-824 |

`usb/mod.rs` re-exports (line 7-9): `MultiplexedSession, PersistentUsbConnection, SessionReadHalf, SessionWriteHalf`. `usb_transport` is `pub(crate)`; `USBTransport` re-exported line 10.

**External callers of the new API:** NONE in-tree. `PersistentUsbConnection` / `MultiplexedSession` / `shell_exec` / `open_session(self)` / `into_split` are not referenced anywhere outside `persistent.rs` (no `bin/`, no CLI, no tests). The only `open_session` callers found are the *generic* `ADBMessageDevice::open_session` (different type). This means signature changes to the persistent API have zero downstream blast radius today — only `usb/mod.rs` re-exports may need new names.

---

## Shared infrastructure both `ADBMessageTransport`/message support files expose (read-only reference)

- `ADBTransportMessage` (`adb_transport_message.rs`): `try_new(command, arg0, arg1, &data)` (110), `header()` (141), `payload() -> &Vec<u8>` (145), `into_payload() -> Vec<u8>` (149). Header accessors `command()` 41, `arg0()` 45, `arg1()` 49, `data_length()` 53. **All payload bytes are accessible**; there is NO helper to read a 4-byte LE i32 from the payload yet — PR3 must add that parse inline (e.g. `i32::from_le_bytes(payload[..4])`).
- `MessageCommand` enum (`message_commands.rs:10-27`): `Cnxn, Clse, Auth, Open, Write, Okay, Stls`. `Open = 0x4E45_504F`. `MessageSubcommand` (35-46): `Stat, Send, Recv, Quit, Fail, Done, Data, List` (the SYNC opcodes) + `with_arg(u32) -> SubcommandWithArg` (62) for PR4.
- `ADBLocalCommand` (`models/adb_local_command.rs`): `Sync => "sync:"` (37), `ShellCommand(cmd, args)` shell-v1 vs shell-v2 formatting (38-47: `args.is_empty()` ⇒ `shell:`; else `shell,{args},raw:`). PR5 shell-v2 will pass non-empty args.
- `RustADBError` (`error.rs:8-164`): existing relevant variants `ADBShellV2ParseError(String)` (12-14), `ADBRequestFailed(String)`, `WrongResponseReceived(String,String)`, `UsbTimeout` (93-96), `SendError` (149-151), `ConversionError`. `thiserror`-derived; new variants are append-only enum arms.
- `ShellChannel` enum + `TryFrom<u8>` reference for PR5 lives in `server_device/adb_server_device_commands.rs:17-38` (Stdout=1, Stderr=2, ExitStatus=3). The 5-byte-frame parse loop is `shell_command_v2` at **163-266** (metadata read 193-205, stdout/stderr drain 211-248, exit-status 249-263). This is `ADBServerDevice` (TCP-server transport), not USB — PR5 must port the framing, not call it.
- SYNC client reference logic for PR4: `commands/push.rs` (`push` → `open_synchronization_session` 16, SEND framing 18-31, `session.push_file` 31, `end_transaction` 32), `commands/pull.rs` (`pull` 14-54, STAT-then-RECV, `recv_file` 51), `commands/stat.rs` (8-15), `commands/list.rs` (`list` 18-25, `handle_list` 83-171). The reusable engine is on `ADBSession<T>` in `adb_session.rs`: `push_file` 115-180, `recv_file` 78-113, `stat_with_explicit_ids` 182-203, `send_and_expect_okay` 66-76, `recv_and_reply_okay` 54-63. `open_synchronization_session` itself = `adb_message_device.rs:154-156` (just `open_session(&ADBLocalCommand::Sync)`).

---

## PR1 — #6 Honest banner + `DeviceFeatureSet`

| Item | Detail |
|---|---|
| **Files to modify** | `persistent.rs` only (banner build site + add struct/consts + 2 ctors + accessor). `usb/mod.rs` if `DeviceFeatureSet` is re-exported. |
| **New files/modules** | Optional: a small `usb/feature_set.rs` (or inline in `persistent.rs`). Brainstorm leaning is inline given size. No mandatory new file. |
| **Banner build/send site** | Built+sent in `do_connect` at `persistent.rs:127-134` (string literal 127, `try_new(Cnxn, 0x0100_0000, 1_048_576, banner.as_bytes())` 128-133, `write_message` 134). `do_connect` is `fn do_connect(transport, private_key)` (112) — **takes no feature set today**; PR1 must thread a `&DeviceFeatureSet` (or built banner string) into it from `new`/`new_from_ids`. |
| **Existing constructors** | `new(transport, private_key_path)` (52), `new_from_ids(vendor_id, product_id, private_key_path)` (102→calls `new`). Add `new_with_features(transport, private_key_path, features: DeviceFeatureSet)` and route the existing `new` to it with a default set. `new_from_ids` likewise gains a `_with_features` sibling or default. |
| **New symbols** | `pub struct DeviceFeatureSet` (holds which features to advertise), `DeviceFeatureSet::new_with_features(...)` constructor (per brief naming) on the connection: `pub fn new_with_features(...)`, accessor `pub fn device_features(&self) -> &DeviceFeatureSet` on `PersistentUsbConnection`. Feature-name `const` strings: e.g. `const FEATURE_SHELL_V2: &str = "shell_v2"`, `const FEATURE_DELAYED_ACK: &str = "delayed_ack"`, `const FEATURE_CMD`, `const FEATURE_STAT_V2`, `const FEATURE_LS_V2`, `const FEATURE_SENDRECV_V2`, etc. (subset gating per synthesis doc: drop the 4 `sendrecv_v2_*`, keep `delayed_ack` only if PR3 lands, keep `shell_v2` only if PR5 lands). To store the active set on the struct, add a field to `PersistentUsbConnection` (37-46). |
| **Signature/behavior changes** | `do_connect` gains a feature param. `PersistentUsbConnection` struct gains a `features`/`feature_set` field (37-46 + all 3 construction sites at 75-98). |
| **error.rs additions** | None required. |
| **lib.rs / mod.rs re-exports** | `usb/mod.rs:7-9` add `DeviceFeatureSet` to the `pub use persistent::{...}`. `lib.rs:36` already does `pub use message_devices::*`, so it propagates automatically. |

---

## PR2 — reader_loop redesign: #2 device-OPEN routing + #3 raw channel

| Item | Detail |
|---|---|
| **Files to modify** | `persistent.rs` (reader_loop demux + struct fields + new accessors). `usb/mod.rs` if new public types exported. `error.rs` likely (overflow / no-listener variant). |
| **Reader loop** | `reader_loop` at **210-267**; the demux switch to edit is **238-264** (route-by-`arg1`). Today: `target_id = arg1`; if session exists, OKAY→`ack_tx` else→`data_tx`; unknown → drop+trace (258-264). **The single bulk-IN owner** — there can be only ONE reader (spawned 84-89, holds the IN-endpoint mutex via `usb_transport.rs:310`). #2 and #3 MUST be implemented as ONE reader_loop redesign (see cross-PR flags). |
| **#2 inbound-OPEN detection** | Add a branch: `if msg.header().command() == MessageCommand::Open` (device-originated OPEN; per AOSP its `arg0` = device's local id, `arg1` = 0 for the host, or = host's window grant under delayed_ack — see `07-...md` Q2). Route into a **bounded** `pending_opens` queue. Per AOSP `delayed_ack` invariant, also need to validate `(arg1 != 0) == delayed_ack_negotiated` (07-...md Q6 §"What IS fatal"). |
| **#3 raw subscribe/send** | Add `subscribe_raw(filter)` tee — reader, after demuxing, also forwards a clone/copy of matching messages to subscriber channel(s); and `send_raw(msg)` to push an arbitrary `ADBTransportMessage` through the writer mutex. NOTE: `ADBTransportMessage` is **not `Clone`** today (no derive in `adb_transport_message.rs:12-16`); a tee either needs `#[derive(Clone)]` added there or to re-wrap header+payload. Flag: deriving `Clone` on `ADBTransportMessage` is a small support-file edit (`adb_transport_message.rs`). |
| **Structures holding sessions/channels** | `sessions: Arc<Mutex<HashMap<u32, SessionChannels>>>` field (41) — used in reader_loop (247-248) and `open_session` (285-286) and cleanup (519-521, 820-822). `SessionChannels { data_tx, ack_tx }` (411-415). Add new fields to `PersistentUsbConnection` (37-46): a `pending_opens` bounded queue (e.g. `SyncSender<ADBTransportMessage>` + paired `Receiver` handed out via `incoming_opens()`), and a raw-subscriber registry (e.g. `Arc<Mutex<Vec<(Filter, SyncSender<...>)>>>`). The reader thread needs clones of these (cloned at 79-82 alongside `reader_sessions`/`reader_shutdown`; `reader_loop` signature 210-214 gains params). |
| **Writer mutex** | `writer: Arc<Mutex<USBTransport>>` (39). Locked in `open_session` (302-305, 325-330), both write halves (581-585, 634-639, 709-714, 765-770), and cleanup/drop (515-517, 814-816, 386-388). `send_raw` and any OPEN-reply OKAY go through this same lock. |
| **New symbols** | `pub fn incoming_opens(&self) -> Receiver<ADBTransportMessage>` (or a typed `IncomingOpen`), `pub fn subscribe_raw(&self, filter: ...) -> Receiver<ADBTransportMessage>`, `pub fn send_raw(&self, msg: ADBTransportMessage) -> Result<()>`. `reader_loop` signature changes (210-214). `PersistentUsbConnection` struct fields added (37-46) + construction (75-98). |
| **error.rs additions** | Likely `PendingOpenQueueFull` / reuse `SendError` (149-151) for bounded-queue overflow; or a dedicated variant if overflow policy needs distinguishing. Decision is brainstorm Q3 (drop-oldest vs reject). |
| **lib.rs / mod.rs re-exports** | `usb/mod.rs:7-9` add any new public type (e.g. an `IncomingOpen` wrapper). |

---

## PR3 — #1 delayed_ack windowing (per-session FlowControl)

| Item | Detail |
|---|---|
| **Files to modify** | `persistent.rs` (both write halves, both read halves, `open_session` initial-OKAY, channel sizing, struct fields). `adb_transport_message.rs` if a payload-i32 helper is added (optional — can parse inline). `error.rs` possibly (window/parse error). `usb/mod.rs` if write API shape changes publicly. |
| **Two write halves** | `SessionWriteHalf::write` **614-666** (WRTE 626-639; wait-OKAY/`recv_timeout` 641-649; OKAY/CLSE match 651-665). `MultiplexedSession::write` **744-797** (WRTE 757-770; wait-OKAY 772-781; match 783-796). Today both are strict stop-and-wait (send WRTE → block on `ack_rx` → return). PR3 replaces with windowed: debit `available_bytes -= chunk_len`; only block when `available_bytes <= 0`; OKAY arrivals credit the window asynchronously (07-...md §"Rust implementation implications" steps 5-6). |
| **Two read halves** | `SessionReadHalf::read` **547-612** (sends bare OKAY at 573-585 on each WRTE). `MultiplexedSession::read` **673-742** (bare OKAY at 701-714). PR3 changes the emitted OKAY from empty payload to a **4-byte LE i32 = bytes just flushed** (07-...md step 7) when delayed_ack negotiated; keep empty in classic mode. |
| **try_send sites (SESSION_CHANNEL_SIZE)** | reader OKAY→`ack_tx.try_send` at **251**, data→`data_tx.try_send` at **255**; channel size const **31**; channels created **280-281**. With windowing, OKAYs become a continuous stream (not 1:1 with WRTE) so the bounded `ack_tx` (size 64) sizing/overflow needs review; `try_send` silently drops on full today (`let _ =`). |
| **Initial OKAY** | `ready_msg` at **323-330** (empty payload, `try_new(Okay, local_id, remote_id, &[])`). Under delayed_ack this becomes `OKAY(payload = INITIAL_DELAYED_ACK_BYTES as i32 LE)` to grant the receive window (07-...md step 4/Q2). Also the **OPEN** at 299-300 currently sends `arg1 = 0`; under delayed_ack it must send `arg1 = INITIAL_DELAYED_ACK_BYTES (0x0200_0000 = 32 MiB)` and init own send window to 0 (07-...md step 3). |
| **Where FlowControl lives** | Per-session. Natural home: a new `struct FlowControl { available_bytes: Option<i64> }` (None = classic) stored on `MultiplexedSession` (397-409), and threaded through `into_split` (433-494) into BOTH `SessionReadHalf` (525-535, for the receive-side ack accounting) and `SessionWriteHalf` (537-545, for the send-window debit/credit). Because the OKAY credit arrives via `ack_rx` (read by the write half at 642/773) but is produced asynchronously by the reader thread, the write half's window state must be shareable/updatable — likely `Arc<Mutex<FlowControl>>` or atomic `i64`, mirrored on the cleanup-shared `Arc` like `closed` (464-471). |
| **OKAY payload parse/emit** | Emit: append `(bytes_flushed as i32).to_le_bytes()` to the read-half OKAY (replace `&[]` at 577/705). Parse: in the write half's OKAY handler (651/783) read `i32::from_le_bytes(payload[..4])`, treat len-0 as classic/None, reject len ∉ {0,4}. `ADBTransportMessage::payload()` (145) / `into_payload()` (149) give the bytes; `header().arg0()`/`arg1()` (45/49) stay as socket IDs (NOT the count — 07-...md Q1a explicitly: count is in PAYLOAD, not arg0/arg1). |
| **New constants** | `const INITIAL_DELAYED_ACK_BYTES: i64 = 32 << 20`, `const MAX_PAYLOAD: usize = 1 << 20`, `const MAX_PAYLOAD_V1: usize = 4096` (07-...md step 8). The existing per-WRTE chunk cap is `min(buf.len(), 65536)` (623/754) — windowing decouples in-flight bytes from this chunk size. |
| **error.rs additions** | Possibly a variant for malformed OKAY payload (len ∉ {0,4}) or window mismatch; or reuse `ADBRequestFailed`/`ConversionError`. |
| **lib.rs / mod.rs re-exports** | None unless a public `FlowControl`/new write API is exported (likely kept private). |

---

## PR4 — #4 SYNC v1 multiplexed (`open_sync_session()`)

| Item | Detail |
|---|---|
| **Files to modify** | `persistent.rs` (add `open_sync_session()` + a SYNC wrapper type, or reuse `MultiplexedSession`). `usb/mod.rs` re-export if new public type. |
| **New files/modules** | Optional `usb/persistent_sync.rs` for a `MultiplexedSyncSession` wrapper. Brainstorm leaning (synthesis doc MVP item 3): "薄封装" — a thin wrapper over `open_session(&ADBLocalCommand::Sync)`. |
| **What `open_sync_session()` wraps** | Wraps `self.open_session(&ADBLocalCommand::Sync)` (275) — exactly mirroring the generic `open_synchronization_session` at `adb_message_device.rs:154-156`. `ADBLocalCommand::Sync` formats to `"sync:"` (`adb_local_command.rs:37`). SYNC framing rides the existing `arg1` demux (no reader_loop edit needed — synthesis doc: "SYNC 帧已按 arg1 路由，无新增编辑"). |
| **SYNC opcodes** | `MessageSubcommand` in `message_commands.rs:35-46`: `Stat=0x53544154, Send, Recv, Quit, Fail, Done, Data, List`, `.with_arg(u32)` 62. Reusable on the wire via WRTE payloads. |
| **Reference client logic to port** | The engine methods are on `ADBSession<T>` (`adb_session.rs`): `push_file` 115-180, `recv_file` 78-113, `stat_with_explicit_ids` 182-203, `send_and_expect_okay` 66-76, `recv_and_reply_okay` 54-63. Higher-level flows: `push.rs` 14-36, `pull.rs` 14-55, `stat.rs` 8-15, `list.rs` 18-171. These all assume the **owning** `ADBSession<T>` transport model (synchronous read/write on `T`); the persistent wrapper must reimplement the same SEND/RECV/STAT/LIST framing over `MultiplexedSession`'s `Read+Write` (or `SessionReadHalf`/`SessionWriteHalf`) instead of `ADBSession`. The `end_transaction` (QUIT) pattern is `adb_message_device.rs:194-205`. |
| **Signature/behavior changes** | New `pub fn open_sync_session(&self) -> Result<MultiplexedSyncSession>` (or returns `MultiplexedSession`) on `PersistentUsbConnection`. **Sequencing dependency**: SYNC push/pull are themselves WRTE/OKAY streams, so they ride directly on PR3's write/read-half semantics. If PR3 changes blocking→windowed, the SYNC framing must be built against the final shape (synthesis doc: "#4 直接骑在写/读半语义上 … 否则 #4 重写两次"). |
| **error.rs additions** | Possibly a SYNC `FAIL` variant; or reuse `ADBRequestFailed`/`UnknownResponseType`. |
| **lib.rs / mod.rs re-exports** | `usb/mod.rs:7-9` add `MultiplexedSyncSession` if introduced. |

---

## PR5 — #5 shell-v2 (`ShellV2Session` wrapping `MultiplexedSession`)

| Item | Detail |
|---|---|
| **Files to modify** | `persistent.rs` (`shell_exec` rewrite + add `ShellV2Session`). `error.rs` reuse `ADBShellV2ParseError` (already exists, 12-14). `usb/mod.rs` re-export. |
| **New files/modules** | Optional `usb/shell_v2.rs` for `ShellV2Session` + a `ShellChannel`-equivalent enum (the existing one in `adb_server_device_commands.rs:17-38` is private to that module). |
| **`shell_exec`** | `persistent.rs:345-366`. Today opens `ADBLocalCommand::ShellCommand(cmd, vec![])` (347 — **empty args ⇒ shell-v1** per `adb_local_command.rs:39-46`), loops `session.read` into a buffer (351-359), returns `(text, None)` — **never parses v2 frames, always returns `None` exit code** (the lie #5 fixes). PR5: pass non-empty args (e.g. `vec!["v2"]`) so the service string becomes `shell,v2,raw:{cmd}` (or `shell,...,raw:`), then decode frames. |
| **Reference frame parser to port** | `server_device/adb_server_device_commands.rs`: `ShellChannel` enum + `TryFrom<u8>` **17-38** (Stdout=1/Stderr=2/ExitStatus=3); `shell_command_v2` **163-266** — the 5-byte header read (1 byte channel + 4 bytes LE size, **193-205**), stdout/stderr drain **211-248**, exit-status (size must be 1) **249-263**. That impl reads from a raw TCP stream (`BufReader<get_raw_connection()>`); PR5 must port the framing to read from `MultiplexedSession`/`SessionReadHalf` (which yields the WRTE payload bytes). |
| **Where `ShellV2Session` wraps `MultiplexedSession`** | A new wrapper holding a `MultiplexedSession` (or its split halves), implementing the frame state machine on top of `MultiplexedSession::read` (673-742) / `Write` (744-802). Keeps `MultiplexedSession` byte-transparent (synthesis doc Q6 recommendation: wrapper, NOT decode-inside-`read()`). Constructed by an `open_shell_v2(...)` method on `PersistentUsbConnection` that calls `open_session(&ADBLocalCommand::ShellCommand(cmd, vec!["v2",...]))`. |
| **Signature/behavior changes** | `shell_exec` (346) return type can stay `(String, Option<u8>)` but now returns a real exit code. New `pub struct ShellV2Session` + `pub fn open_shell_v2(...)`. **Depends on PR1** to keep `shell_v2` in the banner (synthesis doc: add `shell_v2` to banner only when PR5 lands). |
| **error.rs additions** | None new — `ADBShellV2ParseError(String)` already exists (error.rs:12-14). |
| **lib.rs / mod.rs re-exports** | `usb/mod.rs:7-9` add `ShellV2Session`. |

---

## Cross-PR shared edit points (sequence-critical)

| Shared surface | PRs touching it | Risk / sequencing |
|---|---|---|
| **`reader_loop` demux switch (`persistent.rs:238-264`)** | #2, #3 (and #4 reads through it but adds no edit) | **Single bulk-IN owner — only one reader can exist** (spawn 84-89, IN-endpoint mutex `usb_transport.rs:310`). #2 + #3 MUST be ONE reader_loop redesign in a single PR; touching it twice risks the repeatedly-warned second-reader deadlock. `reader_loop` signature (210-214) and the clone-for-thread block (79-82) change once. |
| **`sessions` map / `SessionChannels` (`persistent.rs:41, 411-415`)** | #2 (pending_opens parallel registry), #3 (raw subscribers), #4 (rides existing map), #3/PR3 (channel sizing 31/280-281) | Field additions to `PersistentUsbConnection` (37-46) + all 3 construction sites (75-98) accrue across PRs — coordinate one struct-shape pass. |
| **Writer mutex `Arc<Mutex<USBTransport>>` (`persistent.rs:39`)** | #2 (`send_raw`, OPEN-reply OKAY), #3 (raw send), PR3 (windowed WRTE/OKAY), all write halves | Single lock; new senders just `lock().write_message`. No structural conflict but every PR adds a caller. |
| **Write halves 614-666 & 744-802 + read halves 547-612 & 673-741** | PR3 (rewrites all four), #4 (SYNC rides on them), #5 (shell-v2 wraps `MultiplexedSession` read/write) | **PR3 must land before #4 and #5 build on the final blocking-vs-windowed API shape** — synthesis doc: if #4/#5 are built on stop-and-wait first, budget a rewrite. Note the read/write logic is DUPLICATED between `MultiplexedSession` (673-802) and the split halves (547-666) — every PR3 change must be applied to **both copies**. |
| **Banner line 127 + `do_connect` (`persistent.rs:127-134`)** | PR1 (rewrite), and #1/#4/#5 gate which features are honest | PR1 (#6) is the **gate/foundation**: it must land first; #1 re-adds `delayed_ack`, #5 re-adds `shell_v2`, #4 keeps/drops `sendrecv_v2*`. One edit at 127 neutralizes the retroactive-lie risk for #1/#4/#5/#6. |
| **`ADBTransportMessage` (`adb_transport_message.rs`)** | #3 (needs `Clone` for tee, currently not derived, 12-16), PR3 (optional payload-i32 helper) | Small support-file edit; if `#[derive(Clone)]` is added it's shared by #3's tee. |
| **`error.rs` enum** | #2 (overflow), PR3 (malformed OKAY), #4 (SYNC FAIL) | Append-only arms; low conflict but multiple PRs append. `ADBShellV2ParseError`, `UsbTimeout`, `SendError` already exist. |
| **`usb/mod.rs:7-9` re-exports** | #1 (`DeviceFeatureSet`), #2 (`IncomingOpen`/raw types), #4 (`MultiplexedSyncSession`), #5 (`ShellV2Session`) | One `pub use persistent::{...}` line grows per PR. `lib.rs:36` `pub use message_devices::*` auto-propagates — no `lib.rs` edit needed unless a type lives outside `message_devices`. |

**Recommended sequence (matches `04-synthesis-sequencing.md`):** PR1 (#6 banner/foundation) → PR2 (#2+#3 single reader_loop redesign) ‖ PR3 (#1 windowing, core write/read semantics) → then PR4 (#4 SYNC, rides PR3) and PR5 (#5 shell-v2, wraps `MultiplexedSession`, re-adds `shell_v2` to banner).

## Caveats / not found

- No external/in-tree callers of the persistent API exist yet, so public-signature churn is currently free — verified by repo-wide grep (only the unrelated generic `ADBMessageDevice::open_session` matched, plus a `_material/` example doc).
- `ADBTransportMessage` is NOT `Clone` today; PR3's read-half "buffer the WRTE payload" path already moves the payload via `into_payload()` (586/716), but #3's raw tee needs a clone — flag for design.
- The read/write byte-stream logic is **duplicated** between `MultiplexedSession` (673-802) and the split `SessionReadHalf`/`SessionWriteHalf` (547-666). Any PR3/PR5 change to flow control or framing must be mirrored in both, or the duplication consolidated first.
- The 5-byte shell-v2 framing reference (`adb_server_device_commands.rs:163-266`) is written against a TCP `RawConnection`, not USB messages — PR5 ports the framing logic, it cannot reuse the function.
- Exact new method names (`new_with_features`, `device_features`, `incoming_opens`, `subscribe_raw`, `send_raw`, `open_sync_session`, `open_shell_v2`) are taken from the task brief / synthesis doc; the brainstorm design forks (overflow policy, blocking-vs-windowed write API, callback-vs-poll for OPEN) are still open per `04-synthesis-sequencing.md` §"待 brainstorm 敲定" and will refine these signatures.
