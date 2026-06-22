# Status: IMPLEMENTED (awaiting user acceptance)

Committed on `main` (not pushed) as `ea88205`. Scheme B frame-atomic write: start-gate WriteTimeout is recoverable (writer_loop continues), mid-frame truncation stays fatal (poison + teardown). New RustADBError::WriteTimeout variant + map_write_status. Quality gate green (fmt, clippy default+usb, 243+141 tests).

NOTE: the real-world signal — through_server.reverse_iperf3 passing again — requires a manual hardware selftest; it cannot run in CI.
