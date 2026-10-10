use crate::config::Thresholds;
use crate::modbus::InverterReading;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The shutdown fires when at least LOW_SOC_NEEDED of the last LOW_SOC_WINDOW
/// SOC readings are at or below `low_battery_soc`: one stray reading (e.g. 0 %
/// while the BMS link hiccups) can't shut the site down, while a real low
/// battery fires on its second low reading -- one poll later.
const LOW_SOC_WINDOW: usize = 3;
const LOW_SOC_NEEDED: usize = 2;

#[derive(Debug, Clone, Copy)]
pub struct BridgeStatus {
    pub on_battery: bool,
    pub low_battery: bool,
    pub battery_soc_pct: f64,
    pub grid_voltage: f64,
    pub grid_relay_closed: Option<bool>,
    /// The decision the state machine acted on this poll: voltage below the
    /// threshold, or the inverter's grid relay open. Everything else in the
    /// bridge that needs to know "is the grid down?" uses this.
    pub grid_lost: bool,
    /// `grid_lost` has held continuously for `on_battery_debounce_secs`
    /// (or `low_battery_fast_debounce_secs` if the battery is already low).
    /// For decisions taken outside the phase machine (resuming a shutdown
    /// after a restart) that must not act on a single glitchy reading.
    pub grid_lost_confirmed: bool,
    pub load_power_w: f64,
    pub battery_power_w: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    GridLostDebouncing,
    OnBattery,
    /// Shutdown sequence has fired. Latched so it never re-fires mid-sequence
    /// or on a flapping grid; only clears after sustained recovery.
    ShutdownLatched,
    RecoveryDebouncing,
}

/// Debounced state machine that decides when to fire the site shutdown
/// sequence and when to send Wake-on-LAN on recovery.
pub struct StateMachine {
    phase: Phase,
    thresholds: Thresholds,
    phase_entered_at: Instant,
    /// What the last valid reading said about the grid. Drives the
    /// comms-loss fail-safe: losing the inverter only matters if we were
    /// running from the battery when it went quiet.
    last_grid_lost: bool,
    /// Since when `grid_lost` has held without a break (None: grid present).
    grid_lost_since: Option<Instant>,
    /// Low-SOC flags of the last LOW_SOC_WINDOW readings, newest last.
    recent_soc_low: VecDeque<bool>,
    /// Set when the shutdown sequence has actually been fired, cleared once
    /// the matching Wake-on-LAN has been sent on recovery. This is what
    /// stops an ordinary grid blip (one that never reached low SOC) from
    /// triggering a pointless WOL broadcast on recovery -- WOL only fires
    /// for a recovery that follows a real shutdown.
    shutdown_fired: bool,
}

pub enum Action {
    None,
    /// Fire the shutdown sequence exactly once.
    TriggerShutdownSequence,
    /// The grid has been back for the full recovery debounce, following a
    /// shutdown this process actually triggered -- send WOL.
    TriggerWakeOnLan,
}

impl StateMachine {
    /// `resume_after_shutdown`: a previous run fired the shutdown sequence
    /// and never finished the matching Wake-on-LAN (see `persist`). Start
    /// latched, as if this process had fired it, so recovery still wakes
    /// everything -- and a still-down grid doesn't fire it a second time.
    pub fn new(thresholds: Thresholds, resume_after_shutdown: bool) -> Self {
        Self {
            phase: if resume_after_shutdown {
                Phase::ShutdownLatched
            } else {
                Phase::Idle
            },
            thresholds,
            phase_entered_at: Instant::now(),
            last_grid_lost: false,
            grid_lost_since: None,
            recent_soc_low: VecDeque::with_capacity(LOW_SOC_WINDOW),
            shutdown_fired: resume_after_shutdown,
        }
    }

    fn enter(&mut self, phase: Phase) {
        log::info!("state: {:?} -> {:?}", self.phase, phase);
        self.phase = phase;
        self.phase_entered_at = Instant::now();
    }

    fn elapsed_in_phase(&self) -> Duration {
        self.phase_entered_at.elapsed()
    }

