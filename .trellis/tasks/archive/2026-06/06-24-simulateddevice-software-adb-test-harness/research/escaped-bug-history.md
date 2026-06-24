# Research: Escaped / reactively-fixed protocol-timing-statemachine bugs (SimulatedDevice candidates)

- **Query**: Catalog protocol-layer / timing / state-machine bugs in adboost that were fixed *reactively* (escaped to xdb, or needed a physical phone + manual `adb root` loop to repro) — each a candidate for a deterministic `SimulatedDevice` cargo test above the `ADBMessageTransport` interface.
- **Scope**: internal (git history + archived Trellis tasks + journal)
- **Date**: 2026-06-24

## Method / sources

- `git log -p` on the key `fix(...)` commits (see per-bug commit hashes).
- `.trellis/workspace/magicdian/journal-1.md` (Sessions 6–9, 12–13, 19–29 are the bug narratives).
- Archived PRDs under `.trellis/tasks/archive/2026-06/` (esp. the two root/unroot tasks with real-hardware traces).

## Key interface fact (drives the "can a SimulatedDevice reproduce it?" column)

The transport seam is the trait `ADBMessageTransport` at
`adboost/src/message_devices/adb_message_transport.rs:25`:

```
pub trait ADBMessageTransport: ADBTransport + Clone + Send + 'static {
    async fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<ADBTransportMessage>;
    fn read_message(&mut self) -> ... ;
    async fn write_message_with_timeout(&mut self, msg: ADBTransportMessage, timeout: Duration) -> Result<()>;
    fn write_message(&mut self, ...) -> ... ;
}
```

A frame-level mock already exists and is the proof the seam works:
`ScriptedTransport` (`adboost/src/message_devices/usb/persistent.rs:2861`, impls
`ADBMessageTransport` at :2886) — a "fail-N-then-succeed" scripted transport used by the
`do_connect` retry contract test (`:2918`). A `SimulatedDevice` would generalize this from
a fixed script into a stateful adbd model. `PersistentConnection<T>` is already generic over
`T: ADBMessageTransport` (`:508`), so a simulated device plugs in with zero production change.

**Decision boundary the catalog turns on**: bugs whose failure is fully expressible as a
*sequence of `ADBTransportMessage`s* (CNXN/OPEN/OKAY/WRTE/CLSE + bytes + timing) are
reproducible above this trait. Bugs whose trigger lives *below* the trait — USB controller
coalescing, IOKit re-enumeration to a new registry id, kernel `TCP_NODELAY`, `tokio::time`
cancellation crossing a partial socket read — are NOT directly reproducible by a frame-level
SimulatedDevice; they need either a byte-level transport mock or are out of scope.

---

## Theme 1 — Handshake / banner / version negotiation (delayed_ack saga)

These are the cleanest SimulatedDevice wins: pure wire-sequence bugs, all originally escaped
to xdb and required Android-16 hardware + raw-frame capture to root-cause.

