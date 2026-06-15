//! Composable reverse port-forwarding engine (device-initiated connections).
//!
//! Reverse is the mirror of forward: the **device** binds the listener and, when
//! something connects to it, sends a device-initiated `A_OPEN(payload="<host
//! target>")` back over the transport. The host binds nothing — it only services
//! those inbound opens: validate the target against policy, accept the open into
//! a [`MultiplexedSession`] ([`PersistentUsbConnection::accept_device_open`]),
//! dial the host target, and bridge the two byte streams via
//! [`bridge_tcp_session`][crate::usb::bridge_tcp_session].
//!
//! # When to use this
//!
//! [`ReverseEngine`] is the data path an **"acts-as-a-server"** backend uses when
//! it *is* the ADB server for a directly-attached device (the bundled
//! [`DefaultDeviceBackend`][crate::server::DefaultDeviceBackend] and downstreams like xdb
//! that hold their own [`PersistentUsbConnection`]). Such a backend has no other
//! adb server to service the device's inbound opens, so it must run the pump
//! itself — that is exactly what this engine does.
//!
//! A **proxy-style** backend (one sitting in front of a real adb server, e.g.
//! [`crate::proxy`]) must NOT use this engine: there the downstream adb server
//! owns the reverse data path, so the proxy only forwards the `reverse:` control
//! command and returns. Using `ReverseEngine` there would race that server for
//! the device's inbound opens.
//!
//! # Composing it (mirrors sync / `shell_v2`)
//!
//! The engine is **per-connection** and does not know about serials; a backend
//! serving multiple devices keeps one `Arc<ReverseEngine>` per device alongside
//! its serial→connection map and delegates the four reverse trait methods:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use adboost::usb::{PersistentUsbConnection, ReverseEngine, ReversePolicy};
//! # async fn demo(conn: Arc<PersistentUsbConnection>) -> adboost::Result<()> {
//! let engine = ReverseEngine::new(conn, ReversePolicy::default());
//! engine.open("tcp:5201", "tcp:5201").await?; // pump is ready before this returns
//! let listing = engine.list().await;
//! engine.remove("tcp:5201").await?;
//! engine.remove_all().await?;
//! # let _ = listing;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::Mutex;

use super::bridge_tcp_session;
use super::{ADBTransportMessage, MessageCommand, PersistentUsbConnection, ReversePolicy};
use crate::models::ADBLocalCommand;
use crate::{Result, RustADBError};

/// One device connection's reverse rule set + inbound-open pump.
///
/// Construct with [`ReverseEngine::new`] and drive it through the four intent
/// verbs ([`open`][Self::open] / [`remove`][Self::remove] /
/// [`remove_all`][Self::remove_all] / [`list`][Self::list]). The pump, policy
/// evaluation, control-command framing, and single-consumer `incoming_opens`
/// receiver are all private — their internals may change without affecting
/// callers.
///
/// # Guarantees
///
/// 1. **Pump readiness**: [`open`][Self::open] starts the inbound-open pump
///    *before* it asks the device to bind its listener, so the first inbound
///    connection after the listener appears is never dropped. Callers do not
///    issue a separate `start`.
/// 2. **Per-connection**: the engine owns exactly one
///    [`PersistentUsbConnection`] and holds no serial. Multi-device callers keep
///    one `Arc<ReverseEngine>` per device.
pub struct ReverseEngine {
    /// The device connection this engine services. Held so the engine owns the
    /// whole reverse path (control commands + pump) for one device.
    conn: Arc<PersistentUsbConnection>,
    /// The rule registry + authorization policy. Pure (connection-free) so its
    /// logic is unit-testable without hardware.
    rules: RuleSet,
    /// `true` once the pump task has been spawned (lazy, once per connection).
    pump_started: Mutex<bool>,
}

/// The reverse rule registry plus its authorization policy — the connection-free
/// half of [`ReverseEngine`], split out so the rule/policy logic stays
/// unit-testable without a live [`PersistentUsbConnection`].
struct RuleSet {
    /// Active rules, keyed by the device-listen endpoint (`remote`, e.g.
    /// `tcp:5201`), value = host-connect target (`local`, e.g. `tcp:5201`).
    rules: Mutex<HashMap<String, String>>,
    /// How inbound device opens are authorized.
    policy: ReversePolicy,
}

