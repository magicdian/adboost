//! Host-side `forward` rule parsing and registry.
//!
//! This module owns the *I/O-free* parsing of the `host:forward` family service
//! strings and the *server-global* registry of active forward rules. The actual
//! `TcpListener` bind + per-connection bridge lives in [`super::frontend`]
//! (it needs the generic [`DeviceBackend`] to open device sessions); this module
//! only parses requests and tracks live rules so they can be listed and killed.
//!
//! # What a forward rule is
//!
//! `host:forward:[norebind:]<local>;<remote>` asks the server to bind a
//! host-side TCP listener on `<local>` and, for every inbound connection, open
//! `<remote>` *on the selected device* and bridge the two. Only `tcp:<port>`
//! endpoints are supported (the bundled backend opens device services via
//! `tcp:` — `localabstract:` / `jdwp:` etc. are rejected with a clear FAIL).
//!
//! [`DeviceBackend`]: super::DeviceBackend

use std::collections::HashMap;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// A parsed `host:forward:...` request (the part after `forward:`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ForwardRequest {
    /// `norebind:` was present — refuse to replace an existing rule for `local_port`.
    pub norebind: bool,
    /// `true` when the requested local endpoint was `tcp:0` (OS auto-assign);
    /// the resolved port is then echoed back to the client.
    pub local_is_zero: bool,
    /// Host-side local TCP port to listen on (`0` ⇒ auto-assign).
    pub local_port: u16,
    /// Device-side remote TCP port to connect to per inbound connection.
    pub remote_port: u16,
}

/// Parse the argument of `host:forward:<arg>` (everything after `forward:`).
///
/// Grammar: `[norebind:]tcp:<local>;tcp:<remote>`. Only `tcp:` endpoints are
/// supported. Returns the AOSP-style failure reason string on any error (the
/// caller frames it into a single `FAIL`).
pub(super) fn parse_forward(arg: &str) -> Result<ForwardRequest, String> {
    let (norebind, rest) = match arg.strip_prefix("norebind:") {
        Some(r) => (true, r),
        None => (false, arg),
    };
    let Some((local, remote)) = rest.split_once(';') else {
        return Err(format!("bad forward: {arg}"));
    };
    let local_port = parse_tcp_endpoint(local).ok_or_else(|| format!("bad forward: {arg}"))?;
    let remote_port = parse_tcp_endpoint(remote).ok_or_else(|| format!("bad forward: {arg}"))?;
    Ok(ForwardRequest {
        norebind,
        local_is_zero: local_port == 0,
        local_port,
        remote_port,
    })
}

/// Parse the local endpoint of a `host:killforward:<local>` request — only the
/// local side is given. Returns the local tcp port or an AOSP-style reason.
pub(super) fn parse_killforward(arg: &str) -> Result<u16, String> {
    parse_tcp_endpoint(arg).ok_or_else(|| format!("bad killforward: {arg}"))
}

/// Parse a single `tcp:<port>` endpoint to its port. `None` for any non-`tcp:`
/// scheme or unparseable port (the host protocol only bridges tcp here).
fn parse_tcp_endpoint(s: &str) -> Option<u16> {
    let port_str = s.strip_prefix("tcp:")?;
    port_str.parse::<u16>().ok()
}

/// Metadata for one live forward rule, keyed in the registry by its *resolved*
/// local port. Dropping/aborting `task` tears down the host listener (freeing
/// the port) and stops accepting new bridged connections.
struct ForwardRule {
    remote_port: u16,
    serial: String,
    /// The host listener accept loop. Aborted on remove/remove-all.
    task: JoinHandle<()>,
}

/// Server-global registry of active forward rules, keyed by resolved local port.
///
/// Shared across every client connection (forward rules outlive the socket that
/// created them), so the frontend holds it behind an `Arc`.
#[derive(Default)]
pub(super) struct ForwardRegistry {
    rules: Mutex<HashMap<u16, ForwardRule>>,
}