| id | symptom | root cause | trigger condition (message seq) | SimulatedDevice repro? |
|---|---|---|---|---|
| **B1 delayed_ack-version** | `open_session` times out 10s on Android 16; Android 11 fine | CNXN sent at legacy `0x01000000` but advertised `delayed_ack`; AOSP requires `>= A_VERSION_SKIP_CHECKSUM (0x01000001)` for windowed flow control, so adbd ignored the windowed OPEN | host CNXN(version=0x01000000, features=...,delayed_ack) → device CNXN → host windowed OPEN(arg1=32MiB) → **device never OKAYs** | **YES** — sim sends CNXN banner, then "if host OPEN.arg1>0 while my negotiated version<skip_checksum, drop it". Commit `46d674f`. |
| **B2 data_check regression** | CRITICAL: every payload frame fails `Invalid integrity ... got 0`; CNXN reply is first casualty → whole handshake dies for ALL delayed_ack devices | Bumping CNXN to `0x01000001` (fix B1) activated peer skip-checksum mode → adbd sends `data_check=0`, but `check_message_integrity()` still recomputed+compared crc32 | device sends ANY payload-bearing frame (CNXN banner, WRTE, windowed OKAY, AUTH, OPEN) with `data_check=0` → adboost rejects it | **YES** — sim emits a frame with `data_check=0`; assert adboost (now magic-only) accepts. Commit `09ca21e`. |
| **B3a OPEN-reject hang** | windowed OPEN hangs 10s then times out silently (not the true cause, but the first reactive fix) | On OPEN rejection adbd sends `A_CLSE(arg0=0, arg1=local_id)`, routed to the session DATA channel; `open_session` only awaited the ack channel → CLSE never observed | host OPEN → device CLSE(arg0=0,arg1=local_id) → adboost waits on ack_rx forever (10s) | **YES** — sim replies CLSE to an OPEN; assert fast-fail "OPEN rejected by device (CLSE)" not a 10s timeout. Commit `6fec37e` (item C). |
| **B3b banner trailing NUL** (TRUE root cause of B3) | windowed OPEN rejected with CLSE on real Android-16; only surfaced after B1/B2 let the handshake reach OPEN | `to_banner_string()` appended a trailing `\0`; adbd's `StringToFeatureSet` splits on `,` without trimming → last token became `delayed_ack\0 != delayed_ack` → `SupportsDelayedAck()` false → `arg1` mismatch → `send_close` | host CNXN(features=`shell_v2,delayed_ack\0`) → device CLSE on the subsequent windowed OPEN (~1.8ms). `shell_v2` (first token) masked it. | **PARTIAL** — adboost-side: sim can assert the emitted banner bytes have no trailing NUL (regression lock already does this). Full adbd-side semantics (NUL corrupts last CSV token) require the sim to model `SupportsDelayedAck()==bool(arg1)` and CLSE accordingly — doable but it's modelling an adbd *bug*. Commit `a0e39da`; device-verified (banner WITH NUL → CLSE 1.8ms, WITHOUT → OKAY payload=[00,00,00,02]=32MiB grant ~13ms). |
| **B-feat per-device over-advertise** | stripped adbd (empty `features=` banner, reached via `adb forward`+`adb connect`) CLSE's every `shell,v2` OPEN → `adb shell` unusable | server negotiated caps once, globally, from device-agnostic `capabilities()`; advertised `shell_v2` to a feature-less device | host (via server) OPEN `shell,v2,...,pty:` → stripped device CLSE | **YES** — two simulated devices with different CNXN banners; assert server gates shell_v2 per-device (∩ of server∩device caps). Commit `67cc53e`. Found via hypervisor Yocto-Linux stripped adbd, not unit test. |

---

## Theme 2 — Frame integrity / desync / cancel-safety (the "async path lacks a USB guarantee" class)

Architecturally tagged as a recurring class (see MEMORY: "TCP/async path missing USB guarantees").
Mixed reproducibility: the *consequences* (desync, fatal teardown) are frame-expressible, but the
*triggers* often live below the trait (socket-level partial read, controller coalescing).