impl RuleSet {
    fn new(policy: ReversePolicy) -> Self {
        Self {
            rules: Mutex::new(HashMap::new()),
            policy,
        }
    }

    /// Record a reverse rule (`remote` device-listen → `local` host target).
    async fn add(&self, remote: String, local: String) {
        self.rules.lock().await.insert(remote, local);
    }

    /// Remove the rule for device-listen endpoint `remote`. Returns whether one
    /// was present.
    async fn remove(&self, remote: &str) -> bool {
        self.rules.lock().await.remove(remote).is_some()
    }

    /// Remove every rule.
    async fn clear(&self) {
        self.rules.lock().await.clear();
    }

    /// Render the rules as `host:list-forward` body lines:
    /// `(reverse) <remote> <local>\n`, sorted by remote for stable output.
    async fn list(&self) -> String {
        use std::fmt::Write as _;
        let rules = self.rules.lock().await;
        let mut keys: Vec<&String> = rules.keys().collect();
        keys.sort();
        let mut out = String::new();
        for remote in keys {
            let local = &rules[remote];
            let _ = writeln!(out, "(reverse) {remote} {local}");
        }
        out
    }

    /// Whether `target` (the inbound open's destination string) is allowed by the
    /// policy.
    async fn target_allowed(&self, target: &str) -> bool {
        match &self.policy {
            ReversePolicy::AllowAll => true,
            ReversePolicy::Custom(pred) => pred(target),
            ReversePolicy::RejectUnconfigured => {
                // Allowed iff some rule maps to this host target.
                self.rules
                    .lock()
                    .await
                    .values()
                    .any(|local| local == target)
            }
        }
    }
}

impl ReverseEngine {
    /// Build an engine for one device connection under `policy`. The pump is not
    /// started until the first [`open`][Self::open] (a device that never uses
    /// reverse pays nothing).
    #[must_use]
    pub fn new(conn: Arc<PersistentUsbConnection>, policy: ReversePolicy) -> Arc<Self> {
        Arc::new(Self {
            conn,
            rules: RuleSet::new(policy),
            pump_started: Mutex::new(false),
        })
    }

    /// Establish a reverse rule: ensure the inbound-open pump is running, ask the
    /// device to listen on `remote` (e.g. `tcp:5201`) and tunnel inbound
    /// connections back to the host target `local`, then record the rule so the
    /// pump authorizes opens to `local`.
    ///
    /// The pump is started *before* the device listener is configured, so an
    /// inbound connection arriving immediately after the listener binds is
    /// serviced rather than dropped (guarantee 1).
    pub async fn open(self: &Arc<Self>, remote: &str, local: &str) -> Result<()> {
        // Pump first: the device may accept an inbound connection the instant its
        // listener binds, and the pump must already own `incoming_opens` then.
        self.ensure_pump().await;
        let service = format!("reverse:forward:{remote};{local}");
        self.run_reverse_command(&service).await?;
        self.rules.add(remote.to_owned(), local.to_owned()).await;
        Ok(())
    }

    /// Remove the reverse rule whose device-listen endpoint is `remote`.
    pub async fn remove(&self, remote: &str) -> Result<()> {
        self.run_reverse_command(&format!("reverse:killforward:{remote}"))
            .await?;
        self.rules.remove(remote).await;
        Ok(())
    }

    /// Remove every reverse rule for this connection.
    pub async fn remove_all(&self) -> Result<()> {
        self.run_reverse_command("reverse:killforward-all").await?;
        self.rules.clear().await;
        Ok(())
    }

    /// Render the active rules as `host:list-forward` body lines:
    /// `(reverse) <remote> <local>\n`, sorted by remote for stable output.
    ///
    /// This engine's own registry is the source of truth for what *this* server
    /// set up (the device's list-forward would also include other clients'
    /// rules).
    pub async fn list(&self) -> String {
        self.rules.list().await
    }

