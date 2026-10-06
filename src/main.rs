mod config;
mod modbus;
mod persist;
mod remote_shutdown;
mod state;
mod watchdog;
mod wol;

use anyhow::Result;
use config::Config;
use persist::{ShutdownMarker, ShutdownState};
use state::{Action, StateMachine};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const CONFIG_PATH: &str = "/etc/modbus-ups-bridge/bridge.toml";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config_path = std::env::args().nth(1).unwrap_or_else(|| CONFIG_PATH.to_string());
    let cfg = Config::load(&config_path)?;
    log::info!(
        "loaded config from {} ({} endpoint(s))",
        config_path,
        cfg.endpoints.len()
    );
    for problem in cfg
        .ssh_key_problems()
        .into_iter()
        .chain(remote_shutdown::known_host_problems(&cfg))
    {
        log::error!("{}", problem);
    }

    let wdt = watchdog::Watchdog::open(cfg.watchdog.as_ref())?;
    let wdt_for_stop = wdt.try_clone()?;

    // A requested stop (systemctl stop/restart, Ctrl+C) must disarm the
    // watchdog; otherwise the kernel reboots the box ~30 s later. Anything
    // else that ends the process -- a crash, a hang, SIGKILL -- leaves it
    // armed, which is the point of having it.
    tokio::select! {
        result = run(cfg, wdt) => result,
        _ = stop_requested() => {
            log::info!("stop requested -- exiting");
            wdt_for_stop.disarm();
            Ok(())
        }
    }
}

