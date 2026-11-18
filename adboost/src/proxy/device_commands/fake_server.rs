//! In-process fake adb server for device-operation integration tests.
//!
//! Drives the real [`ADBProxyDevice`] against a loopback socket that speaks the
//! `%04x` host framing. This is the only way to exercise the whole
//! request → upload → reply path (transport switch, service string, streamed
//! APK, reply reading) without hardware, and it keeps those tests independent of
//! the `server` feature's simulator.

use std::net::SocketAddrV4;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::RustADBError;
use crate::models::{InstallControl, InstallOptions, RootState};
use crate::proxy::ADBProxyDevice;

/// What the fake server sends for the operation request (the one issued after
/// the `host:transport:` switch).
enum FakeReply {
    /// Write these chunks after `OKAY`; the short pause between writes forces the
    /// client to observe a fragmented reply.
    Text(Vec<&'static [u8]>),
    /// Write `OKAY`, consume the `-S <n>` payload, then write these chunks.
    AfterUpload(Vec<&'static [u8]>),
}

struct FakeAdbServer {
    addr: SocketAddrV4,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl FakeAdbServer {
    async fn start(reply: FakeReply) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("fake adb server must bind a loopback port");
        let addr = match listener.local_addr().expect("local address") {
            std::net::SocketAddr::V4(addr) => addr,
            std::net::SocketAddr::V6(_) => {
                panic!("binding to 127.0.0.1 must produce an IPv4 address");
            }
        };

        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client must connect");
            loop {
                let mut length = [0u8; 4];
                if stream.read_exact(&mut length).await.is_err() {
                    break;
                }
                let length = usize::from_str_radix(
                    std::str::from_utf8(&length).expect("length prefix is ASCII"),
                    16,
                )
                .expect("length prefix is hex");
                let mut body = vec![0u8; length];
                stream
                    .read_exact(&mut body)
                    .await
                    .expect("request body must arrive");
                let request = String::from_utf8(body).expect("request is UTF-8");
                recorder
                    .lock()
                    .expect("request log is not poisoned")
                    .push(request.clone());

                if request.starts_with("host:transport") {
                    stream.write_all(b"OKAY").await.expect("OKAY reply");
                    continue;
                }

                stream.write_all(b"OKAY").await.expect("OKAY reply");
                let chunks = match reply {
                    FakeReply::Text(chunks) => chunks,
                    FakeReply::AfterUpload(chunks) => {
                        read_upload(&mut stream, announced_size(&request)).await;
                        chunks
                    }
                };
                for chunk in chunks {
                    stream.write_all(chunk).await.expect("reply chunk");
                    // Separate the chunks in time so the client cannot coalesce
                    // them into one read.
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                break;
            }
            let _ = stream.shutdown().await;
        });

        Self {
            addr,
            requests,
            task,
        }
    }

    const fn addr(&self) -> SocketAddrV4 {
        self.addr
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("request log is not poisoned")
            .clone()
    }

    async fn wait(self) {
        self.task.await.expect("fake adb server task must finish");
    }
}

async fn read_upload(stream: &mut TcpStream, size: usize) {
    let mut remaining = size;
    let mut buffer = [0u8; 8192];
    while remaining > 0 {
        let capacity = remaining.min(buffer.len());
        let read = stream
            .read(&mut buffer[..capacity])
            .await
            .expect("APK upload must arrive");
        assert!(read > 0, "client closed before sending {size} bytes");
        remaining -= read;
    }
}

fn announced_size(request: &str) -> usize {
    let (_, tail) = request
        .rsplit_once(" -S ")
        .expect("a streamed install request must carry -S <size>");
    tail.split_whitespace()
        .next()
        .expect("-S must have a value")
        .parse()
        .expect("-S value is a byte count")
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// A temporary `.apk` file removed on drop.
struct TempApk {
    path: PathBuf,
}

impl TempApk {
    fn new(contents: &[u8]) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "adboost-device-ops-{}-{id}.apk",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("temporary APK must be writable");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempApk {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn connected_device(server: &FakeAdbServer) -> ADBProxyDevice {
    ADBProxyDevice::new("S1".to_string(), Some(server.addr()))
}

#[tokio::test]
async fn install_sends_flags_and_reads_fragmented_success() {
    let apk = TempApk::new(&[0x5a; 32]);
    let server = FakeAdbServer::start(FakeReply::AfterUpload(vec![b"Succ", b"ess\n"])).await;
    let mut device = connected_device(&server);
    let options = InstallOptions {
        replace: true,
        grant: true,
        test_only: true,
        downgrade: true,
        user: Some("10".to_string()),
    };

    let outcome = device
        .install_with_options(apk.path(), &options, InstallControl::default())
        .await
        .expect("a fragmented Success reply must install");

    assert!(outcome.success, "Success must produce a successful outcome");
    assert_eq!(outcome.raw, "Success\n", "raw reply must be preserved");
    assert_eq!(
        outcome.package, None,
        "the standard reply carries no package"
    );

    let requests = server.requests();
    assert_eq!(
        requests.first().map(String::as_str),
        Some("host:transport:S1"),
        "the device must be selected before the service"
    );
    assert_eq!(
        requests.get(1).map(String::as_str),
        Some("exec:cmd package 'install' -r -g -t -d --user 10 -S 32"),
        "install flags and size must be rendered in the documented order"
    );
    server.wait().await;
}

#[tokio::test]
async fn install_failure_reports_structured_reason() {
    let apk = TempApk::new(&[0x01; 8]);
    let server = FakeAdbServer::start(FakeReply::AfterUpload(vec![
        b"Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]\n",
    ]))
    .await;
    let mut device = connected_device(&server);

    let error = device
        .install_with_options(
            apk.path(),
            &InstallOptions::default(),
            InstallControl::default(),
        )
        .await
        .expect_err("a Failure reply must be an error");

    match error {
        RustADBError::InstallFailed { reason, raw } => {
            assert_eq!(
                reason, "INSTALL_FAILED_INSUFFICIENT_STORAGE",
                "the device reason must be extracted from the brackets"
            );
            assert!(raw.contains("Failure"), "raw reply must be preserved");
        }
        other => panic!("expected InstallFailed, got {other:?}"),
    }
    server.wait().await;
}

#[tokio::test]
async fn legacy_install_still_streams_with_user() {
    let apk = TempApk::new(&[0x02; 3]);
    let server = FakeAdbServer::start(FakeReply::AfterUpload(vec![b"Success\n"])).await;
    let mut device = connected_device(&server);

    device
        .install(apk.path(), Some("0"))
        .await
        .expect("legacy install must keep working");

    let requests = server.requests();
    assert_eq!(
        requests.get(1).map(String::as_str),
        Some("exec:cmd package 'install' --user 0 -S 3"),
        "the legacy API must render the same service string as before"
    );
    server.wait().await;
}

#[tokio::test]
async fn uninstall_returns_structured_outcome() {
    let server = FakeAdbServer::start(FakeReply::Text(vec![b"Success\n"])).await;
    let mut device = connected_device(&server);

    let outcome = device
        .uninstall_with_outcome("com.example.app", None)
        .await
        .expect("uninstall must succeed");

    assert!(outcome.success);
    assert_eq!(outcome.raw, "Success\n");
    assert_eq!(
        server.requests().get(1).map(String::as_str),
        Some("exec:cmd package 'uninstall' com.example.app")
    );
    server.wait().await;
}

#[tokio::test]
async fn uninstall_failure_reports_structured_reason() {
    let server = FakeAdbServer::start(FakeReply::Text(vec![
        b"Failure [DELETE_FAILED_INTERNAL_ERROR]\n",
    ]))
    .await;
    let mut device = connected_device(&server);

    let error = device
        .uninstall_with_outcome("com.example.app", Some("10"))
        .await
        .expect_err("a Failure reply must be an error");
    assert!(
        matches!(
            &error,
            RustADBError::UninstallFailed { reason, .. }
                if reason == "DELETE_FAILED_INTERNAL_ERROR"
        ),
        "expected UninstallFailed with the device reason, got {error:?}"
    );
    server.wait().await;
}

#[tokio::test]
async fn root_receipt_is_classified() {
    let server = FakeAdbServer::start(FakeReply::Text(vec![b"restarting adbd as root\n"])).await;
    let mut device = connected_device(&server);

    let outcome = device
        .root_with_outcome()
        .await
        .expect("reading the root receipt must succeed");
    assert_eq!(
        outcome.state,
        RootState::Restarting,
        "a restart receipt must not be reported as an unknown state"
    );
    assert_eq!(outcome.raw, "restarting adbd as root\n");
    server.wait().await;
}

#[tokio::test]
async fn legacy_root_errors_when_device_refuses() {
    let server = FakeAdbServer::start(FakeReply::Text(vec![
        b"adbd cannot run as root in production builds\n",
    ]))
    .await;
    let mut device = connected_device(&server);

    let error = device
        .root()
        .await
        .expect_err("a refusal must not be reported as success");
    assert!(
        matches!(&error, RustADBError::RootRefused { raw } if raw.contains("cannot run as root")),
        "expected RootRefused, got {error:?}"
    );
    server.wait().await;
}

#[tokio::test]
async fn remount_reads_multiline_output() {
    let server = FakeAdbServer::start(FakeReply::Text(vec![
        b"Using overlayfs for /system\nUsing overlayfs for /vendor\nremount succeeded\n",
    ]))
    .await;
    let mut device = connected_device(&server);

    let outcome = device
        .remount_with_outcome()
        .await
        .expect("multi-line success must remount");
    assert_eq!(outcome.info.len(), 2, "both Using lines must be parsed");
    assert!(outcome.raw.contains("remount succeeded"));
    server.wait().await;
}

#[tokio::test]
async fn remount_failure_carries_raw_text() {
    let server = FakeAdbServer::start(FakeReply::Text(vec![
        b"remount failed: Permission denied\n",
    ]))
    .await;
    let mut device = connected_device(&server);

    let error = device
        .remount_with_outcome()
        .await
        .expect_err("a failure reply must be an error");
    match error {
        RustADBError::RemountError(raw) => {
            assert!(
                raw.contains("remount failed"),
                "the raw device text must be preserved: {raw}"
            );
        }
        other => panic!("expected RemountError, got {other:?}"),
    }
    server.wait().await;
}
