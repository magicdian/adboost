//! Persistent USB connection with session multiplexing.
//!
//! Holds a single CNXN+AUTH'd USB connection and allows multiple concurrent
//! ADB sessions (shell, tcp, sync) to be opened without re-authenticating.
//! A background reader thread demultiplexes incoming messages by local_id.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rand::RngExt;

use crate::message_devices::adb_message_transport::ADBMessageTransport;
use crate::message_devices::adb_transport_message::{
    ADBTransportMessage, AUTH_RSAPUBLICKEY, AUTH_SIGNATURE, AUTH_TOKEN,
};
use crate::message_devices::message_commands::MessageCommand;
use crate::message_devices::models::{ADBRsaKey, read_adb_private_key};
use crate::message_devices::usb::usb_transport::USBTransport;
use crate::models::ADBLocalCommand;
use crate::utils::get_default_adb_key_path;
use crate::{Result, RustADBError};
use crate::adb_transport::ADBTransport;

/// Channel buffer size for per-session message queues.
const SESSION_CHANNEL_SIZE: usize = 64;

/// A persistent USB connection to an ADB device that supports concurrent sessions.
///
/// Unlike the per-operation model in `ADBUSBDevice`, this holds the USB handle
/// permanently and multiplexes multiple sessions over a single authenticated connection.
pub struct PersistentUsbConnection {
    /// Writer half — serialized access via mutex.
    writer: Arc<Mutex<USBTransport>>,
    /// Session registry: local_id -> channels for incoming messages.
    sessions: Arc<Mutex<HashMap<u32, SessionChannels>>>,
    /// Reader thread handle.
    reader_handle: Option<thread::JoinHandle<()>>,
    /// Flag to signal shutdown.
    shutdown: Arc<AtomicBool>,
}

impl PersistentUsbConnection {
    /// Create a new persistent connection from a USB transport.
    ///
    /// Performs CNXN+AUTH handshake, then spawns a reader thread for message demuxing.
    pub fn new(transport: USBTransport, private_key_path: Option<PathBuf>) -> Result<Self> {
        let key_path = match private_key_path {
            Some(p) => p,
            None => get_default_adb_key_path()?,
        };

        let private_key = match read_adb_private_key(&key_path)? {
            Some(k) => k,
            None => {
                log::warn!("No private key found at {}. Generating random.", key_path.display());
                ADBRsaKey::new_random()?
            }
        };

        // Connect the transport (claim interface, find endpoints)
        let mut transport = transport;
        transport.connect()?;

        // Perform CNXN handshake
        Self::do_connect(&mut transport, &private_key)?;

        let sessions: Arc<Mutex<HashMap<u32, SessionChannels>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        // Clone transport for reader thread (shares Arc<DeviceHandle>)
        let reader_transport = transport.clone();
        let reader_sessions = sessions.clone();
        let reader_shutdown = shutdown.clone();

        let reader_handle = thread::Builder::new()
            .name("usb-reader".into())
            .spawn(move || {
                Self::reader_loop(reader_transport, reader_sessions, reader_shutdown);
            })
            .map_err(|e| RustADBError::IOError(io::Error::new(io::ErrorKind::Other, e)))?;

        let writer = Arc::new(Mutex::new(transport));

        Ok(Self {
            writer,
            sessions,
            reader_handle: Some(reader_handle),
            shutdown,
        })
    }

    /// Create from vendor_id/product_id.
    pub fn new_from_ids(vendor_id: u16, product_id: u16, private_key_path: Option<PathBuf>) -> Result<Self> {
        let transport = USBTransport::new(vendor_id, product_id)?;
        Self::new(transport, private_key_path)
    }

