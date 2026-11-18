//! Real-device probe for the typed device-operation APIs.
//!
//! Exercises the `ADBProxyDevice` outcome APIs against a live adb server, so a
//! physical device can validate the wire path that xdb uses:
//!
//! ```text
//! cargo run -p adboost --example device_ops_probe -- [--serial S] [--server IP:PORT] <command>
//!
//! commands:
//!   root | unroot | remount
//!   install <path.apk> [--replace] [--grant] [--test-only] [--downgrade] [--user ID]
//!   uninstall <package> [--user ID]
//! ```
//!
//! Exit status is 0 on a device-reported success and 1 on a device-reported
//! failure (the structured error is printed), 2 on a usage error.

use std::net::{Ipv4Addr, SocketAddrV4};

use adboost::proxy::ADBProxyDevice;
use adboost::{InstallControl, InstallOptions, InstallProgress, RootOutcome, RustADBError};

#[tokio::main]
async fn main() {
    let mut serial: Option<String> = None;
    let mut server = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5037);
    let mut positional = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--serial" | "-s" => match args.next() {
                Some(value) => serial = Some(value),
                None => usage_exit("--serial needs a value"),
            },
            "--server" => {
                let Some(value) = args.next() else {
                    usage_exit("--server needs IP:PORT");
                };
                let Some((ip, port)) = value.rsplit_once(':') else {
                    usage_exit("--server expects IP:PORT");
                };
                let (Ok(ip), Ok(port)) = (ip.parse(), port.parse()) else {
                    usage_exit("--server expects IP:PORT");
                };
                server = SocketAddrV4::new(ip, port);
            }
            other => positional.push(other.to_string()),
        }
    }

    let Some(command) = positional.first().cloned() else {
        usage_exit("missing command");
    };

    let mut device = match serial {
        Some(serial) => ADBProxyDevice::new(serial, Some(server)),
        None => ADBProxyDevice::autodetect(Some(server)),
    };

    let result = match command.as_str() {
        "root" => device
            .root_with_outcome()
            .await
            .map(|outcome| print_root("root", &outcome)),
        "unroot" => device
            .unroot_with_outcome()
            .await
            .map(|outcome| print_root("unroot", &outcome)),
        "remount" => device.remount_with_outcome().await.map(|outcome| {
            println!(
                "remount: {} partition entries, raw={:?}",
                outcome.info.len(),
                outcome.raw
            );
        }),
        "install" => install(&mut device, &positional[1..]).await,
        "uninstall" => uninstall(&mut device, &positional[1..]).await,
        other => usage_exit(&format!("unknown command {other:?}")),
    };

    if let Err(error) = result {
        eprintln!("device operation failed: {error}");
        std::process::exit(1);
    }
}

fn print_root(label: &str, outcome: &RootOutcome) {
    println!("{label}: state={:?} raw={:?}", outcome.state, outcome.raw);
}

async fn install(device: &mut ADBProxyDevice, args: &[String]) -> Result<(), RustADBError> {
    let Some(path) = args.first() else {
        usage_exit("install needs an APK path");
    };
    let mut options = InstallOptions::default();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--replace" | "-r" => options.replace = true,
            "--grant" | "-g" => options.grant = true,
            "--test-only" | "-t" => options.test_only = true,
            "--downgrade" | "-d" => options.downgrade = true,
            "--user" => {
                index += 1;
                let Some(user) = args.get(index) else {
                    usage_exit("--user needs an id");
                };
                options.user = Some(user.clone());
            }
            other => usage_exit(&format!("unknown install option {other:?}")),
        }
        index += 1;
    }

    let total = std::fs::metadata(path)?.len();
    println!(
        "install: path={path} size={total} replace={} grant={} test_only={} downgrade={} user={:?}",
        options.replace, options.grant, options.test_only, options.downgrade, options.user
    );

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<InstallProgress>(8);
    let progress_task = tokio::spawn(async move {
        let mut last = 0;
        while let Some(progress) = progress_rx.recv().await {
            // Print at most every 4 MiB plus the final sample, to keep the probe
            // output readable on large APKs.
            if progress.bytes_sent == progress.total_bytes
                || progress.bytes_sent >= last + 4 * 1024 * 1024
            {
                last = progress.bytes_sent;
                println!("progress: {}/{}", progress.bytes_sent, progress.total_bytes);
            }
        }
    });

    let control = InstallControl::with_progress(progress_tx);
    let result = device.install_with_options(path, &options, control).await;
    progress_task.await.expect("progress task must finish");

    match result {
        Ok(outcome) => {
            println!(
                "install: success package={:?} raw={:?}",
                outcome.package, outcome.raw
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn uninstall(device: &mut ADBProxyDevice, args: &[String]) -> Result<(), RustADBError> {
    let Some(package) = args.first() else {
        usage_exit("uninstall needs a package name");
    };
    let user = match args.get(1).map(String::as_str) {
        None => None,
        Some("--user") => {
            let Some(user) = args.get(2) else {
                usage_exit("--user needs an id");
            };
            Some(user.as_str())
        }
        Some(other) => usage_exit(&format!("unknown uninstall option {other:?}")),
    };

    match device.uninstall_with_outcome(package, user).await {
        Ok(outcome) => {
            println!("uninstall: success raw={:?}", outcome.raw);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn usage_exit(message: &str) -> ! {
    eprintln!("{message}");
    eprintln!(
        "usage: device_ops_probe [--serial S] [--server IP:PORT] \
         <root|unroot|remount|install|uninstall> ..."
    );
    std::process::exit(2);
}
