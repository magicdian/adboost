# Neutral Module Design Review — `framed_stream` + `StreamTransport`

- **Repository:** `magicdian/adboost`
- **Working tree:** uncommitted changes on `main` (feature implemented and verified locally)
- **Purpose of this document:** hand a neutral, implementation-independent evaluation of the
  module split to another session/engineer. It states what changed, why it has value, what the
  risks are, and which decisions still deserve independent review. It does **not** assume the
  change will be kept.
- **Related input:** `/private/tmp/adboost-streamtransport-feature-request.md` (the downstream
  `xpeng-debug-bridge` feature request)
- **Status:** implemented, all tests green; no commit made.

---

## 1. Executive summary

The change adds two things:

1. **`StreamTransport`** — an `ADBMessageTransport` over any already-connected full-duplex byte
   stream (`AsyncRead + AsyncWrite + Unpin + Send + 'static`), plus
   `PersistentConnection::from_stream` / `from_stream_with_features`. This lets the existing,
   proven CNXN/AUTH/multiplexer state machine run over a stream that is not a `TcpStream`
   (another ADB device's local-service pipe, a `MultiplexedSession`, a Unix socket, a TLS tunnel,
   `tokio::io::duplex`).
2. **`framed_stream`** — a transport-neutral async framing core (`FrameReader`,
   `write_frame_atomic`, `FramedStream`) extracted from `TcpTransport`, which now also uses it.

The second item is the architectural point: without it, `StreamTransport` would have had to copy
the TCP framing, split-lock topology, per-read timeout loop, and write-poisoning policy. The
change removes roughly 440 lines of transport-specific framing code and leaves one place where
those invariants live.

**No wire-protocol behavior changes. No new dependencies. No new cargo features.**

---

## 2. Before / after

### Before

```
adb_transport_message        wire types
framed_read                  sans-io FrameReadBuffer (cancel-safe parser)

tcp/tcp_transport.rs
  ├── CurrentConnection { Tcp | Tls }
  ├── private FrameReader<ReadHalf<CurrentConnection>>   ← hard-coded type
  ├── private write_frame_atomic(...)                    ← private
  ├── private WRITE_COMPLETION_TIMEOUT / READ_CHUNK_LEN
  └── private read/write guards + poison policy

usb/usb_transport.rs          separate endpoint model (unchanged)
usb/persistent.rs             PersistentConnection<T>, T: ADBMessageTransport
```

Consequence: a downstream crate could implement `ADBMessageTransport`, but could not reuse the
framing/locking invariants — it would have to copy them (explicitly forbidden by the downstream
spec, because the framing invariants are load-bearing for the whole multiplexed connection).

### After

```
adb_transport_message        wire types (unchanged)
framed_read                  sans-io FrameReadBuffer (unchanged)

framed_stream                FrameReader<R>          (generic)
  ├── write_frame_atomic<W: AsyncWrite>
  ├── FramedStream<S>        split halves, independent locks, poison policy
  └── READ_CHUNK_LEN / WRITE_COMPLETION_TIMEOUT / not_connected()

tcp/tcp_transport.rs         connect + TLS upgrade + CurrentConnection
                             read/write delegate to FramedStream<CurrentConnection>

stream/stream_transport.rs   StreamTransport over Box<dyn AsyncStream + ...>
                             connect = no-op, disconnect = shutdown write half,
                             upgrade_connection = UpgradeError (STLS refused)

usb/usb_transport.rs         unchanged
usb/persistent.rs            unchanged + from_stream(_with_features)
```

The layering rule is now explicit in the spec:
*“A new `AsyncRead + AsyncWrite` transport must use `FramedStream`, not re-derive the lock
topology, per-read timeout handling, or write poisoning.”*

---

## 3. Public API surface (the part that matters for stability)

