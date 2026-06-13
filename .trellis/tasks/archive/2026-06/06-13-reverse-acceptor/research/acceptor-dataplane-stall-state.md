# Acceptor data-plane stall — current diagnostic state

- **Date**: 2026-06-13
- **Status**: command plane works; connection + handshake establish on real device;
  bulk data stalls. Bug isolated to the acceptor receive path.

## What works (verified on real device 90e594f3, Qualcomm SA8155P)

- `reverse:forward:` command: official `adb -P <port> reverse` succeeds (no protocol fault).
- Acceptor handshake: AOSP-verified correct (see adbd-clse-after-accept-diagnosis.md);
  adbd accepts our OKAY, links the peer.
- iperf3 reverse: connection + control negotiation flow; device "sender 9 MBytes"
  but host "receiver 0 Bytes" — bulk data does not actually transfer.
- **FORWARD path works fully** with iperf3 (sender 50 / receiver 44 Mbits/sec),
  using the SAME `bridge_session` + the `open_session` OPENER path. So the bridge
  and the receive/credit machinery are correct for the opener role.

## The isolated bug

For an iperf3 reverse, two device OPENs arrive (control + data conn). Trace of the
**data** session (device id=405, our local_id=3338749091):

```
reader: cmd=WRTE arg0=405 arg1=3338749091 payload_len=37   # adbd sends 37-byte cookie
... (2 seconds of nothing) ...
WARN: could not enqueue CLSE for session 3338749091 on drop: writer task gone
```

- We received exactly ONE 37-byte WRTE and sent NO crediting OKAY back.
- `poll_read_impl` sends a crediting OKAY whenever it reads a WRTE
  (persistent.rs ~1621). No OKAY was sent → `poll_read_impl` was never called for
  this session → the bridge's `u2c` (`tokio::io::copy(usb_read → host)`) never read
  the buffered 37-byte frame.
- adbd, having sent 37 bytes and received no OKAY (classic stop-and-wait, this
  device negotiated `window=None`), never sends more → device-side iperf3 buffers
  9 MB locally but only 37 bytes cross the wire.

## Key facts

- This device negotiated CLASSIC (no delayed_ack): session traces show
  `window=None`; the reverse OPEN arg1=0.
- `accept_device_open` registers the session BEFORE replying (correct ordering);
  the WRTE is classified to the session's data channel (`cmd=WRTE arg0=405
  arg1=<our_id>` confirms routing keyed on arg1=our_id).
- Registration happened ~0.6ms BEFORE the WRTE arrived, so it is not a
  register-after-frame race.

## Prime suspect

The bridge `u2c` task for the data session is spawned ("bridging" logged twice)
but its `usb_read` copy never drains the 37-byte frame. Hypotheses to check:
1. The accepted `MultiplexedSession`'s `data_rx` is not the receiver paired with
   the registered `data_tx` (some swap in `accept_device_open` vs `open_session`).
2. `tokio::io::copy(usb_read → client_write)` is blocked WRITING to the host
   (iperf3 server not reading the data socket until control negotiation completes),
   so it read the 37 bytes (which WOULD have sent an OKAY) — but we see NO OKAY, so
   it did not even read once. → points back to (1) or a poll wiring issue.
3. Compare field-by-field the session returned by `accept_device_open` vs
   `open_session`: data_rx/ack_rx pairing, shared.windowed, send_flow. The opener
   path works, so diffing is the fastest route.

## Repro

```
adb kill-server
./target/debug/adboost_cli server start --address 127.0.0.1:5039 --foreground &
iperf3 -s -p 55986 -1 &
adb -P 5039 -s <serial> reverse tcp:47153 tcp:55986
adb -P 5039 -s <serial> shell 'iperf3 -c 127.0.0.1 -p 47153 -t 2 2>&1 | tail -4'
```
Expect: sender > 0 AND receiver > 0 when fixed (currently receiver = 0).

Forward control (known-good comparison):
```
adb -P 5039 -s <serial> shell 'iperf3 -s -p 47200 -1 &'
adb -P 5039 -s <serial> forward tcp:55980 tcp:47200
iperf3 -c 127.0.0.1 -p 55980 -t 2     # works: sender+receiver both > 0
```
</content>
