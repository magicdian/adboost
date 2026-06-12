//! ADB **server** frontend (feature `server`).
//!
//! This module is adboost acting *as* an ADB server: it listens on TCP
//! (default `:5037`), speaks the smartsocket **host protocol** to native `adb`
//! and `scrcpy` clients, and bridges their local services (`shell:` / `tcp:`)
//! onto a [device backend]. It is the mirror image of [`crate::proxy`], which is
//! a *client* that talks to someone else's adb server.
//!
//! The protocol layer ([`protocol`]) lands first: pure, I/O-free wire
//! encode/decode with full unit coverage. The backend trait and the listening
//! frontend build on top of it in later phases.

pub mod protocol;