    /// Perform CNXN+AUTH handshake on a connected transport.
    fn do_connect(transport: &mut USBTransport, private_key: &ADBRsaKey) -> Result<()> {
        // Drain any stale messages from previous sessions on this USB pipe
        loop {
            match transport.read_message_with_timeout(std::time::Duration::from_millis(100)) {
                Ok(msg) => {
                    log::trace!("PersistentUsb: drained stale message: cmd={}", msg.header().command());
                }
                Err(_) => break, // Timeout = pipe is clean
            }
        }

        // Try CNXN up to 3 times (adbd may send stale CLSE after unclean disconnect)
        for attempt in 1..=3 {
            // Banner must include features to enable all adbd services (e.g., tcp:).
        // Match what real adb server sends.
        let banner = "host::features=shell_v2,cmd,stat_v2,ls_v2,fixed_push_mkdir,apex,abb,fixed_push_symlink_timestamp,abb_exec,remount_shell,track_app,sendrecv_v2,sendrecv_v2_brotli,sendrecv_v2_lz4,sendrecv_v2_zstd,sendrecv_v2_dry_run_send,openscreen_mdns,delayed_ack\0";
        let cnxn_msg = ADBTransportMessage::try_new(
            MessageCommand::Cnxn,
            0x0100_0000, // A_VERSION
            1_048_576,
            banner.as_bytes(),
        )?;
        transport.write_message(cnxn_msg)?;

        let response = transport.read_message()?;

        match response.header().command() {
            MessageCommand::Cnxn => {
                let dev_banner = String::from_utf8_lossy(response.payload());
                log::debug!("PersistentUsb: unencrypted connection established, device banner: {:?}", dev_banner);
                return Ok(());
            }
            MessageCommand::Auth => {
                log::debug!("PersistentUsb: authentication required");
                return Self::do_auth(transport, response, private_key);
            }
            MessageCommand::Stls => {
                return Err(RustADBError::ADBRequestFailed(
                    "STLS not supported in persistent USB connection".into(),
                ));
            }
            MessageCommand::Clse => {
                // Stale CLSE from previous session — retry
                log::debug!("PersistentUsb: got stale CLSE on attempt {}, retrying", attempt);
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            _ => {
                return Err(RustADBError::WrongResponseReceived(
                    "Expected CNXN or AUTH".into(),
                    response.header().command().to_string(),
                ));
            }
        }
        }
        Err(RustADBError::ADBRequestFailed("CNXN failed after 3 attempts (stale CLSE)".into()))
    }

    fn do_auth(
        transport: &mut USBTransport,
        message: ADBTransportMessage,
        private_key: &ADBRsaKey,
    ) -> Result<()> {
        if message.header().arg0() != AUTH_TOKEN {
            return Err(RustADBError::ADBRequestFailed(format!(
                "AUTH message with type != TOKEN ({})",
                message.header().arg0()
            )));
        }

        let sign = private_key.sign(message.into_payload())?;
        let sig_msg = ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_SIGNATURE, 0, &sign)?;
        transport.write_message(sig_msg)?;

        let received = transport.read_message()?;
        if received.header().command() == MessageCommand::Cnxn {
            let banner = String::from_utf8_lossy(received.payload());
            log::info!("PersistentUsb: auth OK (signature accepted), device banner: {:?}", banner);
            return Ok(());
        }

        // Send public key
        let mut pubkey = private_key.android_pubkey_encode()?.into_bytes();
        pubkey.push(b'\0');
        let pk_msg = ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_RSAPUBLICKEY, 0, &pubkey)?;
        transport.write_message(pk_msg)?;