    /// Spawn the inbound-open pump once. Idempotent: subsequent calls are no-ops.
    /// The pump owns the connection's `incoming_opens` receiver and runs until the
    /// connection's reader stops (queue closes).
    async fn ensure_pump(self: &Arc<Self>) {
        {
            let mut started = self.pump_started.lock().await;
            if *started {
                return;
            }
            *started = true;
        }
        let opens = match self.conn.incoming_opens() {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!("reverse pump: cannot take incoming_opens: {e}");
                // Reset so a later attempt can retry (e.g. if another consumer
                // released the receiver).
                *self.pump_started.lock().await = false;
                return;
            }
        };
        tracing::debug!("reverse pump: started");
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            run_reverse_pump(engine, opens).await;
        });
    }

    /// Open a `reverse:*` control service on the device and consume its reply,
    /// returning `Ok(())` on the device's `OKAY` or an error carrying its `FAIL`
    /// reason. The reply rides the opened session's byte stream (the service is a
    /// normal local service from the connection's point of view).
    async fn run_reverse_command(&self, service: &str) -> Result<()> {
        use tokio::io::AsyncReadExt;

        let mut session = self
            .conn
            .open_session(&ADBLocalCommand::Raw(service.to_owned()))
            .await?;
        // adbd replies with a smartsocket status: `OKAY` or `FAIL<%04x><reason>`.
        let mut head = [0u8; 4];
        match session.read_exact(&mut head).await {
            Ok(_) => {}
            // No reply bytes (some adbd builds just accept the stream) → treat the
            // successful OPEN as success.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(RustADBError::IOError(e)),
        }
        match &head {
            b"OKAY" => Ok(()),
            b"FAIL" => {
                // Length-prefixed reason (4 ASCII hex + body); best-effort decode.
                let mut len_buf = [0u8; 4];
                let reason = if session.read_exact(&mut len_buf).await.is_ok() {
                    let len =
                        usize::from_str_radix(std::str::from_utf8(&len_buf).unwrap_or("0000"), 16)
                            .unwrap_or(0);
                    let mut body = vec![0u8; len];
                    if session.read_exact(&mut body).await.is_ok() {
                        String::from_utf8_lossy(&body).into_owned()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                Err(RustADBError::ADBRequestFailed(format!(
                    "reverse command failed: {reason}"
                )))
            }
            other => Err(RustADBError::ADBRequestFailed(format!(
                "reverse command: unexpected reply {:?}",
                String::from_utf8_lossy(other)
            ))),
        }
    }
}

/// Drain device-initiated opens: authorize, accept, dial the host target, bridge.
async fn run_reverse_pump(
    engine: Arc<ReverseEngine>,
    mut opens: tokio::sync::mpsc::Receiver<ADBTransportMessage>,
) {
    while let Some(open_msg) = opens.recv().await {
        let device_id = open_msg.header().arg0();
        let target = decode_open_target(&open_msg);
        tracing::debug!("reverse pump: got device OPEN id={device_id} target={target:?}");

        if !engine.rules.target_allowed(&target).await {
            tracing::warn!("reverse: rejecting unconfigured inbound open to {target:?}");
            reject_open(&engine.conn, device_id).await;
            continue;
        }

        // Accept the open into a session, then dial the host target and bridge.
        let session = match engine.conn.accept_device_open(&open_msg).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("reverse: accept_device_open failed: {e}");
                continue;
            }
        };
        let Some(addr) = parse_tcp_target(&target) else {
            tracing::warn!("reverse: unsupported target {target:?} (only tcp: supported)");
            // Drop the session → device sees a CLSE on teardown.
            drop(session);
            continue;
        };
        tracing::debug!("reverse pump: accepted open id={device_id}, dialing {addr}");
        tokio::spawn(async move {
            match TcpStream::connect(addr).await {
                Ok(host) => {
                    if let Err(e) = host.set_nodelay(true) {
                        tracing::debug!("reverse: set_nodelay failed: {e}");
                    }
                    tracing::debug!("reverse: dialed {addr}, bridging");
                    bridge_tcp_session(host, session).await;
                    tracing::debug!("reverse: bridge for {addr} ended");
                }
                Err(e) => {
                    tracing::debug!("reverse: dial {addr} failed: {e}");
                    // Dropping the session tears the device stream down.
                }
            }
        });
    }
    tracing::debug!("reverse pump ended (connection closed)");
}

/// Decode the NUL-terminated destination string from a device-initiated OPEN.
fn decode_open_target(open_msg: &ADBTransportMessage) -> String {
    let payload = open_msg.payload();
    let end = payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(payload.len());
    String::from_utf8_lossy(&payload[..end]).into_owned()
}

