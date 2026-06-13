# Persistence & External State

> **There is no database in this project.** `xp_adb_client` is an ADB protocol
> client library — it talks to Android devices over USB / TCP, not to any
> datastore. This file replaces the generic "database guidelines" template
> with the persistence/external-state conventions that *do* apply here.

---

## No database, no ORM, no migrations

The project has no SQL/NoSQL store, no ORM, and no migrations. Do not introduce
one to solve a problem that file-based or in-memory state can handle. If a
genuine persistence need arises, raise it as a design discussion first.

---

## The persistent state that does exist

### ADB RSA key (on-disk, read-only by convention)

ADB device authentication uses an RSA keypair read from the filesystem:

- `ADBRsaKey`, `read_adb_private_key` — `message_devices/models/adb_rsa_key.rs`.
- Default key path resolved via `utils::get_default_adb_key_path`.
- If no key is found, a **random** key is generated and a `warn!` is logged
  (`message_devices/usb/persistent.rs:61`) — see `logging-guidelines.md`.
- Never log or persist private key bytes (see `logging-guidelines.md` →
  "What NOT to Log").

### In-memory session multiplexing (USB)

The closest thing to a "connection pool" is the persistent USB session layer:

- `PersistentUsbConnection`, `MultiplexedSession`, `SessionChannels` —
  `message_devices/usb/persistent.rs`.
- One CNXN+AUTH'd USB connection is multiplexed across logical sessions, with a
  background reader thread routing messages.
- Shared state is guarded by `Mutex`. **Lock-handling rule:** new lock sites
  must propagate `RustADBError::PoisonError` via `?` (see `error-handling.md` →
  "Common Mistakes"). The original 9 `lock().unwrap()` sites were eliminated
  during the server-capabilities work (count is now 0 in `persistent.rs`) — do
  not reintroduce them.

### USB transport concurrency (nusb endpoint-split model)

The USB transport (`message_devices/usb/usb_transport.rs`) runs on **`nusb`**,
whose `Endpoint<EpType, Dir>` is `&mut self`-exclusive and **not `Clone`**. The
IN and OUT endpoints are two independent objects. To preserve the reader-thread
(IN) + writer (OUT) concurrency model — and because `ADBMessageTransport` is
bounded `: ADBTransport + Clone + Send + 'static` so `USBTransport` **must stay
`Clone`** (the non-persistent `ADBMessageDevice<USBTransport>` path clones the
transport to share it across its own reader/writer) — the IN and OUT endpoints
live behind **two separate `Arc<Mutex<…>>` locks**, not one shared lock.

> **Why two locks, not one:** the reader's blocking 1s read holds only the IN
> lock; the writer holds only the OUT lock. No code path acquires both, so the
> reader can never stall the writer and there is no lock-ordering deadlock. A
> single shared `Arc<Mutex<USBTransport>>` would let a long blocking read starve
> writes — do not collapse the two locks back into one.

### Single bulk-IN reader (hard architectural constraint)

There is exactly **one** reader thread (`usb-reader`, spawned in
`PersistentUsbConnection::new_with_features`) that owns the USB bulk-IN
endpoint. **Never spawn a second reader of the IN endpoint** — `nusb`'s blocking
`transfer_blocking` holds the IN lock for its whole duration, so two readers
would contend/deadlock and steal each other's frames.

Consequences for any feature that needs inbound messages:

- **All inbound demux happens inside the one `reader_loop`.** The routing
  decision is factored into the pure, I/O-free `classify_message(&msg,
  &known_sessions) -> RouteDecision` (`SessionAck` / `SessionData` /
  `DeviceOpen` / `Unknown`) so it is unit-testable without USB.
- **Device-originated OPEN** (`A_OPEN(device_local_id, arg1=0, dest)`, target
  local_id not in the sessions map) routes to a bounded `pending_opens` channel,
  consumed via `incoming_opens()` (pull model). Reverse policy lives in the
  caller (xdb), not the crate.
- **Raw taps** (`subscribe_raw(filter)` + `send_raw`) are teed *inside*
  reader_loop alongside normal session dispatch — not via a second reader.
- **Overflow must be observable, never silent.** Reader-side channel sends use
  `try_send` (the reader must never block, or it stalls *all* sessions); on
  `Full`, `log::warn!` with the session id + command. Do **not** restore the old
  silent `let _ = …try_send` drops.
- **Control signals must NOT ride a droppable queue.** The per-session `data_tx`
  / `ack_tx` are bounded and CAN drop a frame on overflow (the never-block rule
  above). Two signals must survive that drop, so they live in shared atomics the
  reader updates directly, NOT (only) as messages on those queues:
  - **CLSE (close):** the reader sets the session's shared `closed: Arc<AtomicBool>`
    the instant it classifies a CLSE — regardless of whether the CLSE message also
    fits on `data_tx`. The read half reports EOF from this flag, so a dropped CLSE
    cannot hang a reader. `poll_read` still delivers any already-queued WRTEs
    BEFORE honoring the flag (drain-then-EOF), so close never abandons buffered
    data. Losing a CLSE was also a feeder of the stale-CLSE pollution loop (see the
    graceful-teardown section in `adb-wire-protocol-contract.md`).
  - **OKAY window credit:** the reader parses each OKAY's signed delta
    (`parse_okay_delta`) and `fetch_add`s it into the session's shared
    `recv_credit: Arc<AtomicI64>` — the SINGLE source of send-window credit. The
    OKAY message on `ack_tx` is then only a wakeup *poke*; its payload is NOT
    re-read for credit (that would double-count). The write half drains the atomic
    (`apply_delta(recv_credit.swap(0))`) in `poll_write` step 2 and again before
    parking in step 3, so a poke dropped on a full `ack_tx` never loses credit and
    never deadlocks a parked writer (a poke only drops when the queue is non-empty,
    i.e. another poke is pending to wake on). Only a dropped DATA (WRTE) frame is a
    real, warned loss — the acknowledged never-block ∧ bounded-memory tradeoff.

