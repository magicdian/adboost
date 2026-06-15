use std::time::Duration;

use adboost::usb::{
    ADBTransportMessage, MessageCommand, PersistentUsbConnection, find_all_connected_adb_devices,
};
use adboost::{DeviceFeatureSet, RustADBError};
use tokio::task::JoinError;
use tokio::time::error::Elapsed;

use crate::models::{ADBCliError, ADBCliResult, PersistentCommand, PersistentSubcommand};

/// Run the persistent-USB exerciser.
///
/// This formalizes the throwaway `/tmp` diagnostic harness that found bug #3 into
/// a permanent, one-command reproducer. It:
/// 1. resolves the device (explicit vid/pid or autodetect),
/// 2. builds a [`PersistentUsbConnection`] with the chosen feature set
///    (default = windowed/`delayed_ack`; `--no-delayed-ack` = classic),
/// 3. prints a negotiation self-check (advertised feature set + banner + the
///    first session frame after OPEN — OKAY=accepted vs CLSE=rejected),
/// 4. runs the shell command and prints stdout + exit code.
pub async fn handle_persistent_command(command: PersistentCommand) -> ADBCliResult<()> {
    let (vendor_id, product_id) = resolve_device(command.vendor_id, command.product_id)?;

    let features = if command.no_delayed_ack {
        DeviceFeatureSet {
            delayed_ack: false,
            ..DeviceFeatureSet::default()
        }
    } else {
        DeviceFeatureSet::default()
    };

    let shell_cmd = match command.command {
        Some(PersistentSubcommand::Shell { commands }) if !commands.is_empty() => {
            commands.join(" ")
        }
        _ => "getprop".to_string(),
    };

    println!("=== adboost persistent exerciser ===");
    println!("device:           {vendor_id:04x}:{product_id:04x}");
    println!("requested mode:   {}", mode_label(&features));
    println!(
        "advertised banner (to device): {}",
        features.to_banner_string()
    );

    // Build the connection (CNXN + AUTH happen here). The advertised feature set
    // is the one we asked for; what the device honored is reflected post-connect.
    let conn = PersistentUsbConnection::new_from_ids_with_features(
        vendor_id,
        product_id,
        command.path_to_private_key,
        features,
    )
    .await?;

    // Negotiation self-check: what the connection actually advertised/negotiated.
    let negotiated = conn.device_features();
    println!("--- negotiation self-check ---");
    println!("connection feature set: {negotiated:?}");
    println!(
        "delayed_ack negotiated: {}",
        if negotiated.delayed_ack {
            "true (windowed)"
        } else {
            "false (classic stop-and-wait)"
        }
    );

    // Tee the OPEN's response so OKAY-vs-CLSE is visible (this is the exact view
    // that made bug #3 observable). Subscribe BEFORE opening the session so we
    // don't miss the first frame. The filter EXCLUDES connection-setup frames
    // (CNXN/AUTH/STLS) — those were already consumed during
    // `new_from_ids_with_features` and a buffered/late one could otherwise be
    // mis-reported as the "first frame". We only accept session-routing frames
    // (OKAY/CLSE/WRTE), so the reported frame is genuinely the device's reaction
    // to our session OPEN (OKAY=accepted, CLSE=rejected — the bug #3 signal).
    let mut raw_rx = conn
        .subscribe_raw(|m| {
            matches!(
                m.header().command(),
                MessageCommand::Okay | MessageCommand::Clse | MessageCommand::Write
            )
        })
        .await?;
    let first_frame =
        tokio::spawn(
            async move { tokio::time::timeout(Duration::from_secs(5), raw_rx.recv()).await },
        );

    // Run the shell command.
    println!("--- running: shell {shell_cmd} ---");
    let exec_result = conn.shell_exec(&shell_cmd).await;

    // Report the first observed inbound frame (best-effort, non-fatal).
    report_first_frame(first_frame.await);

    match exec_result {
        Ok((output, exit_code)) => {
            println!("--- output ---");
            print!("{output}");
            if !output.ends_with('\n') {
                println!();
            }
            println!(
                "--- exit code: {} ---",
                exit_code.map_or_else(|| "<none (v1 path)>".to_string(), |c| c.to_string())
            );
            Ok(())
        }
        Err(RustADBError::ADBRequestFailed(reason)) => Err(ADBCliError::Standard(
            format!("shell command failed: {reason}").into(),
        )),
        Err(e) => Err(e.into()),
    }
}

fn mode_label(features: &DeviceFeatureSet) -> &'static str {
    if features.delayed_ack {
        "windowed (delayed_ack)"
    } else {
        "classic (--no-delayed-ack)"
    }
}

/// Resolve the target device: explicit vid/pid, or autodetect the first one.
fn resolve_device(vendor_id: Option<u16>, product_id: Option<u16>) -> ADBCliResult<(u16, u16)> {
    match (vendor_id, product_id) {
        (Some(vid), Some(pid)) => Ok((vid, pid)),
        (None, None) => {
            let devices = find_all_connected_adb_devices()?;
            let device = devices.first().ok_or_else(|| {
                ADBCliError::Standard("no connected ADB USB device found to autodetect".into())
            })?;
            tracing::info!(
                "autodetected USB device {:04x}:{:04x} ({})",
                device.vendor_id,
                device.product_id,
                device.device_description
            );
            Ok((device.vendor_id, device.product_id))
        }
        _ => Err(ADBCliError::Standard(
            "cannot specify --vendor-id without --product-id or vice versa".into(),
        )),
    }
}

/// Print the first inbound frame observed after OPEN (OKAY vs CLSE). Best-effort.
type FirstFrame = Result<Result<Option<ADBTransportMessage>, Elapsed>, JoinError>;

fn report_first_frame(result: FirstFrame) {
    match result {
        Ok(Ok(Some(msg))) => {
            let h = msg.header();
            println!(
                "first session frame after OPEN (OKAY=accepted / CLSE=rejected): {} arg0={} arg1={} payload_len={}",
                h.command(),
                h.arg0(),
                h.arg1(),
                h.data_length()
            );
        }
        Ok(Ok(None)) => {
            println!(
                "first session frame after OPEN (OKAY=accepted / CLSE=rejected): <reader channel closed>"
            );
        }
        Ok(Err(_)) => {
            println!(
                "first session frame after OPEN (OKAY=accepted / CLSE=rejected): <none within 5s>"
            );
        }
        Err(_) => {
            println!(
                "first session frame after OPEN (OKAY=accepted / CLSE=rejected): <observer task failed>"
            );
        }
    }
}
