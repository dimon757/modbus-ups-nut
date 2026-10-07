use crate::config::ModbusConfig;
use anyhow::{anyhow, bail, Result};
use tokio_modbus::client::rtu;
use tokio_modbus::prelude::*;
use std::time::Duration;
use tokio_serial::SerialStream;

/// Per-register read timeout. A healthy RTU round trip at 9600 baud is tens
/// of milliseconds; this only needs to be well short of the watchdog timeout.
const READ_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Sunsynk register map -- the one place it is defined. Addresses, units and
// scales from the Sunsynk/Deye "Modbus RTU Protocol" document (V117), copy in
// docs/protocol/Sunsynk-Modbus-Protocol-V117.pdf, section
// for storage inverters (V117; identical for every register below in V119
// except the unit note on 54 -- see docs/protocol-versions.md). All holding
// registers, read with function 0x03 at
// exactly these (decimal) addresses. To adapt to another firmware, change
// them here and rebuild; the test simulator (test/inverter_sim.py) keeps its
// own copy on purpose, so update it too.
//
// Some addresses mean something else on other inverter types (e.g. 190 is
// "string 13 energy" on a string inverter) -- which is why REG_DEVICE_TYPE is
// checked on every connect.
// ---------------------------------------------------------------------------

/// Device type. 0x0300 = single-phase (low-voltage) storage inverter.
const REG_DEVICE_TYPE: u16 = 0;
pub const DEVICE_TYPE_SINGLE_PHASE_STORAGE: u16 = 0x0300;

/// "Communication protocol version" the firmware follows, e.g. 0x0102 =
/// 1.2. Logged only, to tell which protocol document applies (see
/// docs/protocol-versions.md).
const REG_PROTOCOL_VERSION: u16 = 2;

/// Logged only. V119: "AC power ratio" -- whether the power registers
/// (incl. 178 and 190) are in 1 W or 10 W units. V117: "EEPROM initial
/// enabled", a command register -- harmless to READ; the bridge never
/// writes. See docs/protocol-versions.md.
const REG_AC_POWER_RATIO: u16 = 54;

/// Grid side voltage L1-N, 0.1 V.
const REG_GRID_VOLTAGE: u16 = 150;
const GRID_VOLTAGE_SCALE: f64 = 10.0;

/// Grid side relay status: 0 = open (the inverter is disconnected from the
/// grid and running from the battery), 1 = closed. Unlike the voltage above,
/// this is what the inverter actually decided: on a sagging grid it opens
/// the relay while register 150 can still read well above `grid_lost_voltage`.
/// Optional on purpose: if the firmware doesn't answer for it, the bridge
/// falls back to the voltage alone.
const REG_GRID_RELAY: u16 = 194;

/// Load side total power, 1 W, signed int.
const REG_LOAD_POWER: u16 = 178;

/// Battery capacity (SOC), 1 %, range 0-100.
const REG_BATTERY_SOC: u16 = 184;

/// Battery output power, 1 W, signed int. The document doesn't say which
/// sign means charging; Deye's usual convention is + discharging /
/// - charging. Unverified on this unit -- it is only logged, never decided on.
const REG_BATTERY_POWER: u16 = 190;

/// How the inverter manages the battery: 0 = by voltage, 1 = by capacity
/// (SOC), 2 = no battery.
const REG_BATTERY_MODE: u16 = 213;

/// "Battery capacity ShutDown": the inverter's own low-SOC cutoff, 1 %.
const REG_BATTERY_SHUTDOWN_SOC: u16 = 217;

/// "Battery voltage ShutDown": the inverter's own low-voltage cutoff, 0.01 V.
const REG_BATTERY_SHUTDOWN_VOLTAGE: u16 = 220;
const BATTERY_VOLTAGE_SCALE: f64 = 100.0;

/// A single poll of the inverter's relevant registers.
#[derive(Debug, Clone, Copy)]
pub struct InverterReading {
    pub battery_soc_pct: f64,
    pub grid_voltage: f64,
    /// Register 194: `Some(true)` relay closed (on grid), `Some(false)` open
    /// (off grid), `None` if the register is unreadable or holds another value.
    pub grid_relay_closed: Option<bool>,
    pub load_power_w: f64,
    pub battery_power_w: f64,
}

pub struct ModbusClient {
    ctx: tokio_modbus::client::Context,
}