```rust
// adboost::message_devices::stream (re-exported at crate root)
pub trait AsyncStream: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite + ?Sized> AsyncStream for T {}

pub type StreamConnection = Box<dyn AsyncStream + Unpin + Send + 'static>;

pub struct StreamTransport { /* private */ }

impl StreamTransport {
    pub fn new<S>(stream: S, label: impl Into<String>) -> Self
    where S: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    pub fn from_tcp_stream(stream: tokio::net::TcpStream, label: impl Into<String>)
        -> Result<Self>;              // sets TCP_NODELAY; propagates setsockopt failure

    pub fn label(&self) -> &str;
}

impl Clone for StreamTransport;
impl Debug for StreamTransport;        // label only, never payload bytes
impl ADBTransport for StreamTransport;
impl ADBMessageTransport for StreamTransport;

// adboost::usb::PersistentConnection (existing type; gated by `usb`)
impl PersistentConnection<StreamTransport> {
    pub async fn from_stream<S>(stream: S, private_key_path: Option<PathBuf>) -> Result<Self>
    where S: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    pub async fn from_stream_with_features<S>(
        stream: S,
        private_key_path: Option<PathBuf>,
        features: DeviceFeatureSet,
    ) -> Result<Self>
    where S: AsyncRead + AsyncWrite + Unpin + Send + 'static;
}
```

`from_stream` uses the default diagnostic label `"stream"`. For a custom label:
`PersistentConnection::new(StreamTransport::new(stream, label), key)`.

### Semantics that callers must know

| Situation | Behavior |
|---|---|
| `ADBTransport::connect()` | no-op — `StreamTransport::new` means “already connected” |
| `disconnect()` | shuts down the **write** half; the read side belongs to the caller |
| device sends `STLS` | `UpgradeError("STLS is not supported over stream transport \"<label>\"")`; never continues in plaintext |
| read idle | `ReadTimeout`, only at a frame boundary; partial bytes stay buffered |
| write stall before first byte | recoverable `WriteTimeout`; write half not poisoned |
| write failure after first byte | write half poisoned; later writes return `NotConnected` |
| reconnection | caller’s responsibility (no reconnect policy in the transport) |
| diagnostics | label appears in error/trace text, never payload bytes |

---

## 4. Value

1. **Enables nested/layered ADB without reimplementing the protocol.** The concrete driver is a
   T-Box reached through a base Android device’s relay port: open `tcp:6668` as a raw byte pipe,
   then speak the ADB *device* protocol over it. That removes host-side `adb forward`, fixed host
   ports, `adb connect` state, and port collisions.
2. **One copy of the invariants, not two.** Cancel-safe reads, frame-atomic writes, independent
   read/write locks (interactive-shell latency), and write poisoning now live in
   `framed_stream`. `TcpTransport` no longer owns them privately.
3. **Smaller, safer future transports.** Any future byte-stream transport (Unix socket, TLS,
   in-process relay) is a ~100-line adapter instead of a ~450-line copy.
4. **Testability without hardware.** `tokio::io::duplex` drives the real handshake, shell
   session, and two-session demux in-process. The new tests are deterministic except for RSA key
   generation.
5. **No adoption tax.** Additive API; existing `TcpTransport` / `PersistentTcpConnection` /
   `PersistentUsbConnection` signatures and behavior are unchanged. No new crate dependency, no
   MSRV change.

---

## 5. Risks and mitigations

### R1 — API surface growth / naming lock-in (medium)

New public items: `AsyncStream`, `StreamConnection`, `StreamTransport`,
`PersistentConnection::{from_stream, from_stream_with_features}`. Once released these names are
semver-relevant.

- Mitigation: the names match the feature request’s proposal; `StreamConnection` is a type alias,
  so the erased-box representation is not part of any signature. `StreamTransport` is
  `#[non_exhaustive]`-free but has private fields, so fields can evolve.
- Evaluation question: is `AsyncStream` the name the downstream wants long-term, or would
  `AdbStream` / `ByteStream` age better? Renaming is cheap now, expensive after a release.

### R2 — Dynamic dispatch / one extra Box (low)

