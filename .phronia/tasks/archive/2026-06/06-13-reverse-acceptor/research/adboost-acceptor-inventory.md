# Research: adboost Acceptor Inventory (device-initiated A_OPEN → MultiplexedSession)

- **Query**: Inventory existing opener (`open_session`) machinery so an "acceptor" path (`accept_device_open`) can be built that reuses everything except sending OPEN.
- **Scope**: internal
- **Date**: 2026-06-13

All line numbers below are for `adb_client/src/message_devices/usb/persistent.rs` unless another file is named.

## Files

| File Path | Role |
|---|---|
| `adb_client/src/message_devices/usb/persistent.rs` | `PersistentUsbConnection`, reader/writer tasks, `open_session`, `incoming_opens`, all session structs, `ReaderControl`, `WriterHandle`, `classify_message` |
| `adb_client/src/message_devices/usb/flow_control.rs` | `FlowControl` state machine, `encode_okay_payload`, `INITIAL_DELAYED_ACK_BYTES`, `MAX_PAYLOAD` (re-export) |
| `adb_client/src/message_devices/adb_transport_message.rs` | `ADBTransportMessage` + `ADBTransportMessageHeader`, `try_new`, accessors, `MAX_PAYLOAD` source |
| `adb_client/src/message_devices/message_commands.rs` | `MessageCommand` enum |

---

## 1. `open_session` walkthrough (lines 928–1066) — the OPENER

`pub async fn open_session(&self, cmd: &ADBLocalCommand) -> Result<MultiplexedSession>` (signature at **929**). Step by step:

1. **local_id generation** (930–934): `let local_id: u32 = { let mut rng = rand::rng(); rng.random() };` then `tracing::Span::current().record("local_id", local_id);`. Random `u32`.
2. **Create per-session channels** (940–941): `let (data_tx, mut data_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);` and `let (ack_tx, mut ack_rx) = mpsc::channel(SESSION_CHANNEL_SIZE);`. `SESSION_CHANNEL_SIZE = 64` (line 66). `data_rx`/`ack_rx` kept `mut` because they're borrowed during the handshake then moved into the returned session.
3. **Register BEFORE sending OPEN** (947–953): `self.control_tx.send(ReaderControl::Register(local_id, SessionChannels { data_tx, ack_tx })).await.map_err(|_| RustADBError::SendError)?;`. Registration is applied by the reader's `select!` loop before any frame for `local_id` can be routed (see `read_or_control`, 811–815).
4. **Build OPEN payload** (955–959): service string from `cmd.to_string()`, NUL-terminated if not already.
5. **OPEN arg1 = receive-window grant** (960–966): `open_arg1 = if self.delayed_ack_negotiated { INITIAL_DELAYED_ACK_BYTES as u32 } else { 0 }`. This is the OPENER granting ITS OWN receive window in OPEN arg1.
6. **Send OPEN** (974–983): `ADBTransportMessage::try_new(MessageCommand::Open, local_id /*arg0*/, open_arg1 /*arg1*/, &service_bytes)`, sent via `self.writer.send_with_ack(open_msg).await`. On failure → `unregister_session(local_id)` + `Err(SendError)`.
7. **Await first response racing ack_rx/data_rx** (985–995): `await_open_response(&mut ack_rx, &mut data_rx).await` (defined 221–249). Returns the OKAY from `ack_rx`, or fails fast on an early CLSE routed to `data_rx`. Timeout `OPEN_RESPONSE_TIMEOUT = 10s` (line 206). On error → unregister + return.
8. **Defensive command check** (1000–1006): ensures `response.header().command() == Okay`.
9. **Extract remote_id** (1008): `let remote_id = response.header().arg0();` — the device's local id comes from the OKAY's **arg0**.
10. **Build send_flow** (1016–1021): `let mut send_flow = if self.delayed_ack_negotiated { FlowControl::new_windowed(0) } else { FlowControl::new_classic() };` then **seed from the OKAY payload** (1021): `send_flow.on_okay_payload(response.payload());`. Opener starts its own send window at **0** and the device's OKAY credits it.
11. **Send the ready OKAY** (1023–1039): `let ready_payload = encode_okay_payload(self.delayed_ack_negotiated, INITIAL_DELAYED_ACK_BYTES as usize);` then `ADBTransportMessage::try_new(MessageCommand::Okay, local_id /*arg0*/, remote_id /*arg1*/, &ready_payload)` sent via `self.writer.try_send_fire_forget(ready_msg)`. This is the initial OKAY granting our receive window; adbd won't send WRTE until it arrives.
12. **Drain already-buffered ack-channel window deltas** (1041–1045): `while let Ok(extra) = ack_rx.try_recv() { send_flow.on_okay_payload(extra.payload()); }`.
13. **Build SessionInner** (1047–1055): `close_state = Arc::new(AtomicBool::new(false));` then struct literal `SessionInner { local_id, remote_id, writer: self.writer.clone(), control_tx: self.control_tx.clone(), closed: close_state, windowed: self.delayed_ack_negotiated }`.
14. **Build MultiplexedSession** (1057–1065): struct literal `MultiplexedSession { shared: Arc::new(inner), data_rx, ack_rx, read_buf: Vec::new(), read_pos: 0, send_flow, write_state: WriteState::Idle }`.