    fn fire_shutdown(&mut self, reading: &InverterReading) -> Action {
        log::warn!(
            "SOC {:.1}% <= threshold {:.1}% ({} of the last {} readings) while on battery -- firing shutdown sequence",
            reading.battery_soc_pct,
            self.thresholds.low_battery_soc,
            self.recent_soc_low.iter().filter(|&&low| low).count(),
            self.recent_soc_low.len()
        );
        self.enter(Phase::ShutdownLatched);
        self.shutdown_fired = true;
        Action::TriggerShutdownSequence
    }

    /// Called on every poll cycle that produced no valid reading (inverter
    /// silent, adapter missing, settings refused), with how long it has been
    /// since the last valid one.
    ///
    /// Fail-safe: if that last reading said the grid was lost, the battery
    /// has been draining unobserved; once `comms_loss_shutdown_secs` have
    /// passed, fire the sequence rather than wait for the inverter's hard
    /// cutoff. Never fires when the grid was present at the last reading, or
    /// when the sequence has already been fired. A later valid reading is
    /// handled as usual: grid back -> recovery -> Wake-on-LAN.
    pub fn comms_lost(&mut self, since_last_reading: Duration) -> Action {
        let limit = self.thresholds.comms_loss_shutdown_secs;
        if limit == 0
            || !self.last_grid_lost
            || self.shutdown_fired
            || since_last_reading < Duration::from_secs(limit)
        {
            return Action::None;
        }
        log::error!(
            "no valid inverter reading for {} s while the grid was lost at the last one -- \
             battery state unknown, firing shutdown sequence (comms_loss_shutdown_secs = {})",
            since_last_reading.as_secs(),
            limit
        );
        self.enter(Phase::ShutdownLatched);
        self.shutdown_fired = true;
        Action::TriggerShutdownSequence
    }

    /// Feed one poll result in; returns the status to publish and any action
    /// to take as a result of this transition.
    pub fn observe(&mut self, reading: InverterReading) -> (BridgeStatus, Action) {
        // Lost if the voltage is low OR the inverter itself has opened its
        // grid relay (it does that on a sagging grid while the voltage still
        // reads above the threshold). An unreadable relay (None) counts as
        // "not open": voltage alone decides, as before. Recovery needs both
        // signals healthy, so a relay that is still open keeps us on battery.
        let voltage_lost = reading.grid_voltage < self.thresholds.grid_lost_voltage;
        let relay_open = self.thresholds.use_grid_relay && reading.grid_relay_closed == Some(false);
        let grid_lost = voltage_lost || relay_open;
        self.last_grid_lost = grid_lost;
        if !grid_lost {
            self.grid_lost_since = None;
        } else if self.grid_lost_since.is_none() {
            self.grid_lost_since = Some(Instant::now());
        }
        if self.recent_soc_low.len() == LOW_SOC_WINDOW {
            self.recent_soc_low.pop_front();
        }
        self.recent_soc_low
            .push_back(reading.battery_soc_pct <= self.thresholds.low_battery_soc);
        let soc_low = self.recent_soc_low.iter().filter(|&&low| low).count() >= LOW_SOC_NEEDED;
        let confirmed_debounce = if self.thresholds.low_battery_fast_debounce_secs > 0 && soc_low {
            self.thresholds
                .on_battery_debounce_secs
                .min(self.thresholds.low_battery_fast_debounce_secs)
        } else {
            self.thresholds.on_battery_debounce_secs
        };
        let grid_lost_confirmed = self.grid_lost_since.is_some_and(|t| {
            t.elapsed() >= Duration::from_secs(confirmed_debounce)
        });
        let mut action = Action::None;

        match self.phase {
            Phase::Idle => {
                if grid_lost {
                    self.enter(Phase::GridLostDebouncing);
                }
            }
            Phase::GridLostDebouncing => {
                if !grid_lost {
                    self.enter(Phase::Idle);
                } else {
                    let fast_limit = self.thresholds.low_battery_fast_debounce_secs;
                    let fast_due = fast_limit > 0
                        && soc_low
                        && self.elapsed_in_phase() >= Duration::from_secs(fast_limit);
                    let normal_due = self.elapsed_in_phase()
                        >= Duration::from_secs(self.thresholds.on_battery_debounce_secs);

                    if fast_due || normal_due {
                        self.enter(Phase::OnBattery);
                        if soc_low {
                            action = self.fire_shutdown(&reading);
                        }
                    }
                }
            }
            Phase::OnBattery => {
                if !grid_lost {
                    self.enter(Phase::RecoveryDebouncing);
                } else if soc_low {
                    action = self.fire_shutdown(&reading);
                }
            }
            Phase::ShutdownLatched => {
                // Grid back: start the recovery countdown whatever the SOC --
                // the endpoints are woken once the grid has held for
                // recovery_debounce_secs. While the grid stays down, stay
                // latched: the sequence is never sent twice.
                if !grid_lost {
                    self.enter(Phase::RecoveryDebouncing);
                }
            }
            Phase::RecoveryDebouncing => {
                if grid_lost {
                    // Grid dropped again before we finished confirming
                    // recovery. Only return to the latch if the sequence was
                    // actually sent; otherwise a low SOC here must fire it now,
                    // or nothing would ever shut the endpoints down.
                    if self.shutdown_fired {
                        self.enter(Phase::ShutdownLatched);
                    } else if soc_low {
                        action = self.fire_shutdown(&reading);
                    } else {
                        self.enter(Phase::OnBattery);
                    }
                } else if self.elapsed_in_phase()
                    >= Duration::from_secs(self.thresholds.recovery_debounce_secs)
                {
                    self.enter(Phase::Idle);
                    if self.shutdown_fired {
                        self.shutdown_fired = false;
                        action = Action::TriggerWakeOnLan;
                    }
                }
            }
        }

        let status = BridgeStatus {
            on_battery: !matches!(self.phase, Phase::Idle),
            low_battery: matches!(self.phase, Phase::ShutdownLatched),
            battery_soc_pct: reading.battery_soc_pct,
            grid_voltage: reading.grid_voltage,
            grid_relay_closed: reading.grid_relay_closed,
            grid_lost,
            grid_lost_confirmed,
            load_power_w: reading.load_power_w,
            battery_power_w: reading.battery_power_w,
        };

        (status, action)
    }