`StreamTransport` erases the concrete stream into a `Box<dyn AsyncStream + Unpin + Send>`.
Every read/write pays one virtual dispatch, and the stream is heap-allocated.

- Mitigation: the transport is I/O-bound; a virtual call is noise next to a syscall or a serial
  transfer. `TcpTransport` is not erased and keeps its concrete enum.
- Evaluation question: only relevant if a caller needs `StreamTransport` in a zero-alloc,
  non-`Send` context — out of scope today.

### R3 — STLS is refused rather than supported (accepted limitation)

A modern adbd behind a wrapped stream that requests STLS cannot be upgraded in place.

- Rationale: answering STLS and continuing cleartext would be a protocol lie. The caller can
  establish TLS before wrapping; then the device does not request STLS.
- Risk: a future Android device behind a relay that *requires* STLS is unusable through this
  transport.
- Evaluation question: if that driver appears, add a caller-supplied upgrade hook rather than
  changing this default.

### R4 — `PersistentConnection` is still physically under the `usb` feature (pre-existing debt)

`StreamTransport` is always compiled, but `PersistentConnection::from_stream` requires
`--features usb`, because the multiplexer lives in `message_devices/usb/persistent.rs` and
depends on `usb::flow_control`, `usb::sync_session`, `usb::shell_v2_session`.

- This change does **not** worsen it; it avoids the FR’s suggestion to gate the *transport*
  module on `usb`, which would have been the wrong layering.
- Moving the multiplexer to a transport-neutral module is a separate, larger refactor (≈4000
  lines, re-export churn). Recommended follow-up, not part of this change.
- Evaluation question: does any consumer need `from_stream` without `usb`? If yes, schedule that
  refactor explicitly; do not duplicate the multiplexer.

### R5 — Write completion deadline is a fixed 10 s constant (pre-existing behavior)

`write_frame_atomic` uses a 10 s mid-frame completion deadline. Through the public transport API
there is no injection point, so a test of “mid-frame stall is fatal” would take 10 s.
Tests therefore exercise the fatal path with an immediate mock error (rather than a timer) and
exercise the timer logic in unit tests with an explicit short completion deadline.

- Risk: no change versus previous TCP behavior. A future configurable timeout is an additive
  option, not required now.
- Evaluation question: should the transport expose timeout knobs? Recommend no until a consumer
  asks.

### R6 — Test cost and flakiness (medium, test-only)

The two integration tests construct a real `PersistentConnection`, which generates a fresh
2048-bit RSA key when the supplied path does not exist. Key generation is CPU-bound and can take
seconds in a debug build under a fully parallel test run. Handshake “hang guards” are therefore
60 s; they are not correctness assertions.

- The deterministic tests are the framing ones (`duplex` + partial frames), which do not need a
  key.
- Evaluation question: if CI hardware is slow, replace the per-test key generation with one
  committed test key fixture. That is a test-only change.

### R7 — `tokio::io::split` semantics (low, inherited)

Both TCP and stream use tokio’s generic `io::split`, whose halves allow concurrent read/write
while sharing one inner poll lock. The TCP regression test (`read_does_not_block_concurrent_write`)
locks this in; the new transport has an equivalent test.

- Evaluation question: if a future tokio version changes `split` semantics, both tests fail
  together — desirable.

### R8 — Behavioral drift during the TCP refactor (low)

`TcpTransport`’s framing code moved. The diff is mostly deletion; the surviving TCP tests
(split-frame across timeout, concurrent write, poison-only-on-fatal, nodelay) all pass, and error
message text is preserved (`"TCP ..."` via the stream label).

- Residual risk: `upgrade_connection` now replaces the `FramedStream` instead of restoring halves
  into the same `Arc` guards on the “already TLS” path. No clones exist at handshake time, so
  this is not observable today.
- Evaluation question: reviewers should confirm no future caller runs `upgrade_connection` with
  borrowed clones.

### R9 — Error-variant coupling (low)