        let final_resp = transport.read_message_with_timeout(Duration::from_secs(10))?;
        final_resp.assert_command(MessageCommand::Cnxn)?;
        log::info!("PersistentUsb: auth OK (public key accepted)");
        Ok(())
    }

    /// Reader loop: reads messages and routes them to the appropriate session channel.
    fn reader_loop(
        mut transport: USBTransport,
        sessions: Arc<Mutex<HashMap<u32, SessionChannels>>>,
        shutdown: Arc<AtomicBool>,
    ) {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let msg = match transport.read_message_with_timeout(Duration::from_secs(1)) {
                Ok(msg) => msg,
                Err(e) => {
                    let err_str = e.to_string();
                    // Timeout errors are expected — just loop
                    if err_str.contains("timed out") || err_str.contains("Timeout") {
                        continue;
                    }
                    // Check if we're shutting down
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    log::warn!("PersistentUsb reader error: {}", e);
                    // USB likely disconnected
                    break;
                }
            };

            // Route by arg1 (the recipient's local_id)
            let target_id = msg.header().arg1();
            log::trace!("PersistentUsb reader: cmd={} arg0={} arg1={} payload_len={}",
                msg.header().command(), msg.header().arg0(), target_id, msg.payload().len());
            let sessions_lock = sessions.lock().unwrap();
            if let Some(channels) = sessions_lock.get(&target_id) {
                match msg.header().command() {
                    MessageCommand::Okay => {
                        let _ = channels.ack_tx.try_send(msg);
                    }
                    _ => {
                        // WRTE, CLSE, etc. go to data channel
                        let _ = channels.data_tx.try_send(msg);
                    }
                }
            } else {
                log::trace!(
                    "PersistentUsb: message for unknown session {} (cmd={}, dropping)",
                    target_id,
                    msg.header().command()
                );
            }
        }
        log::debug!("PersistentUsb reader thread exiting");
    }

    /// Open a new multiplexed session with the given ADB command.
    pub fn open_session(&self, cmd: &ADBLocalCommand) -> Result<MultiplexedSession> {
        let mut rng = rand::rng();
        let local_id: u32 = rng.random();

        // Create separate channels for data (WRTE/CLSE) and acks (OKAY)
        let (data_tx, data_rx) = mpsc::sync_channel(SESSION_CHANNEL_SIZE);
        let (ack_tx, ack_rx) = mpsc::sync_channel(SESSION_CHANNEL_SIZE);

        // Register in session map BEFORE sending OPEN (reader may respond fast)
        {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.insert(local_id, SessionChannels { data_tx, ack_tx });
        }

        // Send OPEN message (ADB protocol requires null-terminated service string)
        let mut service_bytes = cmd.to_string().into_bytes();
        if !service_bytes.ends_with(&[0]) {
            service_bytes.push(0);
        }
        log::debug!("PersistentUsb: OPEN local_id={} service={:?}",
            local_id, String::from_utf8_lossy(&service_bytes));
        let open_msg = ADBTransportMessage::try_new(
            MessageCommand::Open,
            local_id,
            0,
            &service_bytes,
        )?;

        {
            let mut writer = self.writer.lock().unwrap();
            writer.write_message(open_msg)?;
        }

        // Wait for OKAY response on ack channel
        let response = ack_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| RustADBError::ADBRequestFailed("open_session: timeout waiting for OKAY".into()))?;

        if response.header().command() != MessageCommand::Okay {
            // Unregister
            self.sessions.lock().unwrap().remove(&local_id);
            return Err(RustADBError::ADBRequestFailed(format!(
                "open_session: expected OKAY, got {}",
                response.header().command()
            )));
        }

        let remote_id = response.header().arg0();

        // With delayed_ack feature, we must send an initial OKAY to signal
        // that we're ready to receive data. Without this, adbd won't send WRTE.
        {
            let ready_msg = ADBTransportMessage::try_new(
                MessageCommand::Okay,
                local_id,
                remote_id,
                &[],
            )?;
            let mut writer = self.writer.lock().unwrap();
            writer.write_message(ready_msg)?;
        }

        Ok(MultiplexedSession {
            local_id,
            remote_id,
            writer: self.writer.clone(),
            data_rx,
            ack_rx,
            sessions: self.sessions.clone(),
            read_buf: Vec::new(),
            read_pos: 0,
            closed: false,
        })
    }

    /// Convenience: execute a shell command and return stdout + exit code.
    pub fn shell_exec(&self, cmd: &str) -> Result<(String, Option<u8>)> {
        let command = ADBLocalCommand::ShellCommand(cmd.to_string(), vec![]);
        let mut session = self.open_session(&command)?;

        let mut output = Vec::new();
        loop {
            let mut buf = [0u8; 4096];
            match session.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(RustADBError::IOError(e)),
            }
        }

        // The last byte of shell output is the exit code (ADB shell v2 protocol isn't used here,
        // but the simple shell protocol doesn't include exit code in the stream).
        // For compatibility with the existing API, return None for exit code.
        let text = String::from_utf8_lossy(&output).to_string();
        Ok((text, None))
    }

    /// Check if the connection is still alive (reader thread running).
    pub fn is_alive(&self) -> bool {
        match &self.reader_handle {
            Some(h) => !h.is_finished(),
            None => false,
        }
    }
}

impl Drop for PersistentUsbConnection {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader_handle.take() {
            // Give reader thread time to notice shutdown
            let _ = handle.join();
        }
        // Disconnect the writer transport
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.disconnect();
        }
    }
}

/// A multiplexed session over a persistent USB connection.
///
/// Represents a single ADB stream (e.g., one shell command or one TCP connection).
/// Implements `Read + Write` for use as a byte stream. Thread-safe writes via
/// the shared writer mutex; reads come from a dedicated per-session channel.
pub struct MultiplexedSession {
    local_id: u32,
    remote_id: u32,
    writer: Arc<Mutex<USBTransport>>,
    /// Channel for data messages (WRTE, CLSE)
    data_rx: Receiver<ADBTransportMessage>,
    /// Channel for flow control (OKAY)
    ack_rx: Receiver<ADBTransportMessage>,
    sessions: Arc<Mutex<HashMap<u32, SessionChannels>>>,
    read_buf: Vec<u8>,
    read_pos: usize,
    closed: bool,
}