impl ModbusClient {
    pub fn connect(cfg: &ModbusConfig) -> Result<Self> {
        let builder = tokio_serial::new(&cfg.device, cfg.baud_rate);
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut port = SerialStream::open(&builder)
            .map_err(|e| anyhow!("opening serial port {}: {}", cfg.device, e))?;
        // The serial crate opens ports in exclusive mode (TIOCEXCL). It adds
        // nothing here -- the bridge runs as root, and root ignores it, as
        // would anything else likely to touch the port -- but it does bite:
        // if the bridge is killed rather than exiting cleanly, the lock can
        // outlive it (it does on the test kit's virtual cable) and lock the
        // restarted bridge out of its own port.
        #[cfg(unix)]
        port.set_exclusive(false)
            .map_err(|e| anyhow!("clearing exclusive mode on {}: {}", cfg.device, e))?;
        let ctx = rtu::attach_slave(port, Slave(cfg.slave_id));
        Ok(Self { ctx })
    }

    /// Read one holding register (function code 0x03). One request per
    /// register keeps each value's address explicit; four small requests per
    /// poll are nothing at 9600 baud.
    async fn read_one(&mut self, addr: u16) -> Result<u16> {
        // Bounded: a silent inverter (unplugged cable, wrong slave id) must
        // surface as a poll error the main loop can log and retry, not hang
        // the loop until the watchdog reboots the board.
        let rsp = tokio::time::timeout(READ_TIMEOUT, self.ctx.read_holding_registers(addr, 1))
            .await
            .map_err(|_| anyhow!("timed out reading register {:#06x}", addr))?
            .map_err(|e| anyhow!("modbus error reading {:#06x}: {}", addr, e))?;
        rsp.first()
            .copied()
            .ok_or_else(|| anyhow!("empty response reading register {:#06x}", addr))
    }

    pub async fn poll(&mut self) -> Result<InverterReading> {
        let soc_raw = self.read_one(REG_BATTERY_SOC).await?;
        let grid_raw = self.read_one(REG_GRID_VOLTAGE).await?;
        let load_raw = self.read_one(REG_LOAD_POWER).await?;
        let batt_raw = self.read_one(REG_BATTERY_POWER).await?;
        // Optional: an unsupported register gives None (voltage-only
        // detection); a timeout is an error, so the loop reconnects.
        let relay_raw = self.read_optional(REG_GRID_RELAY).await?;

        // The protocol document gives SOC as [0,100]; anything else is a
        // garbled read, which must not be allowed to look like a low battery.
        if soc_raw > 100 {
            bail!("battery SOC register read {} (valid range 0-100)", soc_raw);
        }

        Ok(InverterReading {
            battery_soc_pct: soc_raw as f64,
            grid_voltage: grid_raw as f64 / GRID_VOLTAGE_SCALE,
            grid_relay_closed: decode_grid_relay(relay_raw),
            load_power_w: load_raw as i16 as f64,
            battery_power_w: batt_raw as i16 as f64,
        })
    }

    /// Read an optional holding register. If the inverter returns a Modbus error
    /// (e.g. Illegal Data Address on older firmware), returns Ok(None).
    /// If the read times out, returns an Err so the caller reconnects rather than
    /// leaving stale bytes on the serial line that would desynchronize RTU framing.
    async fn read_optional(&mut self, addr: u16) -> Result<Option<u16>> {
        match tokio::time::timeout(READ_TIMEOUT, self.ctx.read_holding_registers(addr, 1)).await {
            Ok(Ok(rsp)) => Ok(rsp.first().copied()),
            Ok(Err(e)) => {
                log::debug!("optional register {:#06x} not available: {}", addr, e);
                Ok(None)
            }
            Err(_) => bail!("timed out reading register {:#06x}", addr),
        }
    }

    /// Reads the inverter's own identity and battery-protection settings, so
    /// they can be checked against the bridge's thresholds (see
    /// `InverterSettings::findings`).
    pub async fn read_settings(&mut self) -> Result<InverterSettings> {
        // The required registers first: a silent inverter then fails on the
        // first read (one 3 s timeout), keeping well inside the watchdog
        // margin -- rather than first sitting out a timeout on each of the
        // informational ones.
        let device_type = self.read_one(REG_DEVICE_TYPE).await?;
        let battery_mode = self.read_one(REG_BATTERY_MODE).await?;
        let shutdown_soc_pct = self.read_one(REG_BATTERY_SHUTDOWN_SOC).await? as f64;
        let shutdown_voltage =
            self.read_one(REG_BATTERY_SHUTDOWN_VOLTAGE).await? as f64 / BATTERY_VOLTAGE_SCALE;
        Ok(InverterSettings {
            // Informational: an unsupported register returns None cleanly;
            // a timeout is an error that reconnects to keep RTU framing in sync.
            protocol_version: self.read_optional(REG_PROTOCOL_VERSION).await?,
            ac_power_ratio: self.read_optional(REG_AC_POWER_RATIO).await?,
            grid_relay: self.read_optional(REG_GRID_RELAY).await?,
            device_type,
            battery_mode,
            shutdown_soc_pct,
            shutdown_voltage,
        })
    }
}

