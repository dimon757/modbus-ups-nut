mod config;
mod modbus;
mod persist;
mod remote_shutdown;
mod state;
mod watchdog;
mod wol;

use anyhow::Result;
use config::{Config, Endpoint};
use persist::{ShutdownMarker, ShutdownState};
use state::{Action, StateMachine};
use std::time::{Duration, Instant};
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
    let pending_resume_endpoints = match &marker_state {
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
    let mut sup = Supervisor {
        cfg: &cfg,
        marker,
        shutdown_task: None,
        wake_task: None,
        winding_down: None,
        pending_resume_endpoints,
        grid_lost_at_last_reading: None,
    };

    let poll_interval = Duration::from_secs(cfg.modbus.poll_interval_secs);
    let strict = cfg.strict_inverter_checks();

    // Connection state. The loop owns (re)connecting -- one attempt per
    // cycle, never an inner retry loop -- so an inverter that stays
    // unreachable still gets a comms-loss tick into the state machine every
    // cycle (see `StateMachine::comms_lost`).
    let mut modbus_client: Option<modbus::ModbusClient> = None;
    let mut settings_checked = false;
    let mut settings_recheck_at = Instant::now();
    // How long after a connect that skipped the settings check to try it
    // again: short after a fresh (re)connect, longer after a periodic retry.
    let mut recheck_delay = SETTINGS_FIRST_RECHECK;
    let mut unreadable_in_a_row = 0u32;
    let mut last_reading_at = Instant::now();

    loop {
        wdt.feed();

        // Monitoring started without a settings check because the inverter
        // would not give its settings up. Try again every so often, so a
        // cutoff or device-type problem is not missed for good.
        if modbus_client.is_some() && !settings_checked && Instant::now() >= settings_recheck_at {
            log::info!("trying the inverter settings check again");
            // Close the old port before opening it again, so the new
            // connection never competes with the old one for the device.
            modbus_client = None;
            recheck_delay = SETTINGS_RECHECK_INTERVAL;
            unreadable_in_a_row = 0;
        }

        if modbus_client.is_none() {
            match connect_once(&cfg, strict, &mut unreadable_in_a_row).await {
                ConnectOutcome::Ready(c, checked) => {
                    unreadable_in_a_row = 0;
                    settings_checked = checked;
                    if !checked {
                        settings_recheck_at = Instant::now() + recheck_delay;
                    }
                    modbus_client = Some(c);
                    wdt.feed();
                }
                ConnectOutcome::Retry(pause) => {
                    sup.on_comms_lost(&mut sm, last_reading_at.elapsed());
                    tokio::time::sleep(pause).await;
                    continue;
                }
            }
        }
        let Some(client) = modbus_client.as_mut() else { continue };

        let reading = match client.poll().await {
            Ok(r) => r,
            Err(e) => {
                log::error!("modbus poll failed: {:#} -- reconnecting", e);
                modbus_client = None;
                recheck_delay = SETTINGS_FIRST_RECHECK;
                unreadable_in_a_row = 0;
                sup.on_comms_lost(&mut sm, last_reading_at.elapsed());
                tokio::time::sleep(poll_interval).await;
                continue;
            }
        };
        last_reading_at = Instant::now();

        let (status, action) = sm.observe(reading);

        log::debug!(
            "soc={:.1}% grid={:.1}V relay={} load={:.0}W batt={:.0}W on_battery={} low_battery={}",
            status.battery_soc_pct,
            status.grid_voltage,
            match status.grid_relay_closed {
                Some(true) => "closed",
                Some(false) => "open",
                None => "n/a",
            },
            status.load_power_w,
            status.battery_power_w,
            status.on_battery,
            status.low_battery
        );

        sup.grid_lost_at_last_reading = Some(status.grid_lost);
        sup.resume_if_grid_down(status.grid_lost, status.grid_lost_confirmed);
        sup.handle(action);

        tokio::time::sleep(poll_interval).await;
    }
}