| id | symptom | root cause | trigger condition | SimulatedDevice repro? |
|---|---|---|---|---|
| **B4 cancel-safe read desync** | IP-direct connections randomly drop on large output (e.g. `ifconfig`); next read decodes illegal command word → fatal `ConversionError` tears down all sessions | `read_exact` wrapped in `tokio::time::timeout` is NOT cancel-safe; a 1s reader timeout crossing a partial read drops bytes already off the wire | a frame split across the 1s read-timeout boundary on TCP | **NO (at frame level) / PARTIAL (byte level)** — the trigger is sub-frame socket-read cancellation. A frame-level `SimulatedDevice` can't split a frame across a timeout. Needs a byte-level/chunked transport mock that returns partial reads + a timeout. Fixed by shared `FrameReadBuffer` (`framed_read.rs`), commit `1aac71c`. (Its own regression test does exactly this at the FrameReader level.) |
| **B5 read_exact over-read (bulk IN coalescing)** | first large reverse WRTE tore down the whole multiplexed connection ("frame desync") | a bulk IN completion can return MORE than requested (max_packet_size alignment + controller coalescing); old fatal guard assumed adbd writes header/payload separately and never over-delivers (spec was WRONG on IN path) | sustained device→host throughput → one IN transfer carries >1 frame's bytes | **NO** — over-delivery is a USB-controller artifact below the trait; a frame-level sim delivers exactly one message per read by construction. Byte-level mock only. Found on device; commit (reverse acceptor session, Session 13) + folded into `1aac71c`. |
| **B6 reader cancelled mid-frame by control_rx** | a Register/Unregister mid-frame corrupted an in-flight WRTE → one of two concurrent device→host streams stalled | reader frame reads were `select!`ed against `control_rx` → not cancel-safe | concurrent control-channel op arriving mid-frame during a WRTE | **PARTIAL** — needs the sim to interleave a control op with a multi-read frame; only matters with a byte-level/partial-read transport. Frame-atomic at frame granularity is fine. Session 13. |
| **B7 write truncation / poisoning** | a partial frame write is unrecoverable for the peer; next frame appended to a truncation → desync | `writer_loop` warn-and-continued on a write error | a write error mid-frame on TCP | **PARTIAL** — sim/transport must inject a write failure after N bytes; frame-level mock can model "fail this write" but not "fail after k bytes". Commit `1aac71c` (#2). |
| **B8 is_alive half-open** | half-open connection (write dir dead, read dir idling on 1s timeout) reported alive; backend reused it; every outbound frame (incl. flow-control OKAYs) silently dropped → peer stalls with no teardown | `is_alive()` consulted only `reader_handle`, not the writer task | writer task dead while reader merely idle | **YES (state-machine)** — sim/transport drives writer to fatal while reads keep timing out; assert `is_alive()==false` and the connection is not reused. Commit `584dd75`. |
| **B9 backpressure misclassified as truncation** | saturating `reverse_iperf3` torn down ("control socket has closed unexpectedly") after B4's "any write error = fatal" | host→device OUT path briefly fills; 2s write timeout fires with ZERO bytes committed (recoverable backpressure) but was treated as fatal truncation | sustained write saturation → write-start timeout, 0 bytes committed | **PARTIAL** — needs a transport that applies backpressure (slow drain) so the write-start timeout fires; frame-atomic Scheme B distinguishes 0-byte-committed (recoverable `WriteTimeout`) from mid-frame (fatal). Commit `ea88205`. |
| **B-recv recv_file short frame panic** | panic slicing a DONE-trailer against a short/empty device frame | unguarded slice on a too-short sync frame | device sends a sync frame shorter than the trailer | **YES** — sim emits a truncated sync DONE frame; assert graceful error not panic. Commit `23c2078`. |

---

## Theme 3 — Reconnect / re-enumeration / wait-for state machine (the root/unroot saga)

The most reactive cluster: every one escaped to xdb and required a physical MTK phone with a
manual `adb root; adb unroot` loop. Mixed reproducibility — the *handshake/framing* parts are
frame-expressible, the *re-enumeration to a new IOKit registry id* is fundamentally below the trait.

| id | symptom | root cause | trigger condition | SimulatedDevice repro? |
|---|---|---|---|---|
| **B10 wait-for single OKAY** | client `error: protocol fault (couldn't read status)`, instant | `serve_wait_for` sent ONE bare OKAY; AOSP client reads TWO for `wait-for-*` (accept + satisfied). `handle_client` never emits a smartsocket accept OKAY, so each service must emit its own pair | client sends `host-transport-id:<N>:wait-for-any-disconnect` after `adb root`; reads 1 OKAY, expects 2 | **YES** — sim drives the frontend wait-for path; assert two OKAYs on the wire. Commit `0977368`. |
| **B11 wait-for-disconnect presence poll hangs 60s** | after `restarting adbd as root` the client hung ~60s; user ^C; rerun showed it had taken effect | disconnect was approximated as polling `list_devices()` until serial absent (200ms/60s). On MTK, adbd restart != USB re-enumeration → serial never leaves `list_devices()` → absence never true | adbd restarts WITHOUT the USB serial dropping from the device list | **PARTIAL** — needs the backend `subscribe_lifecycle`/`transport_alive` seam, not the message transport. A sim that models "reader dies on adbd restart" can drive the new event-driven path (`LifecycleEvent::TransportReset` on reader death), but the bug is about backend presence-vs-teardown semantics, above the message transport. Commit `0977368`. |
| **B12 re-enumeration: in-place CNXN retry spins forever** | `adb unroot` right after `adb root` failed: `CNXN failed after 8 attempts (stale CLSE or transient transfer error)` | adbd restart re-enumerates USB under a NEW IOKit registry id → old transport endpoints permanently dead. `do_connect(&mut T)` only re-sends I/O, never reopens the transport → in-place retries spin on a dead handle (real trace: 15/15 identical `device disconnected`, then first reopen of a fresh transport succeeded) | back-to-back control service: `root` (no longer stalls 60s after B11 fix) → `unroot` immediately hits the re-enumeration window | **NO** — "old endpoint dead, new registry id needed" is an IOKit/USB fact below `ADBMessageTransport`. A frame-level sim cannot model "this transport handle is now permanently dead, you must construct a new one." Reopen recovery belongs at the `get_or_open`/`retry_within` layer (rebuild transport). Commit `0977368` / `19b86d4`. |
| **B13 endpoint Stall not in transient family** | `open session failed: USB transfer error: endpoint stalled` on back-to-back root/unroot | `is_transient_connect_error` deliberately excluded `Stall` ("avoid masking a real stall"); but the re-enumeration window legitimately stalls briefly → CNXN returned Err without using its retry budget | OPEN/CNXN during the re-enumeration readiness window returns `UsbTransferError(Stall)` | **PARTIAL** — `ScriptedTransport` already proves "fail-N-then-succeed" (`persistent.rs:2861`). A sim returning `Stall` then success can validate the *classifier* (is `Stall` transient? does the outer budget re-drive?) WITHOUT hardware. The retry-budget *timing* calibration (reopen 487–1177ms vs 800ms inner budget) is timing-sensitive but the classification is pure. Commit `0977368`. |
| **B14 two retry budgets not chained** | inner CNXN exhaustion returned `ADBRequestFailed`, which the outer `is_retryable_open_error` didn't recognize → outer 10s budget never re-drove the reopen | `is_retryable_open_error` only matched transient-transfer + `DeviceNotFound`, not `ADBRequestFailed` | inner CNXN exhausts 8 attempts → `ADBRequestFailed("CNXN failed...")` → outer gives up immediately | **YES (classifier-level)** — pure: assert `is_retryable_open_error(ADBRequestFailed)` is now retryable and `InvalidArgument`/`Fault`/`DeviceBusy` are fatal. The PRD's own acceptance criteria call for exactly this closure-level test (`retry_within` "first ADBRequestFailed(CNXN exhausted) → reopen succeeds"). Commit `0977368`. |
| **B15 transient transfer not retried after re-enum (backend)** | adb root reconnect raced the not-ready endpoint and failed with zero retry; `0xe00002ed` NotResponding / `0xe00002c0` NoDevice | backend opened with zero retry around the brief not-ready window after adbd restart | first OPEN / CNXN immediately after adbd restart returns IOKit transient | **PARTIAL** — classification is pure (`ScriptedTransport` fail-N-then-succeed proves do_connect rides out transients — this contract test ALREADY EXISTS, `persistent.rs:2918`); the underlying not-ready-endpoint timing is below the trait. Commit `19b86d4`. |
| **B-known back-to-back silent reply** | a back-to-back control service can return SILENTLY (adbd tears the stream down before emitting reply text); command still takes effect | adbd race; native adb shows the same | second control service OPEN races adbd's teardown of the first | **PARTIAL** — documented as known-acceptable, not fixed. A sim could model "CLSE before reply WRTE" to lock the tolerated behavior. Commit `0977368` (spec note). |

---

## Theme 4 — Host-protocol parity / routing / framing (not state-machine timing, lower SimulatedDevice value)

These escaped to xdb / failed against real `adb` clients but are host-protocol *parsing* bugs,
mostly already covered by hermetic frontend unit tests; listed for completeness.

| id | symptom | root cause | repro? |
|---|---|---|---|
| **B16 host-serial colon split** | every shell through a `host:connect`d TCP device failed: `unknown host-serial sub-service: 5555:features` | `host-serial:<serial>:<sub>` split on first colon; `ip:port` serial mis-parsed | YES (pure parser). Commit `a80dfd0`. |
| **B17 ReadTimeout not transport-neutral** | idle TCP conn's 1s read timeout fell through to fatal arm → tore down connection → `host:disconnect` → `open session failed` | reader matched USB-specific `UsbTimeout`; TCP returned `IOError(TimedOut)`; `UsbTimeout` was even `cfg(usb)`-gated | YES (classifier-level) — sim/TCP transport returns the neutral `ReadTimeout`; assert reader treats it non-fatal. Commit `4951301`. |
| **B18 STLS double-read hang** | `host:connect` to any TLS device would HANG | post-STLS `finish_after_stls` double-read; `upgrade_connection()` already consumes the post-STLS CNXN | PARTIAL (TLS upgrade path; caught in review, not on hardware). Commit `c6447d7`. |
| **B19 tport:any wrong error** | multi-device `adb shell` reported "device not found" instead of "more than one device" | `select_tport` collapsed all failures to one reason | YES (pure). Commit `087ee85`. |
| **B20 adb -d/-e kind tokens** | `adb -d`/`-e` failed "device not found"; modern adb sends `host:tport:usb/local`, parsed as a serial | missing kind-token branch | YES (pure). Commits `bbc2b3e`, `43217d2`. |
| **B-fwd forward leak on unplug** | `forward --list` still showed a rule after USB unplug; reverse map lingered | no release on transport disconnect | PARTIAL — needs the lifecycle/disconnect seam, not the message transport. Commit `82006cc`. |

---

## Summary verdict — strongest SimulatedDevice candidates (frame-expressible, escaped, no hardware needed)

1. **B1, B2, B3a, B-feat** (Theme 1): pure CNXN/OPEN/CLSE/banner sequences. Highest value — all
   escaped to xdb and needed an Android-16 phone + raw-frame capture; a stateful adbd model
   (banner → version-gated OPEN accept/reject → CLSE) reproduces them deterministically.
2. **B8** (half-open `is_alive`), **B-recv** (short sync frame panic), **B14/B17** (error-family
   classifiers): state-machine / pure-classifier, fully reproducible above the trait.
3. **B10** (two-OKAY wait-for framing): frontend wire sequence, reproducible.

## Caveats / not reproducible by a frame-level SimulatedDevice

- **Below-the-trait triggers**: B4 (cancel-safe partial socket read), B5 (USB bulk-IN
  coalescing/over-read), B7/B9 (write truncation/backpressure at byte granularity), B12/B15
  (IOKit re-enumeration to a new registry id, not-ready-endpoint window). These need either a
  *byte-level / chunked / fault-injecting* transport mock (a strictly richer thing than a
  frame-level `SimulatedDevice`) or are inherently hardware/OS artifacts.
- **Backend-seam, not message-transport**: B11 (presence vs transport-teardown), B-fwd
  (disconnect rule release) live at `DeviceBackend::subscribe_lifecycle` / `transport_alive`,
  one layer above `ADBMessageTransport`. A SimulatedDevice helps drive them only if paired with
  a lifecycle/death signal.
- **adbd-bug modelling**: B3b (NUL corrupts adbd's last CSV token) requires the sim to model an
  *adbd parsing bug*, not just the protocol — partial value (adboost-side banner assertion is
  already a regression lock).
- The existing `ScriptedTransport` (`persistent.rs:2861`) is a fixed fail-N script, not a
  stateful device; B13/B14/B15 already have/are-specced as classifier-level tests on it. A
  `SimulatedDevice` would generalize it into a stateful adbd state machine.