    #[cfg(test)]
    pub(crate) fn backdate_phase_entered_at(&mut self, d: Duration) {
        self.phase_entered_at -= d;
    }

    #[cfg(test)]
    pub(crate) fn backdate_grid_lost_since(&mut self, d: Duration) {
        if let Some(ref mut t) = self.grid_lost_since {
            *t -= d;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zero debounces so each poll can advance a phase; recovery debounce is
    /// long so RecoveryDebouncing can't complete mid-test unless asked to.
    fn thresholds(recovery_debounce_secs: u64) -> Thresholds {
        Thresholds {
            grid_lost_voltage: 100.0,
            use_grid_relay: true,
            on_battery_debounce_secs: 0,
            low_battery_fast_debounce_secs: 0,
            low_battery_soc: 30.0,
            inverter_cutoff_soc: 20.0,
            recovery_debounce_secs,
            stagger_secs: 0,
            wol_resend_count: 0,
            wol_resend_interval_secs: 0,
            ssh_connect_retry_secs: 0,
            comms_loss_shutdown_secs: 0,
        }
    }

    fn reading(grid_voltage: f64, soc: f64) -> InverterReading {
        InverterReading {
            battery_soc_pct: soc,
            grid_voltage,
            grid_relay_closed: None,
            load_power_w: 0.0,
            battery_power_w: 0.0,
        }
    }

    /// Same, with register 194 (the grid relay) as well.
    fn reading_relay(grid_voltage: f64, soc: f64, relay_closed: bool) -> InverterReading {
        InverterReading {
            grid_relay_closed: Some(relay_closed),
            ..reading(grid_voltage, soc)
        }
    }

    const GRID_UP: f64 = 230.0;
    const GRID_DOWN: f64 = 0.0;

    fn is_shutdown(a: &Action) -> bool {
        matches!(a, Action::TriggerShutdownSequence)
    }

    #[test]
    fn grid_present_never_shuts_down() {
        let mut sm = StateMachine::new(thresholds(3600), false);
        for _ in 0..5 {
            let (_, a) = sm.observe(reading(GRID_UP, 5.0));
            assert!(matches!(a, Action::None));
        }
    }

    /// Grid lost, then on battery (the debounce is 0 in these tests).
    fn on_battery(sm: &mut StateMachine, soc: f64) {
        sm.observe(reading(GRID_DOWN, soc)); // -> GridLostDebouncing
        sm.observe(reading(GRID_DOWN, soc)); // -> OnBattery
    }

    const GRID_SAG: f64 = 150.0; // above grid_lost_voltage, but the inverter has let go

    #[test]
    fn sagging_grid_with_open_relay_counts_as_lost() {
        // The voltage register still reads 150 V (> 100 V), but the inverter
        // has opened its grid relay and runs from the battery. Voltage alone
        // would never notice; the relay must.
        let mut sm = StateMachine::new(thresholds(3600), false);
        sm.observe(reading_relay(GRID_SAG, 50.0, false)); // -> GridLostDebouncing
        sm.observe(reading_relay(GRID_SAG, 50.0, false)); // -> OnBattery
        let (status, a) = sm.observe(reading_relay(GRID_SAG, 29.0, false));
        assert!(status.grid_lost, "the published decision must say grid lost");
        assert!(matches!(a, Action::None), "one low reading is not enough");
        assert!(is_shutdown(&sm.observe(reading_relay(GRID_SAG, 28.0, false)).1));
    }

    #[test]
    fn voltage_alone_would_have_missed_the_sag() {
        // Same readings with the relay check switched off: no shutdown ever.
        let mut t = thresholds(3600);
        t.use_grid_relay = false;
        let mut sm = StateMachine::new(t, false);
        for soc in [50.0, 50.0, 29.0, 28.0, 27.0] {
            let (status, a) = sm.observe(reading_relay(GRID_SAG, soc, false));
            assert!(!status.grid_lost);
            assert!(matches!(a, Action::None));
        }
    }

    #[test]
    fn unreadable_relay_falls_back_to_voltage() {
        // reading() has no relay value (None): behaves exactly as before.
        let mut sm = StateMachine::new(thresholds(3600), false);
        for _ in 0..4 {
            let (status, a) = sm.observe(reading(GRID_SAG, 5.0));
            assert!(!status.grid_lost, "no relay info and voltage fine: grid present");
            assert!(matches!(a, Action::None));
        }
        on_battery(&mut sm, 50.0); // GRID_DOWN voltage still works
        sm.observe(reading(GRID_DOWN, 29.0));
        assert!(is_shutdown(&sm.observe(reading(GRID_DOWN, 28.0)).1));
    }

    #[test]
    fn recovery_waits_for_the_relay_to_close() {
        // Voltage is back at 230 V but the inverter has not reconnected yet:
        // still lost, so no recovery countdown and no Wake-on-LAN.
        let mut sm = StateMachine::new(thresholds(0), false);
        on_battery(&mut sm, 50.0);
        sm.observe(reading(GRID_DOWN, 30.0));
        assert!(is_shutdown(&sm.observe(reading(GRID_DOWN, 29.0)).1)); // -> ShutdownLatched
        for _ in 0..3 {
            let (status, a) = sm.observe(reading_relay(GRID_UP, 25.0, false));
            assert!(matches!(a, Action::None));
            assert!(status.grid_lost, "voltage is back but the relay is open: still lost");
            assert!(status.low_battery, "stays latched while the relay is open");
        }
        sm.observe(reading_relay(GRID_UP, 25.0, true)); // -> RecoveryDebouncing
        let (_, a) = sm.observe(reading_relay(GRID_UP, 25.0, true)); // -> Idle
        assert!(matches!(a, Action::TriggerWakeOnLan));
    }

    #[test]
    fn fires_once_when_soc_low_on_battery() {
        let mut sm = StateMachine::new(thresholds(3600), false);
        on_battery(&mut sm, 50.0);
        let (_, a) = sm.observe(reading(GRID_DOWN, 30.0));
        assert!(matches!(a, Action::None), "one low reading is not enough");
        let (_, a) = sm.observe(reading(GRID_DOWN, 29.0));
        assert!(is_shutdown(&a), "second low reading fires");
        let (_, a) = sm.observe(reading(GRID_DOWN, 25.0));
        assert!(matches!(a, Action::None), "must stay latched, not re-fire");
    }

    #[test]
    fn single_low_reading_does_not_fire() {
        // A stray 0 % (e.g. BMS link hiccup) between normal readings.
        let mut sm = StateMachine::new(thresholds(3600), false);
        on_battery(&mut sm, 60.0);
        for soc in [0.0, 60.0, 60.0, 0.0, 60.0, 60.0] {
            let (_, a) = sm.observe(reading(GRID_DOWN, soc));
            assert!(matches!(a, Action::None), "fired on isolated reading {soc}");
        }
    }

    #[test]
    fn two_of_the_last_three_low_readings_fire() {
        let mut sm = StateMachine::new(thresholds(3600), false);
        on_battery(&mut sm, 60.0);
        assert!(matches!(sm.observe(reading(GRID_DOWN, 28.0)).1, Action::None));
        assert!(matches!(sm.observe(reading(GRID_DOWN, 31.0)).1, Action::None));
        assert!(is_shutdown(&sm.observe(reading(GRID_DOWN, 27.0)).1));
    }

    #[test]
    fn grid_flap_then_low_soc_still_fires() {
        // Regression: OnBattery -> brief grid return -> grid lost again with
        // SOC already low used to latch without ever firing the sequence.
        let mut sm = StateMachine::new(thresholds(3600), false);
        on_battery(&mut sm, 35.0);
        sm.observe(reading(GRID_UP, 30.0)); // -> RecoveryDebouncing, 1st low
        let (_, a) = sm.observe(reading(GRID_DOWN, 30.0)); // 2nd low
        assert!(is_shutdown(&a));
    }

    #[test]
    fn grid_flap_after_shutdown_does_not_refire() {
        let mut sm = StateMachine::new(thresholds(3600), false);
        on_battery(&mut sm, 50.0);
        sm.observe(reading(GRID_DOWN, 30.0));
        assert!(is_shutdown(&sm.observe(reading(GRID_DOWN, 30.0)).1));
        sm.observe(reading(GRID_UP, 40.0)); // -> RecoveryDebouncing
        let (_, a) = sm.observe(reading(GRID_DOWN, 40.0));
        assert!(matches!(a, Action::None));
        let (_, a) = sm.observe(reading(GRID_DOWN, 25.0));
        assert!(matches!(a, Action::None));
    }

    #[test]
    fn wol_only_after_a_real_shutdown() {
        // Outage that never reached low SOC: no WOL on recovery.
        let mut sm = StateMachine::new(thresholds(0), false);
        on_battery(&mut sm, 50.0);
        sm.observe(reading(GRID_UP, 50.0)); // -> RecoveryDebouncing
        let (_, a) = sm.observe(reading(GRID_UP, 50.0)); // -> Idle
        assert!(matches!(a, Action::None));

        // Outage that did fire: WOL exactly once on recovery.
        on_battery(&mut sm, 50.0);
        sm.observe(reading(GRID_DOWN, 30.0));
        assert!(is_shutdown(&sm.observe(reading(GRID_DOWN, 30.0)).1));
        sm.observe(reading(GRID_UP, 40.0)); // -> RecoveryDebouncing
        let (_, a) = sm.observe(reading(GRID_UP, 40.0)); // -> Idle
        assert!(matches!(a, Action::TriggerWakeOnLan));
        let (_, a) = sm.observe(reading(GRID_UP, 40.0));
        assert!(matches!(a, Action::None));
    }

    #[test]
    fn wakes_when_grid_is_back_even_with_soc_still_low() {
        // Wake-up waits only for the grid (recovery_debounce_secs), not for
        // the battery to recharge above low_battery_soc.
        let mut sm = StateMachine::new(thresholds(0), false);
        on_battery(&mut sm, 50.0);
        sm.observe(reading(GRID_DOWN, 25.0));
        assert!(is_shutdown(&sm.observe(reading(GRID_DOWN, 24.0)).1));
        sm.observe(reading(GRID_UP, 21.0)); // -> RecoveryDebouncing
        let (_, a) = sm.observe(reading(GRID_UP, 21.0)); // -> Idle
        assert!(matches!(a, Action::TriggerWakeOnLan));
    }

    #[test]
    fn resumed_after_reboot_wakes_on_recovery() {
        // Restarted with the marker present and the grid already back: no
        // new shutdown, and WOL once recovery is confirmed.
        let mut sm = StateMachine::new(thresholds(0), true);
        let (_, a) = sm.observe(reading(GRID_UP, 40.0)); // -> RecoveryDebouncing
        assert!(matches!(a, Action::None));
        let (_, a) = sm.observe(reading(GRID_UP, 40.0)); // -> Idle
        assert!(matches!(a, Action::TriggerWakeOnLan));
    }

    #[test]
    fn resumed_during_outage_does_not_refire() {
        // Restarted mid-outage with the endpoints already down: stay
        // latched, don't send the sequence again.
        let mut sm = StateMachine::new(thresholds(3600), true);
        for soc in [25.0, 22.0, 21.0] {
            let (status, a) = sm.observe(reading(GRID_DOWN, soc));
            assert!(matches!(a, Action::None));
            assert!(status.low_battery);
        }
    }

    fn with_comms_loss(secs: u64) -> Thresholds {
        let mut t = thresholds(0);
        t.comms_loss_shutdown_secs = secs;
        t
    }

    #[test]
    fn comms_loss_on_battery_fires_after_the_limit() {
        let mut sm = StateMachine::new(with_comms_loss(300), false);
        on_battery(&mut sm, 80.0);
        assert!(matches!(sm.comms_lost(Duration::from_secs(299)), Action::None));
        assert!(is_shutdown(&sm.comms_lost(Duration::from_secs(300))));
        // Latched: never fires twice.
        assert!(matches!(sm.comms_lost(Duration::from_secs(900)), Action::None));
        // Readings come back with the grid up -> recovery -> WOL.
        sm.observe(reading(GRID_UP, 80.0)); // -> RecoveryDebouncing
        let (_, a) = sm.observe(reading(GRID_UP, 80.0)); // -> Idle
        assert!(matches!(a, Action::TriggerWakeOnLan));
    }

    #[test]
    fn comms_loss_with_grid_present_never_fires() {
        let mut sm = StateMachine::new(with_comms_loss(300), false);
        sm.observe(reading(GRID_UP, 50.0));
        assert!(matches!(sm.comms_lost(Duration::from_secs(3600)), Action::None));
    }

    #[test]
    fn comms_loss_before_any_reading_never_fires() {
        let mut sm = StateMachine::new(with_comms_loss(300), false);
        assert!(matches!(sm.comms_lost(Duration::from_secs(3600)), Action::None));
    }

    #[test]
    fn comms_loss_disabled_with_zero() {
        let mut sm = StateMachine::new(with_comms_loss(0), false);
        on_battery(&mut sm, 80.0);
        assert!(matches!(sm.comms_lost(Duration::from_secs(3600)), Action::None));
    }

    #[test]
    fn grid_lost_confirmed_needs_the_debounce() {
        // Zero debounce: confirmed on the first lost reading.
        let mut sm = StateMachine::new(thresholds(3600), true);
        assert!(sm.observe(reading(GRID_DOWN, 50.0)).0.grid_lost_confirmed);
        assert!(!sm.observe(reading(GRID_UP, 50.0)).0.grid_lost_confirmed);

        // Long debounce: a single glitch (or several within it) is not confirmed.
        let mut t = thresholds(3600);
        t.on_battery_debounce_secs = 3600;
        let mut sm = StateMachine::new(t, true);
        for _ in 0..3 {
            let (status, _) = sm.observe(reading(GRID_DOWN, 50.0));
            assert!(status.grid_lost);
            assert!(!status.grid_lost_confirmed);
        }
    }

    #[test]
    fn fast_debounce_fires_early_when_battery_already_low() {
        let mut t = thresholds(3600);
        t.on_battery_debounce_secs = 60;
        t.low_battery_fast_debounce_secs = 15;
        let mut sm = StateMachine::new(t, false);

        // Outage starts with battery already low (28% <= 30%)
        let (_, a1) = sm.observe(reading(GRID_DOWN, 28.0));
        assert!(matches!(a1, Action::None)); // -> GridLostDebouncing, 1st low

        // Advance 14s (less than 15s fast debounce)
        sm.backdate_phase_entered_at(Duration::from_secs(14));
        let (_, a2) = sm.observe(reading(GRID_DOWN, 28.0));
        assert!(matches!(a2, Action::None), "must not fire before fast debounce limit");

        // Advance past 15s: fast debounce expires and fires immediately
        sm.backdate_phase_entered_at(Duration::from_secs(1));
        let (status, a3) = sm.observe(reading(GRID_DOWN, 28.0));
        assert!(is_shutdown(&a3), "must fire after low_battery_fast_debounce_secs");
        assert!(status.low_battery);
        assert!(status.on_battery);
    }

    #[test]
    fn fast_debounce_filters_short_blip_with_low_battery() {
        let mut t = thresholds(3600);
        t.on_battery_debounce_secs = 60;
        t.low_battery_fast_debounce_secs = 15;
        let mut sm = StateMachine::new(t, false);

        // Outage starts with low battery
        sm.observe(reading(GRID_DOWN, 28.0));
        sm.backdate_phase_entered_at(Duration::from_secs(10));
        sm.observe(reading(GRID_DOWN, 28.0)); // 2nd low reading

        // Grid returns after 10 s (shorter than 15 s)
        let (status, a) = sm.observe(reading(GRID_UP, 28.0));
        assert!(matches!(a, Action::None), "blip must be ignored");
        assert!(!status.on_battery, "must return to Idle");
        assert!(!status.grid_lost);
    }

    #[test]
    fn fast_debounce_does_not_fire_when_battery_normal() {
        let mut t = thresholds(3600);
        t.on_battery_debounce_secs = 60;
        t.low_battery_fast_debounce_secs = 15;
        let mut sm = StateMachine::new(t, false);

        // Outage starts with battery healthy (80%)
        sm.observe(reading(GRID_DOWN, 80.0));
        sm.backdate_phase_entered_at(Duration::from_secs(30)); // past 15s fast debounce
        let (status, a) = sm.observe(reading(GRID_DOWN, 80.0));
        assert!(matches!(a, Action::None), "must not fire while battery is healthy");
        // Still in GridLostDebouncing because 60s has not elapsed
        assert!(matches!(sm.phase, Phase::GridLostDebouncing));
        assert!(!status.low_battery);

        // Once 60s normal debounce elapses, enters OnBattery
        sm.backdate_phase_entered_at(Duration::from_secs(30));
        let (status, a) = sm.observe(reading(GRID_DOWN, 80.0));
        assert!(matches!(a, Action::None));
        assert!(matches!(sm.phase, Phase::OnBattery));
        assert!(status.on_battery);
    }

    #[test]
    fn fast_debounce_disabled_with_zero() {
        let mut t = thresholds(3600);
        t.on_battery_debounce_secs = 60;
        t.low_battery_fast_debounce_secs = 0; // disabled
        let mut sm = StateMachine::new(t, false);

        // Outage with battery already low
        sm.observe(reading(GRID_DOWN, 25.0));
        sm.backdate_phase_entered_at(Duration::from_secs(30));
        let (_, a) = sm.observe(reading(GRID_DOWN, 25.0));
        assert!(matches!(a, Action::None), "fast debounce disabled -- must not fire early");
        assert!(matches!(sm.phase, Phase::GridLostDebouncing));

        // Waits for the full 60s normal debounce, then fires
        sm.backdate_phase_entered_at(Duration::from_secs(30));
        let (_, a) = sm.observe(reading(GRID_DOWN, 25.0));
        assert!(is_shutdown(&a), "must fire once full debounce elapses");
    }

    #[test]
    fn grid_lost_confirmed_respects_fast_debounce_on_low_battery() {
        let mut t = thresholds(3600);
        t.on_battery_debounce_secs = 60;
        t.low_battery_fast_debounce_secs = 15;
        let mut sm = StateMachine::new(t, true); // resume = true

        // 1st low reading
        sm.observe(reading(GRID_DOWN, 25.0));
        sm.backdate_grid_lost_since(Duration::from_secs(10));
        let (status, _) = sm.observe(reading(GRID_DOWN, 25.0)); // 2nd low reading
        assert!(!status.grid_lost_confirmed, "not confirmed at 10s");

        sm.backdate_grid_lost_since(Duration::from_secs(5));
        let (status, _) = sm.observe(reading(GRID_DOWN, 25.0));
        assert!(status.grid_lost_confirmed, "confirmed after 15s fast debounce with low battery");
    }
}