### Delayed-ack flow-control contract (`flow_control.rs`)

`delayed_ack` windowed flow control replaces stop-and-wait on the persistent
connection. The wire semantics are verified against AOSP
`platform/packages/modules/adb` (see the task research
`07-aosp-delayed-ack-wire-semantics.md`) — **these exact values are
load-bearing; do not "simplify" them from memory:**

- **The OKAY byte count lives in the OKAY _payload_** as a 4-byte
  **little-endian signed `i32`** — NOT in `arg0`/`arg1` (those remain the
  local/remote socket IDs). A **classic-mode** (no delayed_ack) OKAY has an
  **empty** payload.
- **Semantics are DELTA, not cumulative:** the receiver sends the
  just-flushed byte count; the sender does `available_bytes += delta`. The
  delta is **signed and may be negative** — accumulate in `i64` internally and
  `saturating_add`/`saturating_sub` so a negative or large delta cannot
  overflow/panic. Parse/emit the wire value as `i32` LE.
- **Initial window = 32 MiB** (`INITIAL_DELAYED_ACK_BYTES = 32 * 1024 * 1024`),
  granted per-stream. Per-WRTE chunk stays clamped to `MAX_PAYLOAD = 1 MiB` —
  the in-flight window is decoupled from the per-packet cap.
- **Opener rule (we are the opener in `open_session`):** put 32 MiB in the OPEN
  `arg1` (our receive grant) and initialize our own **send window to 0** (NOT
  32 MiB), then apply the device's first OKAY payload to credit it before
  sending data. Starting the send window at 32 MiB would overrun a device that
  granted less. The initial ready-OKAY carries the 32 MiB i32-LE grant in
  windowed mode, empty in classic mode.
- **Feature gate:** windowed mode activates only when **both** banners advertise
  `delayed_ack` (ours via `DeviceFeatureSet`, the device's parsed from its CNXN
  banner with a whole-token `features=` match). Otherwise fall back to classic
  stop-and-wait.
- **Overflow = backpressure, never close.** When the window is exhausted, the
  blocking `Write` impl blocks on the ack channel until an OKAY credits it; it
  does not close the stream. Malformed OKAY payloads (length ∉ {0, 4}) are
  logged and ignored, not fatal.

> **Keep `MultiplexedSession` byte-transparent.** Higher-level framings
> (`SyncSession` for SYNC v1, `ShellV2Session` for shell-v2 inner frames) layer
> *on top of* `MultiplexedSession`'s `Read`/`Write` — never push protocol
> decoding *into* `MultiplexedSession`. The windowed read/write policy itself
> lives in **shared free functions** (`read_with_ack`, `windowed_write`,
> `drain_acks`/`apply_ack`) called by both `MultiplexedSession` and the split
> `Session{Read,Write}Half` — do not copy-paste the window logic into each.

### Sans-io testing pattern for protocol logic

Protocol state machines in the persistent layer are written **I/O-agnostic** so
they unit-test without USB hardware (there is no `tests/` dir — use inline
`#[cfg(test)] mod tests`). Established examples to follow:

- `FlowControl` (window accounting): test initial window, delta apply, negative
  delta, exhaustion + recovery, classic empty-payload no-op, i32 LE round-trip
  incl. `MIN`/`MAX`, malformed payload, overflow saturation.
- `classify_message` (reader routing): feed synthetic `ADBTransportMessage`s and
  assert each lands in the right `RouteDecision`.
- Frame codecs (`SyncSession` SYNC v1 header, `ShellV2Session`
  `[id:u8][len:u32 LE]`): assert header encode/decode, the 65536 DATA-chunk
  boundary, channel routing, and **frame split across reads** (a `ChunkedReader`
  yielding 1 byte per `read()` proves reassembly).

When adding new wire logic, isolate the pure codec/state-machine from the
transport and ship its inline sans-io test in the same file.

---

## State conventions

- Prefer **passing state explicitly** through the device/transport types over
  global mutable statics. The only statics are `LazyLock<Regex>` for parsing
  (compile-time-constant patterns).
- Transport/device structs own their connection state; commands operate on
  `&mut self` or `&self` of the owning device.

---

## Common Mistakes

- Reaching for a database/ORM where the protocol model needs none.
- Adding a global mutable static instead of threading state through the device
  type.
- Copying the `lock().unwrap()` pattern from `persistent.rs` — propagate
  `PoisonError` instead.
- Spawning a second reader of the USB bulk-IN endpoint — there must be exactly
  one; tee/route inside the single `reader_loop`.
- Reading the delayed-ack byte count from `arg0`/`arg1` instead of the OKAY
  payload, or treating it as cumulative instead of a signed delta.
- Initializing the opener's send window to 32 MiB instead of 0 (overruns a
  device that grants less).
- Pushing SYNC / shell-v2 frame decoding into `MultiplexedSession` instead of
  layering it on top, or duplicating the windowed read/write policy across
  `MultiplexedSession` and the split halves instead of using the shared
  `read_with_ack` / `windowed_write` helpers.
- Letting a reader-side channel `try_send` drop silently — log on `Full`.
- Carrying a CLSE or an OKAY window credit ONLY as a message on the bounded
  `data_tx`/`ack_tx` queue — both must be reflected in the shared `closed` /
  `recv_credit` atomics so a full-queue drop cannot lose a close or flow-control
  credit (see "Control signals must NOT ride a droppable queue" above).
- Re-reading the OKAY payload for credit in `apply_ack`/`poll_write` after the
  reader has already banked it into `recv_credit` — that double-counts the window.