/// The inverter's own settings that decide when it cuts its output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InverterSettings {
    /// Register 2, if readable.
    pub protocol_version: Option<u16>,
    /// Register 54, if readable.
    pub ac_power_ratio: Option<u16>,
    /// Register 194 raw, if readable (checked at connect; polled every cycle).
    pub grid_relay: Option<u16>,
    pub device_type: u16,
    pub battery_mode: u16,
    pub shutdown_soc_pct: f64,
    pub shutdown_voltage: f64,
}

/// Register 194 -> relay state. Only 0 and 1 are defined; anything else is
/// treated as unknown rather than guessed at.
fn decode_grid_relay(raw: Option<u16>) -> Option<bool> {
    match raw {
        Some(0) => Some(false),
        Some(1) => Some(true),
        Some(other) => {
            log::debug!(
                "grid relay register {} holds {}, expected 0 or 1 -- ignoring",
                REG_GRID_RELAY,
                other
            );
            None
        }
        None => None,
    }
}

impl InverterSettings {
    /// A warning if the grid relay (register 194) can't be used: grid-loss
    /// detection then rests on the voltage alone, which a sagging grid that
    /// the inverter has already disconnected from can fool.
    pub fn grid_relay_finding(&self, use_grid_relay: bool) -> Option<(log::Level, String)> {
        if use_grid_relay && !matches!(self.grid_relay, Some(0) | Some(1)) {
            return Some((
                log::Level::Warn,
                format!(
                    "grid relay register {} is unreadable or not 0/1 -- grid loss is detected from \
                     the grid voltage alone, so a sagging grid that the inverter has already \
                     disconnected from would go unnoticed",
                    REG_GRID_RELAY
                ),
            ));
        }
        None
    }

    /// One log line identifying the firmware's protocol: register 2 raw and
    /// decoded (0x0102 -> "1.2"), register 54 raw, and register 194 (grid relay).
    pub fn protocol_summary(&self) -> String {
        let version = match self.protocol_version {
            Some(v) => format!("{:#06x} ({}.{})", v, v >> 8, v & 0xFF),
            None => "unreadable".into(),
        };
        let ratio = match self.ac_power_ratio {
            Some(v) => v.to_string(),
            None => "unreadable".into(),
        };
        let relay = match self.grid_relay {
            Some(v) => v.to_string(),
            None => "unreadable".into(),
        };
        format!(
            "protocol version (reg 2) {}, reg 54 {}, grid relay (reg 194) {} -- see docs/protocol-versions.md",
            version, ratio, relay
        )
    }

    /// Checks whether the device type (register 0) matches the expected
    /// single-phase storage inverter (0x0300). If false, the data cannot be trusted.
    pub fn is_trusted_device_type(&self) -> bool {
        self.device_type == DEVICE_TYPE_SINGLE_PHASE_STORAGE
    }

