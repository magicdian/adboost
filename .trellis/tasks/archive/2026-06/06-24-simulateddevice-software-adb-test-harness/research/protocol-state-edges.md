# Research: Protocol / Timing / Ordering Edges for a SimulatedDevice Test Harness

- **Query**: Enumerate every protocol/timing/ordering edge in adboost's persistent connection + flow-control + handshake code that a `SimulatedDevice` (request/response state machine above the `ADBMessageTransport` trait) could exercise deterministically.
- **Scope**: internal
- **Date**: 2026-06-24

## Files Found

| File Path | Description |
|---|---|
| `adboost/src/message_devices/usb/persistent.rs` | The reader/writer loops, `do_connect`/`do_auth`, STLS, `open_session`/`accept_device_open`, classify/route, `DeathSignal`, `is_alive`, session read/write state machines, Drop/close. ~4000 lines. |
| `adboost/src/message_devices/usb/flow_control.rs` | `FlowControl` sans-io window state machine, `encode_okay_payload`, `parse_okay_delta`, `INITIAL_DELAYED_ACK_BYTES` (32 MiB), `MAX_PAYLOAD` (1 MiB re-export). |
| `adboost/src/message_devices/adb_message_transport.rs` | The `ADBMessageTransport` trait + the **ReadTimeout idle-contract** docstring (lines 35-52): a read deadline MUST surface as `RustADBError::ReadTimeout`, never a transport-specific encoding. |
| `adboost/src/message_devices/adb_transport_message.rs` | `AUTH_TOKEN=1`, `AUTH_SIGNATURE=2`, `AUTH_RSAPUBLICKEY=3`; header decode (`TryFrom<[u8;24]>` → `ConversionError`); `payload_len_within_bound` (`data_length <= MAX_PAYLOAD`); `check_message_integrity` verifies ONLY `magic == command ^ 0xffffffff` (CRC no longer consulted, skip-checksum peers). |

## The SimulatedDevice seam

`PersistentConnection<T>` is generic over `T: ADBMessageTransport` and the transport is **moved into** the reader/writer tasks at construction (`new_with_features`, `persistent.rs:594`). The whole public surface (`open_session`, `accept_device_open`, `shell_exec`, `send_raw`, `subscribe_raw`, `incoming_opens`, `is_alive`, `wait_closed`, `shutdown`/`close`) is transport-free. The existing `ScriptedTransport` test mock (`persistent.rs:2860-2913`) already proves this seam works for `do_connect`. A `SimulatedDevice` is the generalization of `ScriptedTransport`: a `Clone` (the reader + writer halves each get a clone — they must share one script behind `Arc<Mutex<_>>` / channels) state machine answering `read_message_with_timeout` / `write_message_with_timeout`.

**Two hard constraints a SimulatedDevice must respect** (both observable in the live code):