/// Channels for a single multiplexed session.
pub struct SessionChannels {
    pub data_tx: SyncSender<ADBTransportMessage>,
    pub ack_tx: SyncSender<ADBTransportMessage>,
}

impl MultiplexedSession {
    /// Get the local session ID.
    pub fn local_id(&self) -> u32 {
        self.local_id
    }

    /// Get the remote session ID.
    pub fn remote_id(&self) -> u32 {
        self.remote_id
    }

    /// Split into independent read and write halves for concurrent use.
    /// The session map entry is cleaned up when BOTH halves are dropped.
    pub fn into_split(mut self) -> (SessionReadHalf, SessionWriteHalf) {
        let local_id = self.local_id;
        let remote_id = self.remote_id;
        let writer = self.writer.clone();
        let sessions = self.sessions.clone();

        // Use ManuallyDrop + ptr::read to move fields out before forgetting self
        let data_rx = std::mem::replace(&mut self.data_rx, {
            // Create a dummy receiver that will never be used
            let (_, rx) = mpsc::sync_channel(1);
            rx
        });
        let ack_rx = std::mem::replace(&mut self.ack_rx, {
            let (_, rx) = mpsc::sync_channel(1);
            rx
        });
        let read_buf = std::mem::take(&mut self.read_buf);
        let read_pos = self.read_pos;
        let closed = self.closed;

        // Prevent Drop from sending CLSE or removing from sessions
        self.closed = true; // suppress CLSE in Drop
        // Drop will still remove from sessions map, but we'll re-insert... 
        // Actually, better: just mark closed so Drop only removes from map.
        // We need to NOT remove from map. Let's just forget self.
        // But we already replaced fields with dummies, so Drop is safe to run
        // on the dummies. Actually the Drop will send CLSE if !closed and remove from map.
        // Set closed=true above prevents CLSE. But it will still remove from sessions.
        // We don't want that. So let's just std::mem::forget.
        std::mem::forget(self);

        let close_state = Arc::new(std::sync::atomic::AtomicBool::new(closed));
        let cleanup = Arc::new(SessionCleanup {
            local_id,
            remote_id,
            writer: writer.clone(),
            sessions,
            closed: close_state.clone(),
        });

        let read_half = SessionReadHalf {
            local_id,
            remote_id,
            writer: writer.clone(),
            data_rx,
            read_buf,
            read_pos,
            closed: close_state.clone(),
            _cleanup: cleanup.clone(),
        };

        let write_half = SessionWriteHalf {
            local_id,
            remote_id,
            writer,
            ack_rx,
            closed: close_state,
            _cleanup: cleanup,
        };

        (read_half, write_half)
    }
}

/// Shared cleanup logic — sends CLSE and removes from sessions map when last reference dropped.
struct SessionCleanup {
    local_id: u32,
    remote_id: u32,
    writer: Arc<Mutex<USBTransport>>,
    sessions: Arc<Mutex<HashMap<u32, SessionChannels>>>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        if !self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok(clse) = ADBTransportMessage::try_new(
                MessageCommand::Clse,
                self.local_id,
                self.remote_id,
                &[],
            ) {
                if let Ok(mut writer) = self.writer.lock() {
                    let _ = writer.write_message(clse);
                }
            }
        }
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(&self.local_id);
        }
    }
}

/// Read half of a split MultiplexedSession.
pub struct SessionReadHalf {
    local_id: u32,
    remote_id: u32,
    writer: Arc<Mutex<USBTransport>>,
    data_rx: Receiver<ADBTransportMessage>,
    read_buf: Vec<u8>,
    read_pos: usize,
    closed: Arc<std::sync::atomic::AtomicBool>,
    _cleanup: Arc<SessionCleanup>,
}

/// Write half of a split MultiplexedSession.
pub struct SessionWriteHalf {
    local_id: u32,
    remote_id: u32,
    writer: Arc<Mutex<USBTransport>>,
    ack_rx: Receiver<ADBTransportMessage>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    _cleanup: Arc<SessionCleanup>,
}