/// The background work the poll loop starts and stops: at most one of each
/// in flight. A new shutdown cancels leftover WOL resends. Confirmed recovery
/// stops the shutdown sequence and prevents any not-yet-executed destructive
/// Proxmox step (hard VM stop or host poweroff). An already-issued guest
/// shutdown may still finish remotely.
struct Supervisor<'a> {
    cfg: &'a Config,
    marker: ShutdownMarker,
    shutdown_task: Option<(JoinHandle<()>, watch::Sender<bool>)>,
    wake_task: Option<JoinHandle<()>>,
    /// A shutdown sequence that recovery cancelled keeps running briefly to
    /// put right what it already did (restarting VMs it had stopped). Kept
    /// here so a new outage can cancel that clean-up before it fights the
    /// new shutdown.
    winding_down: Option<JoinHandle<()>>,
    /// After a restart mid-sequence: the endpoints not yet dispatched. Sent
    /// once the grid is confirmed down, dropped once recovery is confirmed.
    pending_resume_endpoints: Option<Vec<Endpoint>>,
    /// What the last valid reading said about the grid (None: no reading yet
    /// since this process started). The comms-loss fail-safe for waiting
    /// endpoints must not fire when the last reading said the grid was up.
    grid_lost_at_last_reading: Option<bool>,
}

impl Supervisor<'_> {
    fn spawn_shutdown(&mut self, endpoints: Vec<Endpoint>) {
        let opts = remote_shutdown::ShutdownOptions::from_config(self.cfg);
        let (stop_tx, stop_rx) = watch::channel(false);
        // Spawned so the poll loop (and watchdog feed) keeps running while
        // the sequence executes.
        let task = tokio::spawn(remote_shutdown::run_shutdown_sequence(
            endpoints,
            opts,
            Some(self.marker.clone()),
            stop_rx,
        ));
        self.shutdown_task = Some((task, stop_tx));
    }

    /// Resumes an interrupted shutdown once the grid is *confirmed* down --
    /// lost for the full debounce (`on_battery_debounce_secs`, or
    /// `low_battery_fast_debounce_secs` if SOC is low), like the normal path --
    /// so one glitchy reading right after a reboot can't shut down the
    /// endpoints the previous run never reached.
    fn resume_if_grid_down(&mut self, grid_lost: bool, grid_lost_confirmed: bool) {
        let Some(remaining) = self.pending_resume_endpoints.as_ref() else {
            return;
        };
        if !grid_lost_confirmed {
            if !remaining.is_empty() {
                log::debug!(
                    "grid {}; holding {} remaining shutdown(s) pending recovery confirmation",
                    if grid_lost { "lost, not yet confirmed" } else { "currently up" },
                    remaining.len()
                );
            }
            return;
        }
        self.resume_remaining("grid still down");
    }

    /// Sends the shutdown to the endpoints a previous run never reached (or
    /// marks the shutdown complete if there are none). `why` starts the log line.
    fn resume_remaining(&mut self, why: &str) {
        let remaining = self.pending_resume_endpoints.take().unwrap_or_default();
        if remaining.is_empty() {
            log::info!("all endpoints were already dispatched; marking shutdown complete");
            self.marker.mark_completed();
        } else {
            log::warn!(
                "{}: resuming shutdown sequence for {} remaining endpoint(s)",
                why,
                remaining.len()
            );
            self.spawn_shutdown(remaining);
        }
    }

    /// One cycle without a valid inverter reading. Feeds the state machine's
    /// comms-loss fail-safe and, after a restart that interrupted a shutdown,
    /// the same fail-safe for the endpoints still waiting: with the inverter
    /// silent nothing will ever confirm that the grid is down, and the
    /// battery is draining unseen.
    fn on_comms_lost(&mut self, sm: &mut StateMachine, since_last_reading: Duration) {
        self.handle(sm.comms_lost(since_last_reading));
        if comms_resume_due(
            self.cfg.thresholds.comms_loss_shutdown_secs,
            since_last_reading,
            self.pending_resume_endpoints.is_some(),
            self.grid_lost_at_last_reading != Some(false),
        ) {
            log::error!(
                "no valid inverter reading for {} s after a restart that interrupted a shutdown -- \
                 battery state unknown",
                since_last_reading.as_secs()
            );
            self.resume_remaining("inverter silent");
        }
    }

    fn handle(&mut self, action: Action) {
        match action {
            Action::TriggerShutdownSequence => {
                self.pending_resume_endpoints.take();
                if let Some(t) = self.winding_down.take() {
                    if !t.is_finished() {
                        log::warn!("cancelling the clean-up of the previous cancelled shutdown");
                    }
                    t.abort();
                }
                if let Some(t) = self.wake_task.take() {
                    if !t.is_finished() {
                        log::warn!("cancelling pending Wake-on-LAN resends");
                    }
                    t.abort();
                }
                if let Some((t, stop)) = self.shutdown_task.take() {
                    if !t.is_finished() {
                        log::warn!("cancelling previous in-flight shutdown sequence");
                        let _ = stop.send(true);
                        t.abort();
                    }
                }
                // Before anything goes out, so a power cut mid-sequence still
                // leaves a record that recovery needs to wake the endpoints.
                self.marker.set();
                self.spawn_shutdown(self.cfg.endpoints.clone());
            }
            Action::TriggerWakeOnLan => {
                if let Some(remaining) = self.pending_resume_endpoints.take() {
                    if !remaining.is_empty() {
                        log::warn!(
                            "recovery confirmed: {} remaining endpoint(s) were spared from shutdown",
                            remaining.len()
                        );
                    }
                }
                if let Some((task, stop)) = self.shutdown_task.take() {
                    if !task.is_finished() {
                        log::warn!("recovery confirmed -- stopping the rest of the shutdown sequence");
                        let _ = stop.send(true);
                        // An older clean-up still running would otherwise be
                        // detached and impossible to cancel later.
                        if let Some(old) = self.winding_down.take() {
                            old.abort();
                        }
                        // Not dropped: it is still busy undoing what it had done.
                        self.winding_down = Some(task);
                    }
                }
                let endpoints = self.cfg.endpoints.clone();
                let addr = self.cfg.wol_broadcast_addr.clone();
                let resends = self.cfg.thresholds.wol_resend_count;
                let interval = Duration::from_secs(self.cfg.thresholds.wol_resend_interval_secs);
                let marker = self.marker.clone();
                self.wake_task = Some(tokio::spawn(async move {
                    wol::wake_all_repeated(&endpoints, &addr, resends, interval).await;
                    // Only after the last round: a reboot mid-way resumes
                    // latched and runs the whole round again on recovery.
                    marker.clear();
                }));
            }
            Action::None => {}
        }
    }
}