impl ForwardRegistry {
    /// Whether a rule already exists for `local_port` (drives `norebind`).
    pub(super) async fn contains(&self, local_port: u16) -> bool {
        self.rules.lock().await.contains_key(&local_port)
    }

    /// Register a rule for `local_port`. If one already exists it is aborted and
    /// replaced (AOSP rebind semantics; `norebind` is enforced by the caller
    /// *before* binding).
    pub(super) async fn insert(
        &self,
        local_port: u16,
        remote_port: u16,
        serial: String,
        task: JoinHandle<()>,
    ) {
        let mut rules = self.rules.lock().await;
        if let Some(old) = rules.insert(
            local_port,
            ForwardRule {
                remote_port,
                serial,
                task,
            },
        ) {
            old.task.abort();
        }
    }

    /// Remove the rule for `local_port`, aborting its listener. Returns `true`
    /// if a rule was present.
    pub(super) async fn remove(&self, local_port: u16) -> bool {
        if let Some(rule) = self.rules.lock().await.remove(&local_port) {
            rule.task.abort();
            true
        } else {
            false
        }
    }

    /// Remove every rule, aborting all listeners.
    pub(super) async fn remove_all(&self) {
        let mut rules = self.rules.lock().await;
        for (_, rule) in rules.drain() {
            rule.task.abort();
        }
    }

    /// Remove every rule registered for `serial`, aborting their listeners.
    /// Returns the number of rules removed (`0` if the serial had none).
    ///
    /// This is the forward half of the disconnect-cleanup path: when a device's
    /// transport goes away, the frontend drops exactly the listeners bound for
    /// that serial, leaving other devices' forwards untouched (a server-global
    /// registry keys rules by local port, so removal must filter by serial).
    pub(super) async fn remove_by_serial(&self, serial: &str) -> usize {
        let mut rules = self.rules.lock().await;
        // Two-step: collect the matching local ports, then remove+abort each.
        // (Can't abort while holding a `&` into the map mid-`retain`.)
        let ports: Vec<u16> = rules
            .iter()
            .filter(|(_, rule)| rule.serial == serial)
            .map(|(port, _)| *port)
            .collect();
        for port in &ports {
            if let Some(rule) = rules.remove(port) {
                rule.task.abort();
            }
        }
        ports.len()
    }

    /// The distinct device serials that currently have at least one forward
    /// rule. Used by [`ForwardHandle::release_all`](super::ForwardHandle) to fan
    /// reverse cleanup out per serial (reverse rules are keyed by serial in the
    /// backend, not by local port).
    pub(super) async fn serials(&self) -> Vec<String> {
        let rules = self.rules.lock().await;
        let mut serials: Vec<String> = rules.values().map(|r| r.serial.clone()).collect();
        serials.sort_unstable();
        serials.dedup();
        serials
    }

