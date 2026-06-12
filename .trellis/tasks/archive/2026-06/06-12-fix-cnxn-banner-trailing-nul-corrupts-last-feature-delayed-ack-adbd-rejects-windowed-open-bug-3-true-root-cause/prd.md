# Fix: CNXN banner trailing NUL corrupts last feature → adbd rejects windowed OPEN (bug #3 TRUE root cause)

## Status: root cause CONFIRMED on a real Android-16 device (usbmon-equivalent raw-frame capture)

Empirically proven this session with a throwaway diagnostic harness against the
real device (`0e8d:201c`, serial `YTGUSCNFMFAIK7ZP`). This is the **true** root
cause of bug #3 — the earlier task only converted the hang into a fast-fail; this
is why the device rejected the windowed OPEN in the first place.

## Root cause (AOSP-verified + device-confirmed)

`DeviceFeatureSet::to_banner_string()` (`models/device_feature_set.rs:135-137`) builds:
```rust
format!("host::features={}\0", self.feature_names().join(","))   // <- trailing NUL in payload
```
The trailing `\0` is included in the CNXN payload (`try_new` sets `data_length` to
the byte length **including** the NUL). On the wire the feature CSV becomes
`shell_v2,delayed_ack\0`.

adbd parsing (AOSP `packages/modules/adb`, verified):
- `handle_new_connection` builds `banner(payload.begin(), payload.end())` and does
  NOT strip trailing NULs from the CNXN banner (`StripTrailingNulls` is applied
  only to the A_OPEN address, never the banner).
- `parse_banner` → `pieces[2]` = `"features=shell_v2,delayed_ack\0"` → value
  `"shell_v2,delayed_ack\0"`.
- `StringToFeatureSet` does `Split(value, ",")` with **no trimming** → tokens
  `["shell_v2", "delayed_ack\0"]`. The last token is `"delayed_ack\0"` (12 bytes
  incl. NUL) which `!= "delayed_ack"`.
- → `CanUseFeature(features_, kFeatureDelayedAck)` is false → `delayed_ack_ = false`
  → `SupportsDelayedAck()` returns false.

adboost then sends a windowed `OPEN(arg1 = INITIAL_DELAYED_ACK_BYTES = 32 MiB)`.
adbd's A_OPEN handler (`adb.cpp:507`):
```cpp
if (t->SupportsDelayedAck() != static_cast<bool>(send_bytes)) {  // false != true
    send_close(0, p->msg.arg0, t);   // A_CLSE(arg0=0, arg1=local_id) — the rejection
}
```
→ immediate `A_CLSE(0, local_id)`. The real `adb` host never hits this because its
`send_connect` payload contains **no NUL**.

`shell_v2` survived (and the connection appeared "up") because it is the FIRST CSV
token; only the LAST feature is corrupted by the trailing NUL — and `delayed_ack`
is last in our list.

## Device capture (this session)

```
# WINDOWED, banner WITH trailing NUL (current code) — REJECTED:
OPEN local_id=3244157924 ... delayed_ack=true window_grant=33554432
[raw] CLSE arg0=0x00000000 arg1=0xc15debe4   (arg1 == our local_id)
open_session FAILED in 1.76ms: OPEN rejected by device (CLSE)

# CLASSIC, banner WITHOUT delayed_ack, arg1=0 — SUCCESS (control):
OPEN ... delayed_ack=false window_grant=0
[raw] OKAY arg0=0x13 arg1=...  -> open_session SUCCEEDED

# WINDOWED, banner with trailing NUL REMOVED (proposed fix) — SUCCESS:
OPEN ... delayed_ack=true window_grant=33554432
[raw] OKAY arg0=0x14 arg1=... data_len=4 payload=[00,00,00,02]  (32MiB grant!)
open_session SUCCEEDED in 18ms
```
The single removed NUL byte flips windowed OPEN from rejected → accepted with the
expected 4-byte windowed OKAY grant. Decisive.

## Fix

Remove the trailing `\0` from `to_banner_string()` so the CNXN payload matches the
real adb host (`adb.cpp send_connect` appends no NUL):
```rust
pub fn to_banner_string(&self) -> String {
    format!("host::features={}", self.feature_names().join(","))
}
```
- Correct the doc comment (the existing one wrongly claims "The trailing NUL
  terminator matches what a real `adb` server sends" — it does NOT; AOSP
  `send_connect` sends no NUL).
- Update the existing banner tests that assert the trailing `\0`
  (`default_banner_advertises_shell_v2_and_delayed_ack`,
  `custom_banner_lists_enabled_features_in_order`,
  `single_feature_banner_has_no_trailing_comma`) to expect NO trailing NUL.
- At the CNXN construction site (`persistent.rs` ~541), fix the stale comment
  "The trailing NUL matches a real adb server."

### Why NOT instead keep the NUL and reorder features
Reordering so `delayed_ack` isn't last would only mask the bug (the NUL would then
corrupt whatever feature is last, e.g. a future-added feature). The NUL itself is
the defect — the real adb wire format has no NUL in the CNXN banner payload. Remove it.

### Interaction with other connect paths
- `adb_message_device.rs` builds its CNXN payload as `format!("host::{}\0", pkg)` —
  that is a DIFFERENT, non-feature banner (no `features=`) and is the legacy device
  type; its NUL does not corrupt any feature list. Out of scope, but note it: if it
  ever advertises features, it would have the same bug. (Document, don't change now
  unless trivially safe.)

## Acceptance criteria
- [ ] `to_banner_string()` produces NO trailing NUL; `host::features=shell_v2,delayed_ack`
      for the default set.
- [ ] Banner unit tests updated to assert no trailing NUL (and a NEW test explicitly
      asserting the banner does not end in `\0` and that the last feature token has no
      embedded NUL — regression lock for this exact bug).
- [ ] Doc comment on `to_banner_string` corrected (no false "matches real adb" claim);
      stale comment at the CNXN construction site corrected.
- [ ] `cargo build`, `cargo clippy --all-targets --features usb -- -D warnings`,
      `cargo clippy --all-targets -- -D warnings`, `cargo test --features usb`,
      `cargo fmt --check` all green.
- [ ] (Manual, already done this session) windowed OPEN succeeds on the real Android-16
      device with the fix; note this in the commit.

## Out of scope
- `adb_message_device.rs` legacy banner NUL (different banner, no features — note only).
- Any change to the windowed flow-control logic / OPEN arg1 (verified correct: 32 MiB
  in arg1 IS the AOSP protocol when delayed_ack is genuinely negotiated).
- The earlier bug#3 fast-fail fix (already shipped, complementary — keep it).