/// The bridge itself: poll, decide, act. Runs until the process is stopped.
async fn run(cfg: Config, mut wdt: watchdog::Watchdog) -> Result<()> {
    let marker = ShutdownMarker::new(&cfg.state_file);
    let marker_state = marker.state();
    let resume = marker_state != ShutdownState::NotSet;
    let mut pending_resume_endpoints = match &marker_state {
        ShutdownState::Incomplete { dispatched } => {
            let remaining: Vec<_> = cfg
                .endpoints
                .iter()
                .filter(|ep| !dispatched.contains(&ep.name))
                .cloned()
                .collect();
            log::warn!(
                "{} indicates incomplete shutdown: {} endpoint(s) already dispatched, {} remaining: {:?}",
                cfg.state_file,
                dispatched.len(),
                remaining.len(),
                remaining.iter().map(|ep| &ep.name).collect::<Vec<_>>()
            );
            Some(remaining)
        }
        ShutdownState::Completed => {
            log::warn!(
                "{} exists: a previous run shut the endpoints down and never finished \
                 waking them -- resuming latched, Wake-on-LAN will follow recovery",
                cfg.state_file
            );
            None
        }
        ShutdownState::NotSet => None,
    };
    let mut sm = StateMachine::new(cfg.thresholds.clone(), resume);

    // At most one of each in flight. A new shutdown cancels leftover WOL
    // resends. Confirmed recovery stops the shutdown sequence and prevents
    // any not-yet-executed destructive Proxmox step (hard VM stop or host
    // poweroff). An already-issued guest shutdown may still finish remotely.
    let mut shutdown_task: Option<(JoinHandle<()>, watch::Sender<bool>)> = None;
    let mut wake_task: Option<JoinHandle<()>> = None;

    let poll_interval = Duration::from_secs(cfg.modbus.poll_interval_secs);
    let mut modbus_client = connect_with_retry(&cfg, &mut wdt).await;

    loop {
        wdt.feed();

        let reading = match modbus_client.poll().await {
            Ok(r) => r,
            Err(e) => {
                log::error!("modbus poll failed: {:#} -- reconnecting", e);
                // Close the old port before opening it again, so the new
                // connection never competes with the old one for the device.
                drop(modbus_client);
                modbus_client = connect_with_retry(&cfg, &mut wdt).await;
                tokio::time::sleep(poll_interval).await;
                continue;
            }
        };

        let (status, action) = sm.observe(reading);

        log::debug!(
            "soc={:.1}% grid={:.1}V load={:.0}W batt={:.0}W on_battery={} low_battery={}",
            status.battery_soc_pct,
            status.grid_voltage,
            status.load_power_w,
            status.battery_power_w,
            status.on_battery,
            status.low_battery
        );

        let grid_down = status.grid_voltage < cfg.thresholds.grid_lost_voltage;
        if grid_down {
            if let Some(remaining) = pending_resume_endpoints.take() {
                if !remaining.is_empty() {
                    log::warn!(
                        "grid still down: resuming shutdown sequence for {} remaining endpoint(s)",
                        remaining.len()
                    );
                    let opts = remote_shutdown::ShutdownOptions::from_config(&cfg);
                    let (stop_tx, stop_rx) = watch::channel(false);
                    let task = tokio::spawn(remote_shutdown::run_shutdown_sequence(
                        remaining,
                        opts,
                        Some(marker.clone()),
                        stop_rx,
                    ));
                    shutdown_task = Some((task, stop_tx));
                } else {
                    log::info!("all endpoints were already dispatched; marking shutdown complete");
                    marker.mark_completed();
                }
            }
        } else if let Some(ref remaining) = pending_resume_endpoints {
            if !remaining.is_empty() {
                log::debug!(
                    "grid currently up; holding {} remaining shutdown(s) pending recovery confirmation",
                    remaining.len()
                );
            }
        }

        match action {
            Action::TriggerShutdownSequence => {
                pending_resume_endpoints.take();
                if let Some(t) = wake_task.take() {
                    if !t.is_finished() {
                        log::warn!("cancelling pending Wake-on-LAN resends");
                    }
                    t.abort();
                }
                if let Some((t, stop)) = shutdown_task.take() {
                    if !t.is_finished() {
                        log::warn!("cancelling previous in-flight shutdown sequence");
                        let _ = stop.send(true);
                        t.abort();
                    }
                }
                // Before anything goes out, so a power cut mid-sequence still
                // leaves a record that recovery needs to wake the endpoints.
                marker.set();
                let endpoints = cfg.endpoints.clone();
                let opts = remote_shutdown::ShutdownOptions::from_config(&cfg);
                let (stop_tx, stop_rx) = watch::channel(false);
                // Spawned so the poll loop (and watchdog feed) keeps running
                // while the sequence executes.
                let task = tokio::spawn(remote_shutdown::run_shutdown_sequence(
                    endpoints,
                    opts,
                    Some(marker.clone()),
                    stop_rx,
                ));
                shutdown_task = Some((task, stop_tx));
            }
            Action::TriggerWakeOnLan => {
                if let Some(remaining) = pending_resume_endpoints.take() {
                    if !remaining.is_empty() {
                        log::warn!(
                            "recovery confirmed: {} remaining endpoint(s) were spared from shutdown",
                            remaining.len()
                        );
                    }
                }
                if let Some((task, stop)) = shutdown_task.take() {
                    if !task.is_finished() {
                        log::warn!("recovery confirmed -- stopping the rest of the shutdown sequence");
                        let _ = stop.send(true);
                    }
                }
                let endpoints = cfg.endpoints.clone();
                let addr = cfg.wol_broadcast_addr.clone();
                let resends = cfg.thresholds.wol_resend_count;
                let interval = Duration::from_secs(cfg.thresholds.wol_resend_interval_secs);
                let marker = marker.clone();
                wake_task = Some(tokio::spawn(async move {
                    wol::wake_all_repeated(&endpoints, &addr, resends, interval).await;
                    // Only after the last round: a reboot mid-way resumes
                    // latched and runs the whole round again on recovery.
                    marker.clear();
                }));
            }
            Action::None => {}
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Resolves on SIGTERM (what systemd sends on stop) or Ctrl+C.
async fn stop_requested() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(e) => {
                log::error!("cannot listen for SIGTERM ({}) -- only Ctrl+C will disarm the watchdog", e);
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Keeps feeding the watchdog while retrying: a missing RS485 adapter is a
/// condition to log and wait out, not a hang that warrants rebooting the board.
/// Once the port is open, checks the inverter's own cutoff settings against
/// the config, so a change made on the inverter shows up in the log at the
/// next (re)connect.
async fn connect_with_retry(cfg: &Config, wdt: &mut watchdog::Watchdog) -> modbus::ModbusClient {
    loop {
        wdt.feed();
        match modbus::ModbusClient::connect(&cfg.modbus) {
            Ok(mut c) => {
                if check_inverter_settings(&mut c, cfg).await {
                    return c;
                }
                log::error!(
                    "inverter safety checks failed -- retrying in 5s without entering the shutdown state machine"
                );
                drop(c);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(e) => {
                log::error!("modbus connect failed: {:#} -- retrying in 5s", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Evaluates whether the bridge should accept inverter settings and proceed to normal operation.
///
/// Refuses only when the data can't be trusted, which means the wrong device type (reg 0 != 0x0300).
/// For everything else (e.g. cutoff SOC mismatch, low battery margin errors, or battery mode issues),
/// findings are logged at Error level and the bridge continues running.
/// If `strict` is true (opt-in via `strict_inverter_checks`), any Error finding causes the bridge to refuse.
pub fn should_accept_inverter_settings(
    settings: &modbus::InverterSettings,
    findings: &[(log::Level, String)],
    strict: bool,
) -> bool {
    if !settings.is_trusted_device_type() {
        return false;
    }
    if strict {
        let has_errors = findings.iter().any(|(level, _)| *level == log::Level::Error);
        if has_errors {
            return false;
        }
    }
    true
}

/// Checks inverter identity and battery-protection settings against config.
/// Refuses only when data cannot be trusted (wrong device type) or if strict_inverter_checks
/// is enabled and there are Error-level findings. For everything else, logs at Error level
/// and keeps running.
async fn check_inverter_settings(client: &mut modbus::ModbusClient, cfg: &Config) -> bool {
    let strict = cfg.strict_inverter_checks();
    match client.read_settings().await {
        Ok(s) => {
            log::info!(
                "inverter: device type {:#06x}, battery mode {}, cutoff {:.0}% / {:.2} V",
                s.device_type,
                s.battery_mode,
                s.shutdown_soc_pct,
                s.shutdown_voltage
            );
            log::info!("inverter: {}", s.protocol_summary());
            let findings = s.findings(
                cfg.thresholds.low_battery_soc,
                cfg.thresholds.inverter_cutoff_soc,
            );
            for (level, msg) in &findings {
                log::log!(*level, "inverter settings: {}", msg);
            }

            if !s.is_trusted_device_type() {
                log::error!(
                    "inverter data cannot be trusted (wrong device type {:#06x}, expected {:#06x}) -- refusing to operate",
                    s.device_type,
                    modbus::DEVICE_TYPE_SINGLE_PHASE_STORAGE
                );
                return false;
            }

            if strict {
                let has_errors = findings.iter().any(|(level, _)| *level == log::Level::Error);
                if has_errors {
                    log::error!(
                        "strict_inverter_checks is enabled and inverter settings have errors -- refusing to operate"
                    );
                    return false;
                }
            }

            true
        }
        Err(e) => {
            log::error!(
                "could not read required inverter settings: {:#} -- retrying",
                e
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_settings(device_type: u16, battery_mode: u16, cutoff: f64) -> modbus::InverterSettings {
        modbus::InverterSettings {
            protocol_version: Some(0x0102),
            ac_power_ratio: Some(0),
            device_type,
            battery_mode,
            shutdown_soc_pct: cutoff,
            shutdown_voltage: 46.0,
        }
    }

    #[test]
    fn accepts_valid_settings_in_default_and_strict_modes() {
        let s = sample_settings(modbus::DEVICE_TYPE_SINGLE_PHASE_STORAGE, 1, 20.0);
        let findings = s.findings(30.0, 20.0);
        assert!(should_accept_inverter_settings(&s, &findings, false));
        assert!(should_accept_inverter_settings(&s, &findings, true));
    }

    #[test]
    fn refuses_wrong_device_type_even_in_non_strict_mode() {
        let s = sample_settings(0x0500, 1, 20.0);
        let findings = s.findings(30.0, 20.0);
        assert!(!should_accept_inverter_settings(&s, &findings, false));
        assert!(!should_accept_inverter_settings(&s, &findings, true));
    }

    #[test]
    fn non_strict_mode_keeps_running_on_cutoff_mismatch_or_margin_errors() {
        // Cutoff is 30% while low_battery_soc is 30% -> margin error & cutoff mismatch error
        let s = sample_settings(modbus::DEVICE_TYPE_SINGLE_PHASE_STORAGE, 1, 30.0);
        let findings = s.findings(30.0, 20.0);
        assert!(findings.iter().any(|(l, _)| *l == log::Level::Error));

        // In default (non-strict) mode, keeps running
        assert!(should_accept_inverter_settings(&s, &findings, false));

        // In strict mode, refuses
        assert!(!should_accept_inverter_settings(&s, &findings, true));
    }

    #[test]
    fn non_strict_mode_keeps_running_on_no_battery_mode() {
        let s = sample_settings(modbus::DEVICE_TYPE_SINGLE_PHASE_STORAGE, 2, 20.0);
        let findings = s.findings(30.0, 20.0);
        assert!(findings.iter().any(|(l, _)| *l == log::Level::Error));

        // In default (non-strict) mode, keeps running
        assert!(should_accept_inverter_settings(&s, &findings, false));

        // In strict mode, refuses
        assert!(!should_accept_inverter_settings(&s, &findings, true));
    }
}