Helper `unregister_session(&self, local_id)` (912–917): fire-and-forget `control_tx.send(ReaderControl::Unregister(local_id)).await`.

---

## 2. `incoming_opens` (469–475) — how device-initiated OPENs surface

- **Signature**: `pub fn incoming_opens(&mut self) -> Result<mpsc::Receiver<ADBTransportMessage>>` (469). Takes **`&mut self`**.
- **Body**: `self.pending_opens_rx.take().ok_or_else(|| RustADBError::ADBRequestFailed("incoming_opens: receiver already taken (single consumer only)"))`.
- **Single-consumer constraint**: `pending_opens_rx: Option<mpsc::Receiver<...>>` field (310). `take()` empties the `Option`, so a second call errors. Only ONE consumer of the OPEN queue exists for the connection's lifetime.
- **Channel creation**: `let (pending_opens_tx, pending_opens_rx) = mpsc::channel(PENDING_OPENS_CHANNEL_SIZE);` (370), `PENDING_OPENS_CHANNEL_SIZE = 64` (69). `pending_opens_tx` is moved into the reader task (382), `pending_opens_rx` stored in the struct (396).
- **Reader routing** (`reader_loop`, 779–787): `RouteDecision::DeviceOpen => { if pending_opens_tx.try_send(msg).is_err() { warn! } }`. Never blocks; drops on overflow.
- **`classify_message` → DeviceOpen** (181–203): `target_id = msg.header().arg1()` (185). If `target_id` is NOT in `known_sessions` AND `command == MessageCommand::Open` → `RouteDecision::DeviceOpen` (193–199).
- **OPEN message field shape** (documented at 194 and asserted in test `device_originated_open_routes_to_pending_opens`, 1832–1842): device OPEN is `A_OPEN(device_local_id, 0, "<dest>")` →
  - **arg0 = device's local_id** (the remote socket id the acceptor must use).
  - **arg1 = 0** (no host local id yet; this is why it's unrouted and classified as DeviceOpen).
  - **payload = destination string**, NUL-terminated (test uses `b"tcp:1234\0"`).
- Doc comment (450–468) confirms the intended acceptor flow: reply `OKAY(device_local_id, host_local_id)` + register a session, or reject with `CLSE(0, device_local_id)`. NOTE the doc's OKAY arg ordering wording — see §7 for the exact arg semantics to use.

---

## 3. `ReaderControl` enum (111–115)

```rust
enum ReaderControl {
    Register(u32, SessionChannels),
    Unregister(u32),
    Subscribe(RawSubscriber),
}
```

- **`Register(local_id, SessionChannels)`**: inserts into the reader's private `sessions: HashMap<u32, SessionChannels>` keyed by local_id (applied at 812–814 in `read_or_control`).
- **`Unregister(u32)`**: removes the id (816–818).
- **`Subscribe(RawSubscriber)`**: pushes a raw tee subscriber (820–822).
- **Can a session be registered AFTER setup?** YES. `open_session` itself registers at runtime via `self.control_tx.send(ReaderControl::Register(...)).await` (947). The reader applies control messages in the same `select!` as reads (809–832), so a post-setup register is honored before the next routed frame.
- **`control_tx` clonable / accessible from `&self`?** It is `control_tx: mpsc::Sender<ReaderControl>` (294), a field on `PersistentUsbConnection`. `mpsc::Sender` is `Clone`. It is accessed from `&self` in `open_session` (947, and cloned into `SessionInner` at 1052), `unregister_session` (914), and `subscribe_raw` (503). So a new `&self` method can both `send(Register(...))` and clone it into a new `SessionInner`.

---

## 4. Session structs — exact fields & constructibility

### `SessionChannels` (1268–1271) — **`pub`**
```rust
pub struct SessionChannels {
    pub data_tx: mpsc::Sender<ADBTransportMessage>,
    pub ack_tx: mpsc::Sender<ADBTransportMessage>,
}
```
Both fields `pub`. data_tx carries WRTE/CLSE; ack_tx carries OKAY (routing decided in `classify_message`, 187–192).

### `SessionInner` (1199–1208) — module-private struct
```rust
struct SessionInner {
    local_id: u32,
    remote_id: u32,
    writer: WriterHandle,
    control_tx: mpsc::Sender<ReaderControl>,
    closed: Arc<AtomicBool>,
    windowed: bool,
}
```
No constructor — struct-literal only. It is defined in this module so any new method in the same `impl`/module can build it directly (exactly as `open_session` does at 1048–1055). Helpers: `is_closed()` (1211), `mark_closed()` (1215). `Drop` (1220–1245) sends a best-effort CLSE `try_new(Clse, local_id, remote_id, &[])` and `Unregister(local_id)` unless already closed.

### `MultiplexedSession` (1253–1265) — **`pub`** struct, private fields
```rust
pub struct MultiplexedSession {
    shared: Arc<SessionInner>,
    data_rx: mpsc::Receiver<ADBTransportMessage>,
    ack_rx: mpsc::Receiver<ADBTransportMessage>,
    read_buf: Vec<u8>,
    read_pos: usize,
    send_flow: FlowControl,
    write_state: WriteState,
}
```
- **No public constructor.** Built by struct literal inside `open_session` (1057–1065). The only other constructor is `#[cfg(test)] fn new_for_test(...)` (1279–1313). A new acceptor method in the SAME module/file can construct it via struct literal because all fields are in scope there.
- Required values to construct: `shared: Arc<SessionInner>`, `data_rx`, `ack_rx` (the receiver halves of the two registered channels), `read_buf: Vec::new()`, `read_pos: 0`, `send_flow: FlowControl`, `write_state: WriteState::Idle`.
- `WriteState` (1404–1410): `enum { Idle, Sending { ack, chunk_size } }` — start at `Idle`.
- Accessors: `local_id()` (1319), `remote_id()` (1325), `close()` (1331), `into_split()` (1351). AsyncRead/AsyncWrite impls at 1733–1775 delegate to shared `poll_read_impl`/`poll_write_impl`.

**Conclusion**: an `accept_device_open` added to `impl PersistentUsbConnection` (same file) can build `SessionInner` + `MultiplexedSession` by struct literal with zero new public API, identical to `open_session`.

---

## 5. FlowControl (flow_control.rs) — acceptor-side initial state

- `new_windowed(initial_window: i64)` (61–65): windowed mode, `available_bytes = Some(initial_window)`.
- `new_classic()` (70–74): `available_bytes = None`, strict stop-and-wait.
- `on_okay_payload(&mut self, payload: &[u8]) -> bool` (127–142): empty payload → no-op `true`; 4-byte LE i32 delta accumulated into the window → `true`; any other length → `false` (ignored).
- `encode_okay_payload(windowed: bool, bytes: usize) -> Vec<u8>` (162–169): windowed → `bytes as i32 LE` (4 bytes); classic → empty `Vec`.
- `INITIAL_DELAYED_ACK_BYTES: i64 = 32 * 1024 * 1024` (35) — 32 MiB.
- `MAX_PAYLOAD: usize = 1024 * 1024` (re-exported at flow_control.rs:31 from adb_transport_message.rs:19) — 1 MiB per WRTE chunk.
- Other methods: `is_windowed()` (78), `available_bytes()` (84), `can_send()` (96), `record_sent(n)` (111), `apply_delta(delta)` (146).

### Correct ACCEPTOR send-window seed
The device is the OPENER, we are the ACCEPTOR. Wire semantics (flow_control.rs module doc, 8–22, esp. 16–17): "the opener puts [its receive window] in OPEN arg1 and sets its OWN send window to 0; the responder grants its own window via the first OKAY payload."

So for the acceptor:
- **Our send window** (bytes WE may write to the device) is credited by the device's subsequent OKAY payloads. AOSP responder symmetry means our send window also starts at **0** and is credited by the device's OKAYs (the device's OKAYs carry i32 deltas). So: `if windowed { FlowControl::new_windowed(0) } else { FlowControl::new_classic() }`, identical to `open_session` line 1016–1020.
- **Important difference from opener**: there is no "OPEN response OKAY" to seed from at accept time. The device's OPEN payload is the destination string, NOT a window grant for us. The window credit is in OPEN **arg1** but that is the DEVICE's receive-window grant (how much WE may send), i.e. our send window's initial credit. The opener path seeds its send window from the *device's first OKAY payload*; for the acceptor the equivalent credit arrives as the device's later OKAYs on `ack_rx`. Practically: start `new_windowed(0)` and let `on_okay_payload` raise it as OKAYs arrive (same `poll_write_impl` draining at 1604–1647 handles this). The device OPEN's `arg1` MAY also be consumed as an initial send-window grant — confirm desired behavior with implementer; AOSP credits the responder's send window from the OPEN's arg1 when delayed_ack is on.
- **Our receive window grant** to the device is sent in our reply OKAY payload via `encode_okay_payload(windowed, INITIAL_DELAYED_ACK_BYTES as usize)` — exactly the `ready_payload` at open_session 1027–1030.

---

## 6. The writer — sending a raw frame from `&self`

`WriterHandle` (119–154), `#[derive(Clone)]` (119):
- `try_send_fire_forget(&self, msg) -> io::Result<()>` (128–139): non-blocking enqueue of `OutboundFrame::FireForget`. Used for OKAY/CLSE/OPEN/raw. This is what the reply-OKAY should use (mirrors open_session 1037–1039).
- `async fn send_with_ack(&self, msg) -> io::Result<()>` (144–153): enqueues `OutboundFrame::WithAck` with a oneshot and awaits the actual write result. Used for WRTE and for OPEN in open_session (980).
- The connection holds `writer: WriterHandle` field (292), accessible from `&self`. `WriterHandle` is `Clone`, so `self.writer.clone()` goes into `SessionInner.writer` (open_session 1051). A new acceptor method uses `self.writer.try_send_fire_forget(reply_okay)` for the reply and `self.writer.clone()` for `SessionInner`.
- There is also the public `send_raw(&self, msg) -> Result<()>` (525–530) which wraps `send_with_ack`, but for the OKAY reply `try_send_fire_forget` matches the opener's pattern.

---

## 7. Is there an existing "register without OPEN" method? — NO. Minimal new method shape

**No existing method registers a session for an arbitrary `(local_id, remote_id)` without first sending an OPEN.** `open_session` is the only path that registers + builds a `MultiplexedSession`, and it always sends OPEN (974–983) and awaits the device's OKAY (989). `subscribe_raw`/`send_raw` are raw primitives that do NOT register a session or build a `MultiplexedSession`.

### Minimal new method: `accept_device_open`

Proposed: `pub async fn accept_device_open(&self, open_msg: ADBTransportMessage) -> Result<MultiplexedSession>` added to `impl PersistentUsbConnection` (same file, so all private structs are in scope). Mirror `open_session` but:

1. **Extract remote_id from the incoming OPEN's arg0**: `let remote_id = open_msg.header().arg0();` (the device's local id; see §2 — device sends `A_OPEN(device_local_id, 0, dest)`). Optionally read the destination string from `open_msg.payload()` for policy.
2. **Generate our local_id** exactly as 930–934 (`rand::rng().random::<u32>()`), and record on span if desired.
3. **Create channels** (data_tx/data_rx, ack_tx/ack_rx) as 940–941.
4. **Register BEFORE replying** via `self.control_tx.send(ReaderControl::Register(local_id, SessionChannels { data_tx, ack_tx })).await.map_err(|_| RustADBError::SendError)?;` (as 947–953). Registering before the reply guarantees the device's subsequent WRTE/OKAY (which target `arg1 = our local_id`) route to this session, not the DeviceOpen queue.
5. **DO NOT send OPEN.** Skip 955–983 entirely. Skip `await_open_response` (989) — there is no OKAY to wait for; WE are the one replying.
6. **Build send_flow** as 1016–1020: `if self.delayed_ack_negotiated { FlowControl::new_windowed(0) } else { FlowControl::new_classic() }`. (Optionally seed from the OPEN's arg1 if treating it as the device's send-window grant — see §5; the OPEN payload is a string, NOT a window delta, so do NOT call `on_okay_payload(open_msg.payload())`.)
7. **Reply OKAY(our_local_id, remote_id)** with our receive-window grant: build `ready_payload = encode_okay_payload(self.delayed_ack_negotiated, INITIAL_DELAYED_ACK_BYTES as usize)` (open_session 1027–1030), then `ADBTransportMessage::try_new(MessageCommand::Okay, local_id /*arg0 = us*/, remote_id /*arg1 = device*/, &ready_payload)?` and `self.writer.try_send_fire_forget(ready_msg).map_err(|_| SendError)?`. On failure, `self.unregister_session(local_id).await` and return error (mirror 980–983 cleanup).

   **Arg ordering caveat**: ADB convention is `OKAY(arg0 = sender_local_id, arg1 = peer_local_id)`. The reader's `classify_message` routes by `arg1` (185), and the device routes incoming frames to its socket whose local id is in our `arg1`. So our reply must carry `arg0 = our local_id`, `arg1 = remote_id (device's local id)`. This matches open_session's ready OKAY (1031–1036: `Okay, local_id, remote_id`). Note the `incoming_opens` doc comment at 457 writes "OKAY(device_local_id, host_local_id)" which is the OPPOSITE order from `open_session`'s actual code — trust the code (arg0=our local_id, arg1=device's id), not the doc comment.

8. **Drain any already-buffered acks** (optional, mirrors 1041–1045): `while let Ok(extra) = ack_rx.try_recv() { send_flow.on_okay_payload(extra.payload()); }`.
9. **Build SessionInner** (mirror 1047–1055): `SessionInner { local_id, remote_id, writer: self.writer.clone(), control_tx: self.control_tx.clone(), closed: Arc::new(AtomicBool::new(false)), windowed: self.delayed_ack_negotiated }`.
10. **Build MultiplexedSession** (mirror 1057–1065): `MultiplexedSession { shared: Arc::new(inner), data_rx, ack_rx, read_buf: Vec::new(), read_pos: 0, send_flow, write_state: WriteState::Idle }`.

Net delta vs `open_session`: drop steps 4–8 of §1 (OPEN build/send/await-response/command-check), take `remote_id` from `open_msg.header().arg0()` instead of the OKAY's arg0, and keep everything else (register, reply OKAY, seed flow, build inner+session) identical.

---

## Supporting reference: message types & accessors

- `MessageCommand` (message_commands.rs 12–27): `Open = 0x4E45_504F`, `Okay = 0x5941_4B4F`, `Clse = 0x4553_4C43`, `Write = 0x4554_5257`, `Cnxn`, `Auth`, `Stls`. `Display` impl 97–109.
- `ADBTransportMessage::try_new(command, arg0, arg1, data) -> Result<Self>` (adb_transport_message.rs 142–147). Computes header (crc32 = byte-sum, magic = `command ^ 0xFFFFFFFF`).
- Header accessors (adb_transport_message.rs): `command()` 62, `arg0()` 67, `arg1()` 72, `data_length()` 77. On message: `header()` 184, `payload() -> &Vec<u8>` 189, `into_payload() -> Vec<u8>` 194.
- `MAX_PAYLOAD = 1024 * 1024` (adb_transport_message.rs 19).

## Caveats / Not Found

- `persistent.rs` is 2435 lines; lines 2059–2435 are the `#[cfg(test)]` module (test helpers / integration tests using `new_for_test`). No production acceptor logic exists there — confirmed `open_session` is the only registration path by reading the full impl block (313–1161).
- The `incoming_opens` doc comment (457) and the module doc (194–199) describe the intended acceptor reply, but the doc's OKAY arg order ("OKAY(device_local_id, host_local_id)") is inconsistent with `open_session`'s actual code order (`Okay, local_id /*ours*/, remote_id /*device*/`). Implementer should follow the CODE order (arg0 = our local_id, arg1 = device's local id) and verify against an end-to-end test.
- Whether to seed the acceptor's send window from the incoming OPEN's `arg1` (the device's send-window grant to us) is a semantics decision not settled by existing code (open_session seeds only from the device OKAY payload). AOSP responder behavior credits from the OPEN arg1 when delayed_ack is on; flag for confirmation during implementation.