impl Read for SessionReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(0);
        }

        // Return buffered data first
        if self.read_pos < self.read_buf.len() {
            let available = &self.read_buf[self.read_pos..];
            let to_copy = available.len().min(buf.len());
            buf[..to_copy].copy_from_slice(&available[..to_copy]);
            self.read_pos += to_copy;
            if self.read_pos >= self.read_buf.len() {
                self.read_buf.clear();
                self.read_pos = 0;
            }
            return Ok(to_copy);
        }

        let msg = self.data_rx.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "session channel closed")
        })?;

        match msg.header().command() {
            MessageCommand::Write => {
                let okay = ADBTransportMessage::try_new(
                    MessageCommand::Okay,
                    self.local_id,
                    self.remote_id,
                    &[],
                )
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                {
                    let mut writer = self.writer.lock().unwrap();
                    writer.write_message(okay)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                }
                let payload = msg.into_payload();
                if payload.is_empty() {
                    return Ok(0);
                }
                let to_copy = payload.len().min(buf.len());
                buf[..to_copy].copy_from_slice(&payload[..to_copy]);
                if to_copy < payload.len() {
                    self.read_buf = payload;
                    self.read_pos = to_copy;
                }
                Ok(to_copy)
            }
            MessageCommand::Clse => {
                self.closed.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(0)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected command in data channel: {}", msg.header().command()),
            )),
        }
    }
}

impl Write for SessionWriteHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "session closed"));
        }

        let chunk_size = buf.len().min(65536);
        let chunk = &buf[..chunk_size];

        let msg = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id,
            self.remote_id,
            chunk,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        {
            let mut writer = self.writer.lock().unwrap();
            writer.write_message(msg)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        }

        let response = self.ack_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "timeout waiting for OKAY after WRTE")
        })?;

        match response.header().command() {
            MessageCommand::Okay => Ok(chunk_size),
            MessageCommand::Clse => {
                self.closed.store(true, std::sync::atomic::Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "session closed by remote"))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected OKAY, got {}", response.header().command()),
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for MultiplexedSession {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.closed {
            return Ok(0);
        }

        // Return buffered data first
        if self.read_pos < self.read_buf.len() {
            let available = &self.read_buf[self.read_pos..];
            let to_copy = available.len().min(buf.len());
            buf[..to_copy].copy_from_slice(&available[..to_copy]);
            self.read_pos += to_copy;
            if self.read_pos >= self.read_buf.len() {
                self.read_buf.clear();
                self.read_pos = 0;
            }
            return Ok(to_copy);
        }

        // Wait for next message from reader thread (data channel: WRTE/CLSE)
        let msg = self.data_rx.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "session channel closed")
        })?;

        match msg.header().command() {
            MessageCommand::Write => {
                // Send OKAY ack
                let okay = ADBTransportMessage::try_new(
                    MessageCommand::Okay,
                    self.local_id,
                    self.remote_id,
                    &[],
                )
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

                {
                    let mut writer = self.writer.lock().unwrap();
                    writer
                        .write_message(okay)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                }

                let payload = msg.into_payload();
                if payload.is_empty() {
                    return Ok(0);
                }

                let to_copy = payload.len().min(buf.len());
                buf[..to_copy].copy_from_slice(&payload[..to_copy]);
                if to_copy < payload.len() {
                    self.read_buf = payload;
                    self.read_pos = to_copy;
                }
                Ok(to_copy)
            }
            MessageCommand::Clse => {
                self.closed = true;
                Ok(0)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected command in data channel: {}", msg.header().command()),
            )),
        }
    }
}

impl Write for MultiplexedSession {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "session closed"));
        }

        // Chunk to max 64KB
        let chunk_size = buf.len().min(65536);
        let chunk = &buf[..chunk_size];

        let msg = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id,
            self.remote_id,
            chunk,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        {
            let mut writer = self.writer.lock().unwrap();
            writer
                .write_message(msg)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        }

        // Wait for OKAY from reader thread (ack channel)
        let response = self.ack_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "timeout waiting for OKAY after WRTE")
        })?;

        match response.header().command() {
            MessageCommand::Okay => Ok(chunk_size),
            MessageCommand::Clse => {
                self.closed = true;
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "session closed by remote"))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected OKAY, got {}", response.header().command()),
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for MultiplexedSession {
    fn drop(&mut self) {
        // Send CLSE to remote
        if !self.closed {
            if let Ok(clse) = ADBTransportMessage::try_new(
                MessageCommand::Clse,
                self.local_id,
                self.remote_id,
                &[],
            ) {
                if let Ok(mut writer) = self.writer.lock() {
                    let _ = writer.write_message(clse);
                }
            }
        }

        // Unregister from sessions map
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(&self.local_id);
        }
    }
}

// MultiplexedSession is Send: all fields are Send-safe (Mutex, Arc, Receiver, Vec, etc.)
