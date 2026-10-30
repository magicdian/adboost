# Fix: receive-side CRC32 not skipped at `A_VERSION_SKIP_CHECKSUM` (regression from `46d674f`)

## Context

External bug report #2 (`/private/tmp/adboost-bugreport-2-crc32-not-skipped-at-skip-checksum-version.md`).
Regression introduced by `46d674f` (the bug #1 delayed_ack fix), observed at `b52ba4a`.

`46d674f` made `do_connect` send CNXN at `A_VERSION_SKIP_CHECKSUM` (0x01000001) when
`features.delayed_ack`. Per AOSP, **at >= `A_VERSION_SKIP_CHECKSUM` both ends send the
header `data_check` (crc) field as 0**, and the receiver never validates it. adboost's
`check_message_integrity()` still unconditionally compares `data_crc32`, so every inbound
payload-bearing frame from a skip-checksum peer fails with `Invalid integrity. Expected
CRC32 <n>, got 0`. The CNXN banner reply is the first such frame → handshake dies for
**all** delayed_ack-negotiating devices. Severity: Critical (worse than bug #1).

## Root cause (verified against AOSP source + parallel code map)

- `check_message_integrity()` (`adb_transport_message.rs:132-135`) ANDs a `magic` check with
  a `data_crc32` (byte-sum) check, version-unaware.
- Invoked unconditionally on every payload-bearing frame: `usb_transport.rs:372`,
  `tcp_transport.rs:194` — but only when `data_length != 0` (zero-payload control frames
  currently bypass the check entirely, including the magic check).

### AOSP adversarial verification (confirmed)
- AOSP **never** validates `data_check` on receive, in any version (the field is vestigial;
  `check_header()` validates only `magic` + `data_length`).
- `magic` (`command ^ 0xffffffff`) is **always** validated, version-independent.
- The adb "crc32" is a plain unsigned byte-sum (`calculate_apacket_checksum`) — adboost's
  `compute_crc32` matches exactly. It is a very weak check, redundant with USB hardware
  CRC16 / TCP checksum, and abandoned by modern AOSP.

### Latent bugs found beyond the reported CNXN failure (same root cause)
1. **CRITICAL — live-session data frames**: the persistent `reader_loop` reads every
   in-session frame through the same check. After delayed_ack is negotiated (only at skip
   version), the device's WRTE payloads and 4-byte windowed OKAY payloads carry `crc=0` →
   first one returns `InvalidIntegrity` → reader `break`s → **the whole connection and all
   multiplexed sessions die**. This is exactly the configuration delayed_ack was added for.
2. **HIGH — AUTH-phase frames** (AUTH token, post-signature response, final CNXN) are
   payload-bearing and rejected at skip version before any session opens.
3. **reverse / device-initiated OPEN frames**: non-empty payload, rejected pre-classification.
4. TCP path / `adb_message_device.rs` (hardcoded legacy 0x01000000): safe today, latent.

## Chosen fix — Magic-only integrity check (AOSP-faithful)

Drop the `data_crc32` comparison entirely; keep only the `magic` check. This matches AOSP
(which never validates `data_check`), is a single core change, and automatically covers
**all** paths (USB persistent, USB non-persistent, TCP, active sessions, AUTH, reverse, all
versions) with no per-transport version flag to thread through cloned reader transports.

Rationale for not keeping a byte-sum check on legacy connections: it is redundant
(USB/TCP already guarantee link integrity), extremely weak (trivial collisions), and the
reference implementation itself does not perform it. No real integrity is lost.

## Scope of changes

1. **`adb_transport_message.rs`** — `check_message_integrity()`: return
   `compute_magic(command) == magic` only. Update the doc comment to explain AOSP semantics
   (data_check is vestigial / sent as 0 at >= A_VERSION_SKIP_CHECKSUM; magic is the
   version-independent integrity field). Keep `compute_crc32` (still used on the send path
   by `try_new`) — do NOT remove it; just stop comparing it on receive.
2. **`usb_transport.rs` (~365-380)** — ensure the magic check runs for **all** frames, not
   only `data_length != 0`. Move/adjust the `check_message_integrity` call so zero-payload
   control frames are also magic-checked (closes the pre-existing magic-skip gap). Keep the
   `InvalidIntegrity` error shape; it now only fires on a magic mismatch.
3. **`tcp_transport.rs` (~193-198)** — same adjustment for symmetry: magic check on all
   frames.
4. **Send side**: leave `try_new` populating `data_crc32` as-is (AOSP receivers ignore it;
   sending a real byte-sum is harmless interop-wise). Zeroing outgoing crc at skip version
   is explicitly **out of scope** (cosmetic, not a correctness issue).

## Acceptance criteria

- [ ] `check_message_integrity()` validates magic only; no `data_crc32` comparison remains.
- [ ] A message with `data_crc32 = 0` and correct magic over a non-empty payload **passes**
      (the exact bug #2 scenario — regression-lock test).
- [ ] A message with a wrong/corrupted `magic` still **fails** (negative test).
- [ ] Zero-payload control frames are magic-checked too (no integrity bypass), and a
      zero-payload frame with crc=0 passes.
- [ ] Existing integrity-related unit tests updated to the new contract (any test asserting
      crc rejection is re-pointed to magic rejection or the new pass-with-crc0 behavior).
- [ ] `cargo build`, `cargo clippy --all-targets --features usb -- -D warnings`,
      `cargo test --features usb` all green.
- [ ] `InvalidIntegrity` error variant retained (still used for magic mismatch); no dead-code
      warnings.

## Out of scope
- Send-side crc zeroing at skip version (cosmetic).
- Replacing the byte-sum `compute_crc32` with a real CRC32 (not needed; would diverge from AOSP).
- Any change to `adb_message_device.rs` version literal (legacy is correct there).