/// Send `A_CLSE(0, device_id)` to reject a device-initiated open.
async fn reject_open(conn: &PersistentUsbConnection, device_id: u32) {
    if let Ok(clse) = ADBTransportMessage::try_new(MessageCommand::Clse, 0, device_id, &[])
        && let Err(e) = conn.send_raw(clse).await
    {
        tracing::debug!("reverse: failed to send reject CLSE: {e}");
    }
}

/// Parse a `tcp:<port>` reverse target into a loopback `SocketAddr`. `None` for
/// any non-tcp scheme (only tcp targets are bridged, mirroring forward).
fn parse_tcp_target(target: &str) -> Option<SocketAddr> {
    let port: u16 = target.strip_prefix("tcp:")?.parse().ok()?;
    Some(SocketAddr::from(([127, 0, 0, 1], port)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_with_payload(payload: &[u8]) -> ADBTransportMessage {
        ADBTransportMessage::try_new(MessageCommand::Open, 99, 0, payload).expect("msg")
    }

    // The rule registry + authorization policy are factored into the
    // connection-free `RuleSet`, so the full rule/policy behavior is tested here
    // without needing a (hardware-only) `PersistentUsbConnection`. The pure wire
    // helpers (`decode_open_target` / `parse_tcp_target`) are tested directly.

    #[test]
    fn decode_open_target_strips_nul() {
        let m = open_with_payload(b"tcp:5201\0");
        assert_eq!(decode_open_target(&m), "tcp:5201");
    }

    #[test]
    fn decode_open_target_without_nul() {
        let m = open_with_payload(b"tcp:9000");
        assert_eq!(decode_open_target(&m), "tcp:9000");
    }

    #[test]
    fn parse_tcp_target_ok_and_reject() {
        assert_eq!(
            parse_tcp_target("tcp:5201"),
            Some(SocketAddr::from(([127, 0, 0, 1], 5201)))
        );
        assert!(parse_tcp_target("localabstract:foo").is_none());
        assert!(parse_tcp_target("tcp:notaport").is_none());
    }

    #[tokio::test]
    async fn reject_unconfigured_policy_blocks_unknown_target() {
        let rules = RuleSet::new(ReversePolicy::RejectUnconfigured);
        assert!(
            !rules.target_allowed("tcp:5201").await,
            "no rule yet → reject"
        );
        rules.add("tcp:5201".into(), "tcp:5201".into()).await;
        assert!(
            rules.target_allowed("tcp:5201").await,
            "rule maps to this host target → allow"
        );
        assert!(
            !rules.target_allowed("tcp:9999").await,
            "different target still rejected"
        );
    }

    #[tokio::test]
    async fn allow_all_policy_accepts_anything() {
        let rules = RuleSet::new(ReversePolicy::AllowAll);
        assert!(rules.target_allowed("tcp:1").await);
        assert!(rules.target_allowed("tcp:65535").await);
    }

    #[tokio::test]
    async fn custom_policy_consults_predicate() {
        let rules = RuleSet::new(ReversePolicy::Custom(Arc::new(|t: &str| t == "tcp:7")));
        assert!(rules.target_allowed("tcp:7").await);
        assert!(!rules.target_allowed("tcp:8").await);
    }

    #[tokio::test]
    async fn list_is_sorted_with_reverse_marker() {
        let rules = RuleSet::new(ReversePolicy::AllowAll);
        rules.add("tcp:5201".into(), "tcp:6000".into()).await;
        rules.add("tcp:5000".into(), "tcp:7000".into()).await;
        let body = rules.list().await;
        assert_eq!(
            body,
            "(reverse) tcp:5000 tcp:7000\n(reverse) tcp:5201 tcp:6000\n"
        );
    }

    #[tokio::test]
    async fn remove_and_clear_rules() {
        let rules = RuleSet::new(ReversePolicy::AllowAll);
        rules.add("tcp:1".into(), "tcp:2".into()).await;
        assert!(rules.remove("tcp:1").await);
        assert!(!rules.remove("tcp:1").await, "second remove → false");
        rules.add("tcp:3".into(), "tcp:4".into()).await;
        rules.clear().await;
        assert!(rules.list().await.is_empty());
    }
}