    /// Render the `host:list-forward` body: one `\n`-terminated line per rule,
    /// `<serial> tcp:<local> tcp:<remote>\n`, sorted by local port for a stable
    /// listing.
    pub(super) async fn list(&self) -> String {
        use std::fmt::Write as _;
        let rules = self.rules.lock().await;
        let mut ports: Vec<u16> = rules.keys().copied().collect();
        ports.sort_unstable();
        let mut out = String::new();
        for port in ports {
            let rule = &rules[&port];
            let serial = &rule.serial;
            let remote = rule.remote_port;
            let _ = writeln!(out, "{serial} tcp:{port} tcp:{remote}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forward_tcp_pair() {
        let r = parse_forward("tcp:5555;tcp:5556").expect("valid forward");
        assert_eq!(
            r,
            ForwardRequest {
                norebind: false,
                local_is_zero: false,
                local_port: 5555,
                remote_port: 5556,
            }
        );
    }

    #[test]
    fn parse_forward_norebind_flag() {
        let r = parse_forward("norebind:tcp:7000;tcp:8000").expect("valid");
        assert!(r.norebind, "norebind: prefix must set the flag");
        assert_eq!(r.local_port, 7000);
        assert_eq!(r.remote_port, 8000);
    }

    #[test]
    fn parse_forward_local_zero_is_flagged() {
        let r = parse_forward("tcp:0;tcp:9000").expect("valid");
        assert!(r.local_is_zero, "tcp:0 local must flag auto-assign");
        assert_eq!(r.local_port, 0);
    }

    #[test]
    fn parse_forward_rejects_non_tcp() {
        // Only tcp: endpoints are bridged; localabstract must be rejected.
        assert!(parse_forward("localabstract:foo;tcp:9000").is_err());
        assert!(parse_forward("tcp:1;localabstract:bar").is_err());
    }

    #[test]
    fn parse_forward_rejects_missing_semicolon() {
        let err = parse_forward("tcp:5555").expect_err("no ';' separator");
        assert!(err.starts_with("bad forward:"), "got: {err}");
    }

    #[test]
    fn parse_killforward_extracts_local_port() {
        assert_eq!(parse_killforward("tcp:5555").expect("valid"), 5555);
        assert!(parse_killforward("localabstract:x").is_err());
    }

    #[tokio::test]
    async fn registry_insert_contains_remove() {
        let reg = ForwardRegistry::default();
        assert!(!reg.contains(5555).await);
        // A no-op task stands in for the listener accept loop.
        let task = tokio::spawn(async {});
        reg.insert(5555, 5556, "serialX".to_string(), task).await;
        assert!(
            reg.contains(5555).await,
            "rule must be present after insert"
        );
        assert!(reg.remove(5555).await, "remove returns true when present");
        assert!(!reg.contains(5555).await, "rule gone after remove");
        assert!(!reg.remove(5555).await, "remove returns false when absent");
    }

    #[tokio::test]
    async fn registry_list_is_sorted_and_formatted() {
        let reg = ForwardRegistry::default();
        reg.insert(9000, 1, "B".to_string(), tokio::spawn(async {}))
            .await;
        reg.insert(8000, 2, "A".to_string(), tokio::spawn(async {}))
            .await;
        let body = reg.list().await;
        // Sorted by local port: 8000 then 9000.
        assert_eq!(body, "A tcp:8000 tcp:2\nB tcp:9000 tcp:1\n");
    }

    #[tokio::test]
    async fn registry_remove_by_serial_only_drops_that_serial() {
        let reg = ForwardRegistry::default();
        // Two rules for serialA, one for serialB.
        reg.insert(8000, 1, "serialA".to_string(), tokio::spawn(async {}))
            .await;
        reg.insert(8001, 2, "serialA".to_string(), tokio::spawn(async {}))
            .await;
        reg.insert(9000, 3, "serialB".to_string(), tokio::spawn(async {}))
            .await;

        let removed = reg.remove_by_serial("serialA").await;
        assert_eq!(removed, 2, "both serialA rules must be removed");
        assert!(!reg.contains(8000).await, "serialA rule gone");
        assert!(!reg.contains(8001).await, "serialA rule gone");
        assert!(reg.contains(9000).await, "serialB rule must survive");

        // Removing a serial with no rules is a no-op returning 0.
        assert_eq!(
            reg.remove_by_serial("serialA").await,
            0,
            "second removal finds nothing"
        );
        assert_eq!(
            reg.remove_by_serial("unknown").await,
            0,
            "unknown serial removes nothing"
        );
    }

    #[tokio::test]
    async fn registry_remove_all_clears() {
        let reg = ForwardRegistry::default();
        reg.insert(1, 2, "s".to_string(), tokio::spawn(async {}))
            .await;
        reg.insert(3, 4, "s".to_string(), tokio::spawn(async {}))
            .await;
        reg.remove_all().await;
        assert!(
            reg.list().await.is_empty(),
            "remove_all must clear the registry"
        );
    }
}