1. The transport is **cloned** (`persistent.rs:649-650`): reader clone drives bulk-IN (`read_message_with_timeout`), writer clone drives bulk-OUT (`write_message_with_timeout`). A faithful sim must route host→device WRTE/OKAY/OPEN/CLSE (arriving via the writer clone's `write_message`) into shared state that the reader clone's `read_message` then responds to. This is the bulk-IN/bulk-OUT split that the real USB transport enforces structurally.
2. The reader uses a **1s per-frame read timeout** (`read_or_control`, `persistent.rs:1397`) and the `drain_stale` path uses a **100ms** timeout (`persistent.rs:1117`). The sim's `read_message_with_timeout` must honor the timeout by returning `RustADBError::ReadTimeout` when it has nothing to send within the deadline — otherwise the reader loop's `ReadStep::ReadTimeout => continue` idle path is never exercised and an idle test wedges.

---

## Edge Catalog

Legend — current test status: `unit` = inline `#[cfg(test)]` unit test exists; `hw` = only exercised on real hardware; `none` = untested. sim-reproducible: yes / partial / no.

### CNXN handshake (`do_connect`, persistent.rs:947-1107)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| CNXN-1 banner parse OK | Device answers CNXN write with `CNXN(arg0=version, payload=banner)` | `do_connect` returns `(arg0, banner)`; banner UTF-8-lossy decoded | `unit` (ScriptedTransport answers CNXN) | yes |
| CNXN-2 version reported from arg0 | Device CNXN `arg0` carries the protocol version | The returned `device_version` IS `response.header().arg0()` and gates `delayed_ack` | `unit` (negotiate tests use the value) | yes |
| CNXN-3 host CNXN version gating | Host opens with `A_VERSION_SKIP_CHECKSUM` iff `features.delayed_ack`, else `A_VERSION_LEGACY` (persistent.rs:969) | Host MUST NOT advertise delayed_ack at legacy version | `none` (no test asserts the *sent* CNXN version) | yes (sim can capture the host's CNXN write and assert arg0) |
| CNXN-4 max-payload field | Host CNXN `arg1 = 1_048_576` (persistent.rs:999) | Host advertises 1 MiB max payload; device's `arg1` is currently ignored on the response | `none` | yes |
| CNXN-5 malformed/truncated banner | Device CNXN with non-UTF8 / no `features=` segment / empty payload | `from_utf8_lossy` never panics; `banner_advertises_delayed_ack` → false; `DeviceFeatureSet::from_banner` → all-false | `unit` (banner_* tests, flow_control side) partial | yes |
| CNXN-6 wrong response cmd | Device answers CNXN with a cmd ∉ {CNXN,AUTH,STLS,CLSE} (e.g. WRTE/OKAY) | `do_connect` returns `WrongResponseReceived("Expected CNXN or AUTH", got)` (persistent.rs:1096) | `none` | yes |
| CNXN-7 transient write error (NotResponding) retry | Device transport returns `UsbTransferError(Unknown(0xe00002ed))` on N CNXN writes, N < `CONNECT_TRANSIENT_MAX_ATTEMPTS` (3), then succeeds | Rides out the blip, settles 100ms, completes | `unit` (`do_connect_retries_transient_notresponding_then_succeeds`) | yes |
| CNXN-8 transient write error (Disconnected/NoDevice) retry | Same as CNXN-7 but `TransferError::Disconnected` | Rides out, completes | `unit` (`do_connect_retries_transient_disconnected_then_succeeds`) | yes |
| CNXN-9 transient exceeds in-place budget | > `CONNECT_TRANSIENT_MAX_ATTEMPTS` transient errors | Fails FAST (propagates error, does NOT burn full `CNXN_MAX_ATTEMPTS`) → hands off to outer reopen | `unit` (`do_connect_fails_fast_when_transients_exceed_attempts`) | yes |
| CNXN-10 permanently-dead handle | `Disconnected` on EVERY write (forever) | Propagates `Disconnected` after the small in-place budget, NOT the CNXN-exhausted `ADBRequestFailed`, and does NOT hang (anti-amplification) | `unit` (`do_connect_transient_arm_is_small_constant_not_full_cnxn_budget`) | yes |
| CNXN-11 transient on READ (not write) | Write succeeds, READ returns transient error within budget | Same retry path as the write arm (persistent.rs:1028-1038) | `none` (existing mock only fails on write) | yes (sim returns transient from `read_message`) |
| CNXN-12 non-transient error fails fast | Write/read returns e.g. `Stall` or `WrongResponseReceived` | NOT retried in-place (`is_transient_connect_error` false); propagated immediately | `unit` (`is_transient_connect_error_classifies_family`) for the predicate; end-to-end `none` | yes |
| CNXN-13 retry-budget separation invariant | `CONNECT_TRANSIENT_MAX_ATTEMPTS (3) < CNXN_MAX_ATTEMPTS (8)` | Compile-time `assert!` (persistent.rs:132). The two budgets are independent counters in the same loop | `unit` (compile-time const assert) | n/a (compile-time) |

### Stale-CLSE drain (`do_connect` CLSE arm + `drain_stale`, persistent.rs:1087-1130)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| DRAIN-1 single stale CLSE then CNXN | Device answers first CNXN read with `CLSE`, then on retry a real `CNXN` | Loop drains, settles 100ms, retries, succeeds within `CNXN_MAX_ATTEMPTS` | `none` (ScriptedTransport never sends CLSE) | yes |
| DRAIN-2 burst of stale frames | Several buffered CLSE/WRTE frames queued before the real CNXN | `drain_stale` reads up to `STALE_DRAIN_MAX_FRAMES` (64) with 100ms timeout until pipe quiet, in ONE pass | `none` | yes |
| DRAIN-3 stale-drain cap | Device emits > 64 frames continuously | Drain stops at the cap and proceeds (cannot wedge forever) | `none` | yes (sim emits 65+ frames) |
| DRAIN-4 pre-handshake drain | `do_connect` ALWAYS calls `drain_stale` before the first CNXN write (persistent.rs:956) | A timeout (pipe quiet, ReadTimeout/transient) returns from drain immediately | `none` | yes |
| DRAIN-5 CNXN exhausted by stale CLSEs | Device sends CLSE on all `CNXN_MAX_ATTEMPTS` reads | Returns `ADBRequestFailed("CNXN failed after 8 attempts ...")` | `none` | yes |

### AUTH flow (`do_auth`, persistent.rs:1135-1174)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| AUTH-1 TOKEN→SIGNATURE→CNXN | Device CNXN-read returns `AUTH(arg0=AUTH_TOKEN=1, payload=token)`; host signs and writes `AUTH(SIGNATURE=2)`; device replies `CNXN` | `do_auth` returns `(arg0, banner)` after signature accepted | `none` | yes (sim verifies the signature against a known pubkey, or just accepts) |
| AUTH-2 TOKEN→SIGNATURE→AUTH→RSAPUBLICKEY→CNXN | After SIGNATURE, device replies non-CNXN (token re-challenge); host sends `AUTH(RSAPUBLICKEY=3, payload=pubkey+\0)`; device replies `CNXN` | Public-key path completes; final read uses a **10s** timeout (persistent.rs:1168); `assert_command(Cnxn)` | `none` | yes |
| AUTH-3 AUTH type != TOKEN | First AUTH message has `arg0 != AUTH_TOKEN` | `do_auth` returns `ADBRequestFailed("AUTH message with type != TOKEN ...")` (persistent.rs:1140) | `none` | yes |
| AUTH-4 unauthorized device (never accepts) | Device keeps replying AUTH/non-CNXN after RSAPUBLICKEY | Final `read_message_with_timeout(10s)` then `assert_command(Cnxn)` fails → error propagates; the 10s deadline bounds the hang | `none` | partial (sim can stall; 10s real wait unless test uses `start_paused`) |
| AUTH-5 signature payload is the token | Host signs `message.into_payload()` (the token bytes) | The SIGNATURE message's payload is `private_key.sign(token)` | `none` | yes (sim can verify) |
| AUTH-6 pubkey NUL-terminated | RSAPUBLICKEY payload ends with `\0` (persistent.rs:1162) | Trailing NUL appended | `none` | yes |

### delayed_ack negotiation (`negotiate_delayed_ack` + `banner_advertises_delayed_ack`, persistent.rs:464-491)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| DACK-1 both ends + version → enabled | local advertises, banner has `delayed_ack`, version >= `A_VERSION_SKIP_CHECKSUM` | windowed enabled | `unit` (`negotiate_delayed_ack_android16_capable_is_enabled`) | yes |
| DACK-2 legacy version → disabled | banner advertises but version == `A_VERSION_LEGACY` | MUST stay false (windowed OPEN at legacy → adbd ignores → open_session times out) | `unit` (`negotiate_delayed_ack_legacy_version_is_disabled`) | yes |
| DACK-3 banner lacks feature → disabled | capable version but no `delayed_ack` in banner | false | `unit` (`negotiate_delayed_ack_no_banner_feature_is_disabled`) | yes |
| DACK-4 local opt-out → disabled | `local_delayed_ack=false` | false | `unit` (`negotiate_delayed_ack_local_opt_out_is_disabled`) | yes |
| DACK-5 above-threshold version | version > `A_VERSION_SKIP_CHECKSUM` | enabled | `unit` (`negotiate_delayed_ack_above_threshold_is_enabled`) | yes |
| DACK-6 substring false-match guard | banner has `delayed_ack_extended` only | NOT detected (whole-token compare) | `unit` (`banner_substring_does_not_false_match`) | yes |
| DACK-7 end-to-end OPEN arg1 grant | delayed_ack negotiated → OPEN `arg1 = INITIAL_DELAYED_ACK_BYTES` (32 MiB); else `arg1 = 0` (persistent.rs:1563) | Windowed OPEN carries the receive-window grant; classic OPEN carries 0 | `none` (only the negotiate predicate is unit-tested, not the OPEN arg1 wiring) | yes (sim captures the OPEN write) |

### Flow control / window accounting (`flow_control.rs`)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| FC-1 classic has no window | `new_classic` | `is_windowed()==false`, `available_bytes()==None`, `can_send()==true` (caller enforces stop-and-wait) | `unit` (`classic_mode_has_no_window`) | yes |
| FC-2 windowed 32 MiB grant | `new_windowed(INITIAL_DELAYED_ACK_BYTES)` | `available_bytes()==Some(32 MiB)` | `unit` | yes |
| FC-3 opener starts at 0, blocks until credited | `new_windowed(0)` then first OKAY delta credits it | `can_send()==false` at 0; true after `apply_delta(grant)` | `unit` (`opener_starts_at_zero_and_blocks_until_credited`) | yes |
| FC-4 record_sent debits | `record_sent(n)` | window -= n | `unit` | yes |
| FC-5 exhaust → block → OKAY recovers | drain window to 0, then OKAY delta | `can_send()==false` at 0; recovers after delta | `unit` (`window_exhaustion_blocks_then_recovers_after_okay`) | yes |
| FC-6 over-send → negative window | `record_sent` past remaining | window goes negative (one in-flight over-send allowed); `can_send()==false` | `unit` (`window_may_go_negative_via_oversend`) | yes |
| FC-7 negative OKAY delta | OKAY payload `-400` i32 | window shrinks, no panic (signed/saturating) | `unit` (`negative_delta_is_applied_without_panic`) | yes |
| FC-8 delta boundary 0 | OKAY payload empty (len 0) | `parse_okay_delta` → `Some(0)`; no-op credit | `unit` (`empty_payload_in_windowed_mode_is_noop_credit`, `classic_empty_payload_okay_is_noop`) | yes |
| FC-9 delta boundary MAX_PAYLOAD / i32 range | OKAY payload at i32::MAX/MIN, 32 MiB | round-trips i32 LE | `unit` (`i32_le_round_trip_through_okay_payload`) | yes |
| FC-10 overflow saturation | accumulate past i64::MAX | saturates, no panic | `unit` (`overflow_accumulation_does_not_panic`) | yes |
| FC-11 malformed OKAY payload len ∉ {0,4} | OKAY payload 3 or 8 bytes | `parse_okay_delta` → None → reader logs+ignores, window unchanged | `unit` (flow: `malformed_okay_payload_is_rejected`; reader-side `none`) | yes (reader-side: send a 3-byte OKAY and assert window unchanged) |
| FC-12 encode classic empty / windowed i32 LE | `encode_okay_payload(false/true, bytes)` | classic empty; windowed = byte count i32 LE, clamped to i32::MAX | `unit` (`encode_okay_payload_*`) | yes |
| FC-13 per-WRTE chunk clamp | write buf > MAX_PAYLOAD | `poll_write_impl` clamps `chunk_size = buf.len().min(MAX_PAYLOAD)` (persistent.rs:2666), decoupled from window | `none` (no test drives a >1 MiB write) | yes |
| FC-14 lossless credit via atomic | reader banks delta in `recv_credit` atomic even when ack queue full; empty-payload poke wakes parked writer | write unblocks from the ATOMIC, not the poke payload | `unit` (`windowed_write_credit_comes_from_atomic_not_poke_payload`) | yes |

### Session OPEN handshake (`open_session` / `await_open_response`, persistent.rs:1597-1725)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| OPEN-1 OPEN→OKAY success | device replies `OKAY(arg0=remote_id, arg1=local_id, payload=window-grant)` | `open_session` returns a session; `remote_id = OKAY.arg0`; host then sends a ready-OKAY | `unit` (`await_open_response` ack path; full open_session `none` end-to-end) | yes |
| OPEN-2 early CLSE (rejection) | device replies `CLSE(arg0=0, arg1=local_id)` on the DATA channel before any OKAY | fails FAST with `ADBRequestFailed("OPEN rejected by device (CLSE)")`, NOT a 10s timeout (bug #3) | `unit` (`open_response_clse_on_data_channel_fails_fast`) | yes |
| OPEN-3 OPEN timeout (no response) | device sends nothing for `OPEN_RESPONSE_TIMEOUT` (10s) | `ADBRequestFailed("timeout waiting for OKAY")` | `none` end-to-end (timeout logic via `await_open_response` is unit-covered for the race, not the timeout) | partial (10s wall unless `start_paused`) |
| OPEN-4 register-before-OPEN ordering | session registered in reader map BEFORE OPEN is written (persistent.rs:1623) so the device's reply routes to the session not the device-OPEN queue | A fast OKAY reply is not misrouted | `none` (ordering is structural; the reader's `drain_control` mid-frame logic at persistent.rs:1287 backs it) | partial (needs the live reader loop, not just `await_open_response`) |
| OPEN-5 ready-OKAY after OPEN | windowed → ready-OKAY payload = 32 MiB i32 LE; classic → empty payload (persistent.rs:1682) | adbd won't WRTE until it gets this initial OKAY | `none` (the OKAY is built but no test asserts it on the wire end-to-end) | yes (sim asserts the host ready-OKAY) |
| OPEN-6 send-window seeded from atomic not payload | after OKAY, `send_flow.apply_delta(recv_credit.swap(0))` (persistent.rs:1702) | grant captured ONCE from the lossless atomic, no double-count of the handshake OKAY | `none` (the seed logic; `windowed_write_credit_comes_from_atomic_not_poke_payload` covers the analogous write-path drain) | yes |
| OPEN-7 OPEN write failure unregisters | `send_open` errors → `unregister_session` then propagate | partial registration is undone | `none` | yes (sim writer returns error on the OPEN) |
| OPEN-8 unexpected non-OKAY on ack channel | defensive: a non-OKAY frame somehow on `ack_rx` | `ADBRequestFailed("expected OKAY, got ...")` (persistent.rs:1660) | `none` (reader only routes OKAY to ack, so unreachable via real routing) | partial |

### Session accept (device-initiated OPEN) (`accept_device_open`, persistent.rs:1756-1854)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| ACC-1 device OPEN → accept | inbound `OPEN(arg0=device_local, arg1=window|0, payload="dest\0")` routed to `incoming_opens`; caller accepts | `remote_id=OPEN.arg0`; registers OUR local_id; replies `OKAY(arg0=local, arg1=remote, payload=grant)` | `unit` (`device_originated_open_routes_to_pending_opens` for the routing; full accept `none`) | yes |
| ACC-2 send window seeded from OPEN arg1 | windowed → `initial_send_grant = OPEN.arg1`; classic → 0 (persistent.rs:1772) | acceptor send window seeds from OPEN arg1 (often 0), then accrues adbd OKAYs; `recv_credit` starts 0 (no double-count) | `unit` (`acceptor_send_flow_*` for the policy fn) | yes |
| ACC-3 windowing is connection-level not per-OPEN | OPEN arg1 may differ from connection delayed_ack mode | session uses CONNECTION's mode (else a 4-byte OKAY from adbd is rejected and send never credited) | `none` (documented invariant at persistent.rs:1762) | yes |
| ACC-4 ready-OKAY enqueue failure unregisters | `try_send_fire_forget` of the reply OKAY fails | `unregister_session` then propagate IOError (persistent.rs:1827) | `none` | yes |
| ACC-5 reject path | caller sends `CLSE(0, device_local_id)` via `send_raw` instead of accepting | device's stream is rejected; no session registered | `none` | yes |

### Reader routing / classify (`classify_message`, reader_loop, persistent.rs:373-395, 1196-1365)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| RTE-1 WRTE→data | `WRTE(arg1=known local_id)` | `SessionData(id)` | `unit` (`wrte_to_known_session_routes_to_data`) | yes |
| RTE-2 OKAY→ack | `OKAY(arg1=known)` | `SessionAck(id)` | `unit` (`okay_to_known_session_routes_to_ack`) | yes |
| RTE-3 CLSE→data (not ack) | `CLSE(arg1=known)` | `SessionData(id)` (CLSE is a data-channel event) | `unit` (`clse_to_known_session_routes_to_data`) | yes |
| RTE-4 device OPEN→pending | `OPEN(arg1=0/unknown)` | `DeviceOpen` | `unit` (`device_originated_open_routes_to_pending_opens`) | yes |
| RTE-5 unknown non-OPEN dropped | `WRTE(arg1=unknown)` | `Unknown` → silently dropped | `unit` (`unknown_non_open_message_is_dropped`) | yes |
| RTE-6 interleaved streams (multiple local-ids) | concurrent WRTE/OKAY for several registered local_ids | each routes to its own session by `arg1` | `none` (classify is per-message unit-tested; multi-session interleave on the live reader is `none`) | yes (needs live reader_loop) |
| RTE-7 register-mid-frame ordering | `Register` arrives DURING an uninterruptible frame read; reply frame completes the read | `drain_control` runs BEFORE classify (persistent.rs:1287) so the just-registered session is present; otherwise reply misroutes to DeviceOpen | `none` (described as the bug this guards; not unit-tested) | partial (needs precise interleave timing on the live reader) |
| RTE-8 OKAY credit banked even on full ack queue | OKAY for a session whose `ack_tx` queue is full | the signed delta is `fetch_add`ed into `recv_credit` (lossless) BEFORE the best-effort `try_send` poke (persistent.rs:1304-1317) | `none` (the lossless principle is covered on the session side, not the reader's bank-then-poke) | yes |
| RTE-9 CLSE sets closed flag even if data queue full | CLSE for a session whose `data_tx` is full | `channels.closed.store(true)` set DIRECTLY before the best-effort `try_send`; dropped CLSE still yields EOF | `unit` (`clse_closes_session_via_flag_even_if_data_queue_dropped_it`, session-level) | yes |
| RTE-10 dropped WRTE warns | WRTE for a session whose `data_tx` is full (and NOT a CLSE) | reader `tracing::warn!`s the drop (never silent) | `none` | yes |
| RTE-11 malformed OKAY payload at reader | OKAY whose payload len ∉ {0,4} | reader logs+ignores, no credit banked (persistent.rs:1309) | `none` | yes |
| RTE-12 raw tee orthogonal to route | message matching a `subscribe_raw` filter | tee'd IN ADDITION to its primary route; non-matching not tee'd; dropped subscriber pruned | `unit` (`raw_tee_delivers_only_matching_messages`, `raw_tee_prunes_disconnected_subscribers`) | yes |

### Read-step classification / liveness / teardown (persistent.rs:1226-1421, 1939-2092)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| LIV-1 ReadTimeout is NOT fatal | read deadline elapses → `RustADBError::ReadTimeout` | `classify_read_result` → `ReadStep::ReadTimeout` → reader `continue`s; connection NOT torn down | `unit` (`read_timeout_classifies_as_read_timeout_not_fatal`, both USB+TCP) | yes (sim returns ReadTimeout when idle) |
| LIV-2 InvalidIntegrity is recoverable | read returns `InvalidIntegrity` (bad magic) — frame already fully consumed (header+payload read before the magic check) | reader `warn`s and `continue`s, keeps serving other sessions | `none` (the recoverable branch at persistent.rs:1260 is untested) | yes (sim returns `InvalidIntegrity` from one `read_message`, then a good frame) |
| LIV-3 ConversionError is FATAL | read returns `ConversionError` (unknown command in header decode, BEFORE data_length known) | reader treats as fatal `break` (payload still on wire, would desync) | `none` | yes |
| LIV-4 oversize data_length fatal | read returns the bound error (`data_length > MAX_PAYLOAD`) before payload read | fatal `break` (refused payload still on wire) | `none` (predicate `payload_len_within_bound` is unit-tested in adb_transport_message.rs) | yes |
| LIV-5 generic IO / disconnect fatal | read returns any non-ReadTimeout, non-InvalidIntegrity error | fatal `break` → fires DeathSignal | `unit` (`classify_read_result` BrokenPipe → ReadError) | yes |
| LIV-6 control channel closed | all `control_tx` senders dropped | `drain_control` → `ControlDrain::Closed` → `ReadStep::Closed` → reader exits | `none` | partial (needs live connection drop) |
| LIV-7 writer WriteTimeout recoverable | writer write returns `RustADBError::WriteTimeout` (backpressure, nothing reached wire) | writer `continue`s (frame-atomic Scheme B); does NOT tear down (the iperf3 reverse-tunnel regression) | `none` | yes (sim writer returns WriteTimeout once) |
| LIV-8 writer other error fatal | writer write returns any other error (partial frame / truncation) | writer `break`s → fires DeathSignal | `none` | yes |
| LIV-9 is_alive requires BOTH halves | reader OR writer task finished | `is_alive()==false` if either is finished (half-open must not be reused) | `none` (logic at persistent.rs:1952) | partial (needs a live connection where one task exits) |
| LIV-10 DeathSignal wake parked waiter | fire AFTER a waiter parks on `notified()` | waiter wakes | `unit` (`death_signal_wakes_a_parked_waiter`) | yes |
| LIV-11 DeathSignal already-dead | fire BEFORE anyone awaits | `wait()` returns immediately (edge never lost) | `unit` (`death_signal_resolves_when_already_dead`) | yes |
| LIV-12 death mid-handshake | device closes during CNXN/AUTH (read errors out) | `do_connect`/`do_auth` returns an error BEFORE tasks spawn (no DeathSignal yet — it is created at persistent.rs:656 AFTER handshake) | `none` | yes |
| LIV-13 death mid-session | device closes after sessions opened; reader/writer hit fatal break | DeathSignal fires; `wait_closed`/`closed_signal` resolve; server publishes TransportReset | `hw` (PR0 real-hardware: reader died 20/20 cycles, max 250ms) | yes (sim returns a fatal read error after some session traffic) |
| LIV-14 Drop fires DeathSignal | connection dropped while still alive (tasks aborted, not naturally exited) | Drop calls `self.closed.fire()` (idempotent) so a TransportReset watcher does not leak (persistent.rs:2090) | `none` | partial (needs live connection) |

### Session read/write byte-stream state machine (persistent.rs:2403-2703)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| SES-1 WRTE delivers payload + emits OKAY | device WRTE | payload copied to buf; crediting OKAY enqueued SYNCHRONOUSLY (no await between recv and OKAY — cancellation safety P1-③); windowed OKAY = byte count i32 LE | `unit` (`read_emits_okay_and_delivers_payload`) | yes |
| SES-2 0-byte WRTE | WRTE with empty payload | OKAY still emitted; read makes progress, copies 0 bytes (persistent.rs:2494) | `none` | yes |
| SES-3 partial copy / re-buffer tail | WRTE payload larger than `buf.remaining()` | excess stashed in `read_buf`/`read_pos`, returned on next poll before any new recv | `none` (logic at persistent.rs:2424, 2500) | yes |
| SES-4 CLSE → EOF | device CLSE | read returns 0 (EOF) | `unit` (`read_returns_eof_on_clse`) | yes |
| SES-5 drain buffered data before EOF | session closed (flag set) but data still queued | deliver queued WRTE (`try_recv`) FIRST, EOF only when channel empty (persistent.rs:2442) | `unit` (`clse_closes_session_via_flag_even_if_data_queue_dropped_it`) | yes |
| SES-6 channel disconnected → BrokenPipe | `data_rx` poll returns None (reader gone) | read errors `BrokenPipe("session channel closed")` | `none` | partial |
| SES-7 unexpected cmd in data channel | a non-WRTE/CLSE on data channel | read errors `InvalidData("unexpected command ...")` | `none` (unreachable via real routing) | partial |
| SES-8 write goes through writer w/ ack | credited window, write | WRTE enqueued as `WithAck`; window debited only AFTER ack resolves (persistent.rs:2534) | `unit` (`write_goes_through_writer_task_with_ack`) | yes |
| SES-9 write blocks until window credited | opener window 0, write | parks (no WRTE enqueued) until an OKAY credits; then flushes | `unit` (`write_blocks_until_window_credited_then_proceeds`) | yes |
| SES-10 write after remote close | CLSE on ack channel, then write | write fails `BrokenPipe` | `unit` (`write_after_remote_close_fails_with_broken_pipe`) | yes |
| SES-11 writer queue full backpressure | `writer.tx` full at enqueue time | `poll_write` wakes self and returns Pending (re-poll), never blocks (persistent.rs:2681) | `none` | partial (needs to fill the 256-deep writer queue) |
| SES-12 writer task gone on enqueue | `writer.tx` closed | `mark_closed` + `BrokenPipe("writer task gone")` | `none` | yes |
| SES-13 in-flight write ack error / canceled | writer ack returns Err or oneshot canceled | `mark_closed`; `poll_write` returns the error / `BrokenPipe("writer dropped ack")` (persistent.rs:2545) | `none` | yes |
| SES-14 cancellation-safe read | read future cancelled before any WRTE | no frame consumed, no spurious OKAY; next read still gets the WRTE and emits exactly ONE OKAY (no double-credit) | `unit` (`cancelled_read_does_not_lose_the_frame_or_its_credit`) | yes |
| SES-15 drop write half mid-WRTE | drop while `WriteState::Sending` | no panic; un-debited window accepted (dies with the half); CLSE still fires on last shared ref drop | `unit` (`drop_write_half_while_wrte_in_flight_is_clean`) | yes |

### Teardown / Drop / CLSE (persistent.rs:285-297, 2002-2092, 2137-2168, 2292-2306)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| TD-1 graceful connection CLSE once | `shutdown`/`close` | exactly ONE connection-level `CLSE(0,0)` flushed with ack; `conn_closed` set; idempotent (compare_exchange) | `unit` (`flush_connection_clse_sends_once_and_is_idempotent`) | yes |
| TD-2 session Drop sends CLSE + unregisters | drop a session without graceful close | best-effort `CLSE(local,remote)` fire-forget + `Unregister(local)` | `unit` (`drop_without_close_enqueues_clse_and_unregisters`) | yes |
| TD-3 session close() then Drop no duplicate | `close()` sends CLSE, marks closed | Drop sees `closed==true`, only unregisters (no dup CLSE) | `unit` (`close_sends_clse_then_drop_does_not_duplicate`) | yes |
| TD-4 connection-close suppresses per-stream CLSE | `conn_closed` set, then session Drop | session Drop skips per-stream CLSE (still unregisters) — avoids racing the retiring writer | `unit` (`drop_after_connection_closed_suppresses_per_stream_clse`) | yes |
| TD-5 Drop without graceful close fire-forgets conn CLSE | connection dropped, `conn_closed` false | best-effort `CLSE(0,0)` enqueued; warns if writer queue full/gone (persistent.rs:2066) | `none` | partial (needs live connection) |

### STLS / TLS upgrade (persistent.rs:1054-1086; adb_message_transport.rs:28)

| Edge id | Trigger sequence / timing | Invariant | Test status | Sim-reproducible |
|---|---|---|---|---|
| TLS-1 device requests STLS | device CNXN-read returns `STLS` (TCP only; USB never) | host replies `STLS(1,0)` then calls `upgrade_connection()` (TLS handshake for TCP, no-op for USB) | `none` | partial (sim can answer STLS; `upgrade_connection` default is a no-op so the post-upgrade behavior of a real TLS transport can't be exercised by a plain sim) |
| TLS-2 post-STLS banner swallowed | after upgrade, `upgrade_connection` consumes the device's post-STLS CNXN internally | `do_connect` does NOT read again; returns `(A_VERSION_LEGACY, "")` → delayed_ack negotiates to false (classic over TLS) | `none` | partial |
| TLS-3 USB never sends STLS | a USB sim sends STLS | host would still reply+upgrade (upgrade is a no-op for USB); this is a "should never happen" path | `none` | yes (sim can force it, but it is not the real USB contract) |

---

## Caveats / Not Found

- **No end-to-end live-connection harness exists today.** The only transport-level mock is `ScriptedTransport` (persistent.rs:2860), which exercises ONLY `do_connect` retry budgets and answers every read with one canned CNXN banner. It does NOT model AUTH, sessions, multi-frame demux, the reader/writer loops driving a full `PersistentConnection::new`, or any host→device write being observed and answered. A `SimulatedDevice` is the missing generalization.
- **The transport is cloned into two tasks** — a faithful sim must share device state between the reader's `read_message` clone and the writer's `write_message` clone (channels or `Arc<Mutex<_>>`), and must drive both halves of an OPEN/OKAY/WRTE/CLSE conversation from one logical state machine.
- **Timeout-bound edges (OPEN-3 10s, AUTH-4 10s, DRAIN settle 100ms, reader 1s)** need `tokio::test(start_paused = true)` to stay deterministic; the existing `do_connect_*` tests already use `start_paused`.
- **Edges marked `partial`** generally need the *live* `reader_loop`/`writer_loop`/`is_alive` running over the sim transport (e.g. RTE-6/7, LIV-6/9/14, SES-6/11), not just the I/O-free helper (`classify_message`, `await_open_response`, `FlowControl`, `negotiate_delayed_ack`) which is already unit-testable in isolation.
- **`upgrade_connection` (STLS)** has a blanket no-op default in the trait (adb_message_transport.rs:28); a plain sim can answer STLS and assert the host's `STLS(1,0)` reply + that no second read happens, but it cannot exercise a real TLS upgrade — that lives in `TcpTransport::upgrade_connection` (not read here; outside the persistent-connection seam).
- I did not read `usb_transport.rs` / `tcp` transport read-path internals in full; the `data_length`/magic ordering claims for LIV-2/3/4 come from the reader-loop comments (persistent.rs:1239-1266) and `adb_transport_message.rs` (`ConversionError` on header decode, `payload_len_within_bound`, magic-only integrity check), which are consistent with each other.
