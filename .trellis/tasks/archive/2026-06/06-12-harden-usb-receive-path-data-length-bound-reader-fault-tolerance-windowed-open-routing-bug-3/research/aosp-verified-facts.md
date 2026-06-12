# AOSP-verified facts (from bug3-windowed-open-rootcause + magic-only-decision-audit workflows)

All verified against `packages/modules/adb` source.

## Windowed OPEN handshake / rejection
- adbd accepts windowed OPEN → `send_ready(s->id, s->peer->id, t, INITIAL_DELAYED_ACK_BYTES)`:
  `A_OKAY(arg0=device_id, arg1=host_local_id, payload=int32 LE 32MiB)` — payload-bearing
  only when `SupportsDelayedAck()`.
- adbd rejects OPEN (e.g. `create_local_service_socket` fails) → `send_close(0, p->msg.arg0, t)`
  = `A_CLSE(arg0=0, arg1=host_local_id)`. **arg0 is 0**, arg1 is the OPEN's arg0 (our local_id).
  → recognize rejection by `command==Clse` routed to the session (do NOT require nonzero arg0).

## USB framing (refutes report hypothesis 2e)
- adbd sends the 24-byte apacket header and the payload as SEPARATE bulk writes
  (`daemon/usb.cpp` UsbFfsConnection::Write: header in its own Block/IoWriteBlock, payload as
  separate IoWriteBlock(s); each submitted as its own aio op).
- 24-byte header < 512 max-packet → terminates its transfer as a short packet. Host receives
  header and payload as DISTINCT completions. So `read_exact` reading 24 then `data_length`
  matches the wire format; the `received.len() > remaining` discard branch is never hit by
  spec-compliant adbd. (Still worth a defensive error — issue C2.)
- Receiver framing is data_length-driven, not ZLP-driven: read exactly 24, then exactly
  `data_length`.

## Integrity (confirms magic-only decision)
- `transport.cpp::check_header` validates ONLY: (1) `magic == command ^ 0xffffffff`,
  (2) `data_length <= max_payload`. It never touches `data_check`. No `check_data` in modern tree.
- `data_check` ("crc32") is a vestigial additive byte-sum; never validated on receive in any
  version; sent as 0 at `>= A_VERSION_SKIP_CHECKSUM`.
- → magic-only receive integrity is AOSP-faithful. The missing piece adboost still lacks is
  check_header's clause (2): `data_length <= MAX_PAYLOAD` (issue A).

## adboost code anchors (working tree @ start of task)
- `usb_transport.rs:359-384` read path; `:366` unbounded alloc; `:387-422` read_exact; `:417` discard.
- `tcp_transport.rs:168-205` read path; `:184` unbounded alloc.
- `persistent.rs:181-203` classify_message (CLSE→SessionData); `:874-881` open_session ack_rx-only wait;
  `:647-650` reader hard break on any ReadError.
- `flow_control.rs:26` `pub const MAX_PAYLOAD: usize = 1024 * 1024;`