impl Drop for Supervisor<'_> {
    fn drop(&mut self) {
        if let Some((ref t, _)) = self.shutdown_task {
            if !t.is_finished() {
                log::warn!("bridge stopped while a shutdown sequence was still in flight");
            }
        }
        if let Some(ref t) = self.winding_down {
            if !t.is_finished() {
                log::warn!("bridge stopped while VM restart clean-up was still in flight");
            }
        }
        if let Some(ref t) = self.wake_task {
            if !t.is_finished() {
                log::info!("bridge stopped while Wake-on-LAN resends were still in flight");
            }
        }
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

/// When monitoring began without a settings check: how soon after that the
/// check is tried again, and how often after that.
const SETTINGS_FIRST_RECHECK: Duration = Duration::from_secs(10);
const SETTINGS_RECHECK_INTERVAL: Duration = Duration::from_secs(60);

/// What reading the inverter's own settings and comparing them with the
/// config led to.
#[derive(Debug, PartialEq, Eq)]
enum SettingsCheck {
    /// Read, and fine to monitor (problems, if any, were logged).
    Accept,
    /// Read, and this device must not be monitored.
    Refuse,
    /// Could not be read at all.
    Unreadable,
}

/// What to do after a settings check that came back `Unreadable`.
#[derive(Debug, PartialEq, Eq)]
enum UnreadableAction {
    /// Drop the connection (which also clears any stray bytes a timed-out
    /// read left on the line) and try again.
    Reconnect,
    /// Monitor anyway, loudly, and try the check again later.
    RunWithoutCheck,
}

/// `unreadable_in_a_row` includes the failure just seen. One failure is
/// worth a clean reconnect; a second in a row means the inverter will not
/// give these registers up, and protecting the site without the check beats
/// not protecting it. Strict mode never gives in.
fn unreadable_action(strict: bool, unreadable_in_a_row: u32) -> UnreadableAction {
    if strict || unreadable_in_a_row < 2 {
        UnreadableAction::Reconnect
    } else {
        UnreadableAction::RunWithoutCheck
    }
}

/// Whether the endpoints a restart left waiting should be shut down now
/// because the inverter has been silent for the comms-loss limit. Never when
/// the last reading said the grid was up (grid_not_seen_up false): then the
/// machines are spared, as the state machine's own fail-safe also does.
fn comms_resume_due(
    limit_secs: u64,
    since_last_reading: Duration,
    has_pending: bool,
    grid_not_seen_up: bool,
) -> bool {
    has_pending
        && grid_not_seen_up
        && limit_secs > 0
        && since_last_reading >= Duration::from_secs(limit_secs)
}

/// What one connection attempt led to.
enum ConnectOutcome {
    /// Connected; `bool` = whether the settings check actually completed
    /// (false: the settings could not be read, see `unreadable_action`).
    Ready(modbus::ModbusClient, bool),
    /// Not usable yet (port won't open, settings refused or unreadable).
    /// The port is already closed; try again after this pause.
    Retry(Duration),
}

/// One attempt to open the port and check the inverter's own cutoff
/// settings against the config, so a change made on the inverter shows up
/// in the log at the next (re)connect. A missing RS485 adapter is a
/// condition to log and wait out, not a hang: the caller keeps feeding the
/// watchdog -- and the state machine's comms-loss check -- between attempts.
///
/// `unreadable_in_a_row` counts consecutive unreadable settings checks
/// across calls; the caller resets it when a new (re)connect cycle starts.
async fn connect_once(cfg: &Config, strict: bool, unreadable_in_a_row: &mut u32) -> ConnectOutcome {
    match modbus::ModbusClient::connect(&cfg.modbus) {
        Ok(mut c) => match check_inverter_settings(&mut c, cfg).await {
            SettingsCheck::Accept => ConnectOutcome::Ready(c, true),
            SettingsCheck::Refuse => {
                log::error!(
                    "inverter safety checks failed -- retrying in 5s without entering the shutdown state machine"
                );
                drop(c);
                ConnectOutcome::Retry(Duration::from_secs(5))
            }
            SettingsCheck::Unreadable => {
                *unreadable_in_a_row += 1;
                match unreadable_action(strict, *unreadable_in_a_row) {
                    UnreadableAction::Reconnect => {
                        log::warn!(
                            "inverter settings unreadable ({} in a row) -- reconnecting and trying again",
                            unreadable_in_a_row
                        );
                        drop(c);
                        let pause = if strict { 5 } else { 1 };
                        ConnectOutcome::Retry(Duration::from_secs(pause))
                    }
                    UnreadableAction::RunWithoutCheck => {
                        log::error!(
                            "inverter settings could not be read {} times in a row -- monitoring \
                             WITHOUT a settings check (cutoff, margin and device type are \
                             unverified); the check is tried again shortly. Set \
                             strict_inverter_checks = true to refuse to run instead",
                            unreadable_in_a_row
                        );
                        ConnectOutcome::Ready(c, false)
                    }
                }
            }
        },
        Err(e) => {
            log::error!("modbus connect failed: {:#} -- retrying in 5s", e);
            ConnectOutcome::Retry(Duration::from_secs(5))
        }
    }
}

/// Why the bridge must not monitor this inverter, or `None` to go ahead.
///
/// Refuses only when the data can't be trusted: the wrong device type
/// (register 0 != 0x0300). Every other problem is logged at Error level by
/// the caller and monitoring continues -- a late shutdown beats none. With
/// `strict` (opt-in via `strict_inverter_checks`), any Error finding also
/// refuses.
fn settings_refusal(
    settings: &modbus::InverterSettings,
    findings: &[(log::Level, String)],
    strict: bool,
) -> Option<String> {
    if !settings.is_trusted_device_type() {
        return Some(format!(
            "inverter data cannot be trusted (wrong device type {:#06x}, expected {:#06x})",
            settings.device_type,
            modbus::DEVICE_TYPE_SINGLE_PHASE_STORAGE
        ));
    }
    if strict && findings.iter().any(|(level, _)| *level == log::Level::Error) {
        return Some("strict_inverter_checks is enabled and the inverter settings have errors".to_string());
    }
    None
}

/// Checks inverter identity and battery-protection settings against config.
async fn check_inverter_settings(client: &mut modbus::ModbusClient, cfg: &Config) -> SettingsCheck {
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
            let mut findings = s.findings(
                cfg.thresholds.low_battery_soc,
                cfg.thresholds.inverter_cutoff_soc,
            );
            if let Some(f) = s.grid_relay_finding(cfg.thresholds.use_grid_relay) {
                findings.push(f);
            }
            for (level, msg) in &findings {
                log::log!(*level, "inverter settings: {}", msg);
            }

            match settings_refusal(&s, &findings, cfg.strict_inverter_checks()) {
                Some(reason) => {
                    log::error!("{} -- refusing to operate", reason);
                    SettingsCheck::Refuse
                }
                None => {
                    if findings.iter().any(|(level, _)| *level == log::Level::Error) {
                        log::error!(
                            "inverter settings have errors (see above) -- monitoring continues anyway; \
                             fix them as soon as possible"
                        );
                    }
                    SettingsCheck::Accept
                }
            }
        }
        Err(e) => {
            log::error!("could not read required inverter settings: {:#}", e);
            SettingsCheck::Unreadable
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
            grid_relay: Some(1),
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
        assert_eq!(settings_refusal(&s, &findings, false), None);
        assert_eq!(settings_refusal(&s, &findings, true), None);
    }

    #[test]
    fn refuses_wrong_device_type_even_in_non_strict_mode() {
        let s = sample_settings(0x0500, 1, 20.0);
        let findings = s.findings(30.0, 20.0);
        let reason = settings_refusal(&s, &findings, false).expect("must refuse");
        assert!(reason.contains("0x0500") && reason.contains("0x0300"), "{reason}");
        assert!(settings_refusal(&s, &findings, true).is_some());
    }

    #[test]
    fn non_strict_mode_keeps_running_on_cutoff_mismatch_or_margin_errors() {
        // Cutoff is 30% while low_battery_soc is 30% -> margin error & cutoff mismatch error
        let s = sample_settings(modbus::DEVICE_TYPE_SINGLE_PHASE_STORAGE, 1, 30.0);
        let findings = s.findings(30.0, 20.0);
        assert!(findings.iter().any(|(l, _)| *l == log::Level::Error));
        assert_eq!(settings_refusal(&s, &findings, false), None);
        let reason = settings_refusal(&s, &findings, true).expect("strict must refuse");
        assert!(reason.contains("strict_inverter_checks"), "{reason}");
    }

    #[test]
    fn non_strict_mode_keeps_running_on_no_battery_mode() {
        let s = sample_settings(modbus::DEVICE_TYPE_SINGLE_PHASE_STORAGE, 2, 20.0);
        let findings = s.findings(30.0, 20.0);
        assert!(findings.iter().any(|(l, _)| *l == log::Level::Error));
        assert_eq!(settings_refusal(&s, &findings, false), None);
        assert!(settings_refusal(&s, &findings, true).is_some());
    }

    #[test]
    fn unreadable_settings_get_one_clean_reconnect_then_monitoring_goes_ahead() {
        assert_eq!(unreadable_action(false, 1), UnreadableAction::Reconnect);
        assert_eq!(unreadable_action(false, 2), UnreadableAction::RunWithoutCheck);
        assert_eq!(unreadable_action(false, 7), UnreadableAction::RunWithoutCheck);
    }

    #[test]
    fn comms_resume_is_due_only_with_waiting_endpoints_and_the_limit_passed() {
        let limit = 300;
        assert!(!comms_resume_due(limit, Duration::from_secs(299), true, true));
        assert!(comms_resume_due(limit, Duration::from_secs(300), true, true));
        assert!(comms_resume_due(limit, Duration::from_secs(3600), true, true));
        // Nothing waiting, or the fail-safe switched off: never.
        assert!(!comms_resume_due(limit, Duration::from_secs(3600), false, true));
        assert!(!comms_resume_due(0, Duration::from_secs(3600), true, true));
        // The last reading said the grid was up: spared, however long the silence.
        assert!(!comms_resume_due(limit, Duration::from_secs(3600), true, false));
    }

    #[test]
    fn strict_mode_never_runs_without_the_settings_check() {
        for n in [1, 2, 3, 100] {
            assert_eq!(unreadable_action(true, n), UnreadableAction::Reconnect);
        }
    }
}
