mod config;
mod modbus;
mod persist;
mod remote_shutdown;
mod state;
mod watchdog;
mod wol;

use anyhow::Result;
use config::Config;
use persist::ShutdownMarker;
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
    let resume = marker.is_set();
    if resume {
        log::warn!(
            "{} exists: a previous run shut the endpoints down and never finished \
             waking them -- resuming latched, Wake-on-LAN will follow recovery",
            cfg.state_file
        );
    }
    let mut sm = StateMachine::new(cfg.thresholds.clone(), resume);

    // At most one of each in flight. A new shutdown cancels leftover WOL
    // resends. Confirmed recovery stops the shutdown sequence from starting
    // further endpoints -- but hosts already shutting down are left to
    // finish (see remote_shutdown::run_shutdown_sequence), so the stop is a
    // signal, not an abort.
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

        match action {
            Action::TriggerShutdownSequence => {
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
                    endpoints, opts, stop_rx,
                ));
                shutdown_task = Some((task, stop_tx));
            }
            Action::TriggerWakeOnLan => {
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
                check_inverter_settings(&mut c, cfg).await;
                return c;
            }
            Err(e) => {
                log::error!("modbus connect failed: {:#} -- retrying in 5s", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Logs only -- never refuses to run. If the inverter cuts output too early,
/// a late graceful shutdown still beats none at all.
async fn check_inverter_settings(client: &mut modbus::ModbusClient, cfg: &Config) {
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
            for (level, msg) in s.findings(
                cfg.thresholds.low_battery_soc,
                cfg.thresholds.inverter_cutoff_soc,
            ) {
                log::log!(level, "inverter settings: {}", msg);
            }
        }
        Err(e) => log::warn!("could not read inverter settings: {:#}", e),
    }
}