    /// Compares the inverter's live cutoff settings with the bridge's
    /// thresholds. The config's `inverter_cutoff_soc` is only what someone
    /// typed in; this is what the inverter will actually do.
    pub fn findings(
        &self,
        low_battery_soc: f64,
        configured_cutoff_soc: f64,
    ) -> Vec<(log::Level, String)> {
        use log::Level::{Error, Info, Warn};
        let mut out = Vec::new();

        if self.device_type != DEVICE_TYPE_SINGLE_PHASE_STORAGE {
            out.push((
                Error,
                format!(
                    "device type {:#06x}, expected {:#06x} (single-phase storage inverter) -- \
                     check the serial port and slave id point at the Sunsynk",
                    self.device_type, DEVICE_TYPE_SINGLE_PHASE_STORAGE
                ),
            ));
        }

        match self.battery_mode {
            1 => {
                if low_battery_soc <= self.shutdown_soc_pct {
                    out.push((
                        Error,
                        format!(
                            "inverter cuts its output at {:.0}% SOC, but low_battery_soc is \
                             {:.0}% -- it will cut power before the shutdown sequence starts. \
                             Raise low_battery_soc well above {:.0}%.",
                            self.shutdown_soc_pct, low_battery_soc, self.shutdown_soc_pct
                        ),
                    ));
                } else {
                    out.push((
                        Info,
                        format!(
                            "inverter cutoff {:.0}% SOC, shutdown sequence at {:.0}% -- {:.0} points of margin",
                            self.shutdown_soc_pct,
                            low_battery_soc,
                            low_battery_soc - self.shutdown_soc_pct
                        ),
                    ));
                }
                if self.shutdown_soc_pct != configured_cutoff_soc {
                    out.push((
                        Error,
                        format!(
                            "config inverter_cutoff_soc is {:.0}% but the inverter is set to \
                             {:.0}% -- update the config to match",
                            configured_cutoff_soc, self.shutdown_soc_pct
                        ),
                    ));
                }
            }
            0 => out.push((
                Warn,
                format!(
                    "inverter manages the battery by VOLTAGE: it cuts output at {:.2} V \
                     whatever the SOC, so the low_battery_soc margin can't be checked. \
                     Switch the inverter's battery setting to capacity (%) mode, or make \
                     sure {:.0}% SOC is reached well before {:.2} V.",
                    self.shutdown_voltage, low_battery_soc, self.shutdown_voltage
                ),
            )),
            2 => out.push((
                Error,
                "inverter is configured with NO battery -- SOC readings mean nothing".into(),
            )),
            other => out.push((Warn, format!("unknown battery mode {} in register 213", other))),
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(battery_mode: u16, shutdown_soc_pct: f64) -> InverterSettings {
        InverterSettings {
            protocol_version: Some(0x0102),
            ac_power_ratio: Some(0),
            grid_relay: Some(1),
            device_type: DEVICE_TYPE_SINGLE_PHASE_STORAGE,
            battery_mode,
            shutdown_soc_pct,
            shutdown_voltage: 46.0,
        }
    }

    fn levels(f: &[(log::Level, String)]) -> Vec<log::Level> {
        f.iter().map(|(l, _)| *l).collect()
    }

    #[test]
    fn capacity_mode_with_margin_is_ok() {
        let f = settings(1, 20.0).findings(30.0, 20.0);
        assert_eq!(levels(&f), [log::Level::Info]);
    }

    #[test]
    fn capacity_mode_without_margin_is_an_error() {
        let f = settings(1, 30.0).findings(30.0, 20.0);
        assert_eq!(levels(&f), [log::Level::Error, log::Level::Error]);
    }

    #[test]
    fn voltage_mode_warns() {
        let f = settings(0, 20.0).findings(30.0, 20.0);
        assert_eq!(levels(&f), [log::Level::Warn]);
        assert!(f[0].1.contains("46.00 V"));
    }

    #[test]
    fn wrong_device_type_is_an_error() {
        let mut s = settings(1, 20.0);
        s.device_type = 0x0500;
        let f = s.findings(30.0, 20.0);
        assert_eq!(f[0].0, log::Level::Error);
        assert!(f[0].1.contains("0x0500"));
    }

    #[test]
    fn protocol_summary_decodes_version_and_shows_reg_54() {
        let s = settings(1, 20.0).protocol_summary();
        assert!(s.contains("0x0102 (1.2)"), "{s}");
        assert!(s.contains("reg 54 0"), "{s}");
        assert!(s.contains("grid relay (reg 194) 1"), "{s}");
    }

    #[test]
    fn decode_grid_relay_accepts_only_0_and_1() {
        assert_eq!(decode_grid_relay(Some(0)), Some(false));
        assert_eq!(decode_grid_relay(Some(1)), Some(true));
        assert_eq!(decode_grid_relay(Some(2)), None);
        assert_eq!(decode_grid_relay(None), None);
    }

    #[test]
    fn unreadable_grid_relay_warns_only_when_it_is_used() {
        let mut s = settings(1, 20.0);
        assert!(s.grid_relay_finding(true).is_none(), "a readable relay needs no warning");
        s.grid_relay = None;
        let (level, msg) = s.grid_relay_finding(true).expect("must warn");
        assert_eq!(level, log::Level::Warn);
        assert!(msg.contains("194"), "{msg}");
        // Switched off in the config: nothing to warn about.
        assert!(s.grid_relay_finding(false).is_none());
        // A value other than 0/1 is treated like an unreadable one.
        s.grid_relay = Some(7);
        assert!(s.grid_relay_finding(true).is_some());
    }

    #[test]
    fn protocol_summary_survives_unreadable_registers() {
        let mut st = settings(1, 20.0);
        st.protocol_version = None;
        st.ac_power_ratio = None;
        st.grid_relay = None;
        let s = st.protocol_summary();
        assert!(s.contains("version (reg 2) unreadable"), "{s}");
        assert!(s.contains("reg 54 unreadable"), "{s}");
        assert!(s.contains("reg 194) unreadable"), "{s}");
    }
}
