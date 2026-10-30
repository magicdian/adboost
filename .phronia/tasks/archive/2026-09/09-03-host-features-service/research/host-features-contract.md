# Research: host-features wire contract & codebase touch points

## The adblib decision chain (why host-features gates everything)

```
SessionDeviceTracker.pickBestFormat()
  → AdbHostServices.hostFeatures() → wire host:host-features
  → FAIL → tracker never starts → AS device list empty (retry ~1s forever)
  → OKAY, no "devicetracker_proto_format" → LONG → host:track-devices-l (already OK, 46df633)
```

First-hand evidence from the 二期 report: after 46df633 the new WARN funnel logged
`unsupported adb service: host:host-features (peer: …)` every ~1s from AS.

## AOSP semantics (adb.cpp::handle_host_request)

- `host-features` (wire `host:host-features`, CLI `adb host-features`): SERVER-level.
  Reply = `FeatureSetToString(supported_features())`. Real adb's server set additionally
  contains `libusb` and `push_sync` — adboost must NOT fake these (honest-capability;
  adboost uses nusb, not libusb; push goes through its own sync path).
- bare `host:features` (pre-transport, CLI `adb features`): PER-TRANSPORT. AOSP does
  `acquire_one_transport` then returns `t->features()`. 0 devices →
  `FAIL no devices/emulators found`; >1 → `FAIL more than one device/emulator`.
  (`adb -s <s> features` sends the pinned `host-serial:<s>:features` form instead.)
- Reply framing for both: OKAY + `%04x` + csv payload (the standard host data-query shape).

## Codebase touch points (verified)

- `host_data_query_payload` (frontend.rs) is the single funnel for one-shot OKAY+framed
  data queries: add `"host-features" => Some(Ok(self.caps.features_csv()))`.
- Bare `features` arm currently `Some(Ok(self.caps.features_csv()))` (server semantics —
  the misalignment). Rework to the `get-state`/`get-serialno` bare-form pattern:
  `resolve_single_serial()` → `Ok(serial) => device_features_csv(&serial)` /
  `Err(reason) => Some(Err(reason))`. Byte-parity with `host-serial:<s>:features` then
  holds automatically (both produce `device_features_csv(serial)`).
- Post-transport `handle_client_impl` routes `host:features`/`host:version` after a
  transport switch; add `"host-features"` there too (server-level query, same class as
  `version` — a client that switched transport first should still get server features).
- `device_features_csv(serial)` = caps ∩ device banner (2s DEVICE_CAPS_TIMEOUT bound);
  unknown caps → conservative (drops shell_v2/sync_v2, keeps cmd/stat_v2/
  fixed_push_mkdir/apex). MockBackend returns entry capabilities instantly.
- Existing unit test to rework: `host_features_is_honest_minimal` (uses zero devices +
  bare `host:features` → currently OKAY; under R2 that becomes FAIL). Split into a
  `host-features` zero-device test + bare-features per-device tests.
- Selftest parity pattern to follow: `case_official_adb_get_state` (addr-only,
  non-destructive, once per run, REGRESSION marker on `unknown host service`).
  `adb host-features` / `adb features` both exist in modern platform-tools.
- No selftest case sends bare pre-transport `host:features` today (shell parity uses
  `-s`; connect/get-state cases don't touch features) — R2 is safe to land.

## Verification probes

```python
probe("host:host-features")  # FAIL before → OKAY + csv after (works with 0 devices)
probe("host:features")       # per-transport now: single device → device csv
```

Official client: `adb -P <port> host-features` (server set), `adb -P <port> features`
(bare, per-transport; ambiguity error under multi-device).