The new public docs promise `ReadTimeout`, `WriteTimeout`, `UpgradeError`, and `NotConnected`
semantics. These are existing variants; no new error variant was added. Any future change to
these variants is already semver-visible today.

---

## 6. Compatibility / rollout / rollback

- **Wire behavior:** byte-identical for TCP and USB. No changes to `CNXN`, `AUTH`, `OPEN`,
  `WRTE`, `CLSE`, `OKAY`, `STLS`, sync, or shell-v2 framing.
- **Source compatibility:** additive. Existing symbols unchanged.
- **Dependencies / MSRV / features:** none changed. (`let ... else` and
  `finish_non_exhaustive` are well below MSRV 1.88.)
- **Downstream rollout (xdb):** bump the pinned adboost rev, refresh `Cargo.lock`, replace the
  temporary in-daemon loopback bridge with `StreamTransport::from_tcp_stream` or
  `PersistentConnection::from_stream`, delete the temporary port-forward logic.
- **Rollback:** revert the working-tree changes. The only cross-file coupling is
  `persistent.rs` → `stream::StreamTransport`; removing the two constructors and the module
  leaves the previously-private TCP framing intact in git history (or can be restored from the
  pre-change commit).

---

## 7. Evidence (local run)

```
cargo test -p adboost --all-features        → 426 passed, 0 failed (twice)
cargo test -p adboost                       → 114 passed, 0 failed (+ doctests)
cargo build -p adboost_cli                  → ok
cargo fmt --all --check                     → ok
cargo clippy --all-features --all-targets -- -D warnings
    → changed files: zero findings
    → repository: fails on untouched files (server/, usb/sim/, emulator/) due to
      clippy 1.98 lints that postdate the repository’s 1.88 MSRV baseline;
      pre-existing, not introduced by this change
```

New tests (16 total):

`framed_stream` (5): start-gate recoverable; mid-frame stall fatal; whole frame completes;
start-gate timeout does not poison; mid-frame error poisons.

`stream` (11): legacy handshake + shell echo over `duplex`; partial payload across timeout;
partial header across timeout; concurrent read/write; start-gate recoverable through the
transport; mid-frame error poisons through the transport; two sessions demultiplexed by local id;
oversize `data_length`; unknown command; bad magic; STLS refused with the label.

---

## 8. Decision checklist for another session

Use this as the review agenda; each item is intentionally answerable independently.

1. **Keep the `framed_stream` extraction?** If yes, `TcpTransport` must stay on it (no second
   copy). If no, `StreamTransport` would need to duplicate framing — reject that combination.
2. **Accept the public names** (`AsyncStream`, `StreamConnection`, `StreamTransport`,
   `from_stream`, `from_stream_with_features`) before the next release.
3. **Accept the STLS refusal** as the long-term contract for wrapped streams.
4. **Accept `usb`-gated `from_stream`** for now, with the multiplexer move tracked separately.
5. **Decide on a test-key fixture** if CI keygen latency becomes a problem.
6. **Confirm the downstream integration shape**: `MultiplexedSession` vs `TcpStream` as the
   stream source, and whether a caller-provided label is sufficient for diagnostics.
7. **Confirm no reconnect policy is wanted** inside the transport.

---

## 9. File inventory (working tree)

Modified:
- `adboost/src/message_devices/mod.rs` — module declaration + re-exports
- `adboost/src/message_devices/tcp/tcp_transport.rs` — delegate framing to `FramedStream`
- `adboost/src/message_devices/usb/persistent.rs` — `from_stream(_with_features)`
- `.phronia/spec/backend/directory-structure.md` — module map + shared-driver rule
- `.phronia/spec/backend/adb-wire-protocol-contract.md` — lock-invariant ownership, STLS case
- `README.md` — module-role row

Added:
- `adboost/src/message_devices/framed_stream.rs`
- `adboost/src/message_devices/stream/mod.rs`
- `adboost/src/message_devices/stream/stream_transport.rs`
- `adboost/src/message_devices/stream/README.md`
