//! Reverse port-forwarding data path (device-initiated connections).
//!
//! Reverse is the mirror of forward: the **device** binds the listener and, when
//! something connects to it, sends a device-initiated `A_OPEN(payload="<host
//! target>")` back over the transport. The host binds nothing — it only services
//! those inbound opens: validate the target against policy, accept the open into
//! a [`MultiplexedSession`] ([`PersistentUsbConnection::accept_device_open`]),
//! dial the host target, and bridge the two byte streams.
//!
//! [`ReverseState`] holds one device's reverse rule set + the single pump task
//! draining that connection's `incoming_opens` queue. It is created lazily on the
//! first `reverse:forward:` for a serial (a device that never uses reverse pays
//! nothing).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::Mutex;

use super::backend::ReversePolicy;
use crate::message_devices::message_commands::MessageCommand;
use crate::usb::{ADBTransportMessage, PersistentUsbConnection};

/// One device's reverse rules + pump. Shared (`Arc`) between the backend (which
/// edits rules) and the pump task (which reads them to authorize inbound opens).
pub(super) struct ReverseState {
    /// Active rules, keyed by the device-listen endpoint (`remote`, e.g.
    /// `tcp:5201`), value = host-connect target (`local`, e.g. `tcp:5201`).
    rules: Mutex<HashMap<String, String>>,
    /// How inbound device opens are authorized.
    policy: ReversePolicy,
    /// `true` once the pump task has been spawned (lazy, once per connection).
    pump_started: Mutex<bool>,
}

impl ReverseState {
    pub(super) fn new(policy: ReversePolicy) -> Self {
        Self {
            rules: Mutex::new(HashMap::new()),
            policy,
            pump_started: Mutex::new(false),
        }
    }

    /// Record a reverse rule (`remote` device-listen → `local` host target).
    pub(super) async fn add_rule(&self, remote: String, local: String) {
        self.rules.lock().await.insert(remote, local);
    }

    /// Remove the rule for device-listen endpoint `remote`. Returns whether one
    /// was present.
    pub(super) async fn remove_rule(&self, remote: &str) -> bool {
        self.rules.lock().await.remove(remote).is_some()
    }

    /// Remove every rule.
    pub(super) async fn clear_rules(&self) {
        self.rules.lock().await.clear();
    }

    /// Render the rules as `host:list-forward` body lines:
    /// `(reverse) <remote> <local>\n`, sorted by remote for stable output.
    pub(super) async fn list(&self) -> String {
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

    /// Spawn the inbound-open pump for `conn` once. Idempotent: subsequent calls
    /// are no-ops. The pump owns `conn`'s `incoming_opens` receiver and runs until
    /// the connection's reader stops (queue closes).
    pub(super) async fn ensure_pump(self: &Arc<Self>, conn: &Arc<PersistentUsbConnection>) {
        {
            let mut started = self.pump_started.lock().await;
            if *started {
                return;
            }
            *started = true;
        }
        let opens = match conn.incoming_opens() {
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
        let state = Arc::clone(self);
        let conn = Arc::clone(conn);
        tokio::spawn(async move {
            run_reverse_pump(state, conn, opens).await;
        });
    }

    /// Whether `target` (the inbound open's destination string) is allowed by
    /// this state's policy.
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

/// Drain device-initiated opens: authorize, accept, dial the host target, bridge.
async fn run_reverse_pump(
    state: Arc<ReverseState>,
    conn: Arc<PersistentUsbConnection>,
    mut opens: tokio::sync::mpsc::Receiver<ADBTransportMessage>,
) {
    while let Some(open_msg) = opens.recv().await {
        let device_id = open_msg.header().arg0();
        let target = decode_open_target(&open_msg);
        tracing::debug!("reverse pump: got device OPEN id={device_id} target={target:?}");

        if !state.target_allowed(&target).await {
            tracing::warn!("reverse: rejecting unconfigured inbound open to {target:?}");
            reject_open(&conn, device_id).await;
            continue;
        }

        // Accept the open into a session, then dial the host target and bridge.
        let session = match conn.accept_device_open(&open_msg).await {
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
                    super::frontend::bridge_session_public(host, session).await;
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
        let state = ReverseState::new(ReversePolicy::RejectUnconfigured);
        assert!(
            !state.target_allowed("tcp:5201").await,
            "no rule yet → reject"
        );
        state.add_rule("tcp:5201".into(), "tcp:5201".into()).await;
        assert!(
            state.target_allowed("tcp:5201").await,
            "rule maps to this host target → allow"
        );
        assert!(
            !state.target_allowed("tcp:9999").await,
            "different target still rejected"
        );
    }

    #[tokio::test]
    async fn allow_all_policy_accepts_anything() {
        let state = ReverseState::new(ReversePolicy::AllowAll);
        assert!(state.target_allowed("tcp:1").await);
        assert!(state.target_allowed("tcp:65535").await);
    }

    #[tokio::test]
    async fn custom_policy_consults_predicate() {
        let state = ReverseState::new(ReversePolicy::Custom(Arc::new(|t: &str| t == "tcp:7")));
        assert!(state.target_allowed("tcp:7").await);
        assert!(!state.target_allowed("tcp:8").await);
    }

    #[tokio::test]
    async fn list_is_sorted_with_reverse_marker() {
        let state = ReverseState::new(ReversePolicy::AllowAll);
        state.add_rule("tcp:5201".into(), "tcp:6000".into()).await;
        state.add_rule("tcp:5000".into(), "tcp:7000".into()).await;
        let body = state.list().await;
        assert_eq!(
            body,
            "(reverse) tcp:5000 tcp:7000\n(reverse) tcp:5201 tcp:6000\n"
        );
    }

    #[tokio::test]
    async fn remove_and_clear_rules() {
        let state = ReverseState::new(ReversePolicy::AllowAll);
        state.add_rule("tcp:1".into(), "tcp:2".into()).await;
        assert!(state.remove_rule("tcp:1").await);
        assert!(!state.remove_rule("tcp:1").await, "second remove → false");
        state.add_rule("tcp:3".into(), "tcp:4".into()).await;
        state.clear_rules().await;
        assert!(state.list().await.is_empty());
    }
}
