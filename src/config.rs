use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Top-level bridge configuration, loaded from /etc/modbus-ups-bridge/bridge.toml
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub modbus: ModbusConfig,
    pub thresholds: Thresholds,
    /// Ordered shutdown sequence: whatever order they appear in here is the
    /// order they're shut down in, top to bottom. Put workstations first if
    /// you want people's desktops down before the Proxmox hosts start their own
    /// (already-ordered) guest shutdown.
    pub endpoints: Vec<Endpoint>,
    /// Broadcast address used for Wake-on-LAN magic packets: the endpoints'
    /// subnet broadcast, e.g. "192.168.1.255:9".
    pub wol_broadcast_addr: String,
    /// Where the shutdown marker lives (see `persist`). Must be on
    /// persistent storage. Only worth changing for test runs, so they don't
    /// touch the real service's marker.
    #[serde(default = "default_state_file")]
    pub state_file: String,
    /// known_hosts file ssh checks the endpoints' host keys against. Unset:
    /// ssh's default, ~/.ssh/known_hosts of the service user (root). Host
    /// keys are pinned -- an endpoint missing from it can't be shut down, so
    /// the bridge checks at startup and logs an error for each one.
    #[serde(default)]
    pub ssh_known_hosts_file: Option<String>,
    pub watchdog: Option<WatchdogConfig>,
    /// How Proxmox VE hosts are shut down. Optional section; defaults below.
    #[serde(default)]
    pub proxmox: ProxmoxConfig,
    /// When true, refuse to operate if any inverter setting check yields an Error
    /// (e.g. cutoff SOC mismatch, low battery margin violation, no-battery mode).
    /// When false (default), only refuse if the device type does not match (data cannot be trusted).
    /// All other setting problems are logged at Error level while continuing operation.
    #[serde(default)]
    pub strict_inverter_checks: bool,
}

fn default_state_file() -> String {
    "/var/lib/modbus-ups-bridge/shutdown_fired".into()
}

/// The ways the bridge can shut a Proxmox VE host down.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProxmoxMethod {
    /// `/sbin/poweroff` detached with nohup: Debian/Proxmox's systemd
    /// unit `pve-guests.service` stops all running VMs and containers
    /// gracefully with their configured timeout/ordering, then the host
    /// powers off. One SSH call. (default)
    Poweroff,
    /// The bridge shuts every running VM down in parallel (`qm shutdown <id> --timeout <secs>`),
    /// waits for them concurrently, hard-stops any still running after
    /// `vm_shutdown_timeout_secs` (`qm stop`), then schedules host poweroff.
    VmsThenPoweroff,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProxmoxConfig {
    #[serde(default = "default_proxmox_method")]
    pub method: ProxmoxMethod,
    /// vms_then_poweroff only: how long to wait for the VMs to shut down
    /// cleanly before powering the stragglers off hard. It must fit, with
    /// everything else, between low_battery_soc and the inverter's cutoff.
    #[serde(default = "default_vm_shutdown_timeout_secs")]
    pub vm_shutdown_timeout_secs: u64,
}

impl Default for ProxmoxConfig {
    fn default() -> Self {
        Self {
            method: default_proxmox_method(),
            vm_shutdown_timeout_secs: default_vm_shutdown_timeout_secs(),
        }
    }
}

fn default_proxmox_method() -> ProxmoxMethod {
    ProxmoxMethod::Poweroff
}

fn default_vm_shutdown_timeout_secs() -> u64 {
    300
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ModbusConfig {
    /// Serial device for the RS485 port, e.g. /dev/ttyS0
    pub device: String,
    pub baud_rate: u32,
    pub slave_id: u8,
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub strict_inverter_checks: bool,
    // The register map is not configurable: it belongs to the inverter
    // model, not the site. It is defined once, in src/modbus.rs.
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    /// Grid voltage below this is considered "grid lost" (volts). Together
    /// with the grid relay (see `use_grid_relay`) this is the gate on the
    /// whole shutdown path: while the voltage is above this AND the relay
    /// is closed (or unreadable), nothing here ever shuts anything down,
    /// regardless of SOC.
    pub grid_lost_voltage: f64,
    /// Also treat "grid side relay open" (register 194) as grid lost. The
    /// inverter opens that relay when the grid leaves its accepted window --
    /// e.g. a sag to 150 V -- while the voltage register still reads above
    /// `grid_lost_voltage`. Either signal is enough to count as lost; an
    /// unreadable register falls back to the voltage alone. Set to false
    /// only if the relay register turns out wrong on the real unit.
    #[serde(default = "default_use_grid_relay")]
    pub use_grid_relay: bool,
    /// Grid must be lost for this long before we declare on_battery (seconds).
    pub on_battery_debounce_secs: u64,
    /// SOC (%) at or below which, while on_battery, we start the shutdown
    /// sequence -- once 2 of the last 3 readings are at or below it. MUST be set higher than `inverter_cutoff_soc` below --
    /// checked at startup -- so the graceful sequence has already finished
    /// before the inverter's own hardware protection would cut output.
    pub low_battery_soc: f64,
    /// The SOC (%) at which the inverter's own hardware/firmware shuts its
    /// output off, per its own settings (not something this software
    /// controls -- read it off the inverter's config and put the same
    /// number here so the validation below can catch a misconfiguration).
    pub inverter_cutoff_soc: f64,
    /// The grid must be back for this long before we treat the site as
    /// recovered and send Wake-on-LAN (seconds), whatever the SOC.
    pub recovery_debounce_secs: u64,
    /// Delay between triggering successive endpoints in the shutdown
    /// sequence (seconds).
    pub stagger_secs: u64,
    /// Extra Wake-on-LAN rounds after the first, so a machine that was still
    /// shutting down when the first round went out gets woken once it's off.
    /// `wol_resend_count * wol_resend_interval_secs` should comfortably
    /// exceed the slowest endpoint's shutdown (Proxmox walking its VMs down).
    #[serde(default = "default_wol_resend_count")]
    pub wol_resend_count: u32,
    #[serde(default = "default_wol_resend_interval_secs")]
    pub wol_resend_interval_secs: u64,
    /// How long to keep retrying SSH connection if an endpoint is unreachable
    /// or connection is refused (e.g. still booting from a recent wake-up).
    /// Retries run concurrently without delaying the shutdown of other endpoints.
    #[serde(default = "default_ssh_connect_retry_secs")]
    pub ssh_connect_retry_secs: u64,
}

fn default_wol_resend_count() -> u32 {
    8
}

fn default_use_grid_relay() -> bool {
    true
}

fn default_wol_resend_interval_secs() -> u64 {
    120
}

fn default_ssh_connect_retry_secs() -> u64 {
    300
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EndpointKind {
    Windows,
    Proxmox,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub name: String,
    pub kind: EndpointKind,
    pub host: String,
    pub ssh_user: String,
    pub ssh_key_path: String,
    /// For Proxmox, a `sleep` on the host before `/sbin/poweroff` starts;
    /// for Windows, passed as `shutdown /t` (the user-visible countdown) --
    /// but an sshd `ForceCommand` on the workstation (as the README sets up)
    /// replaces the command we send, so there its own `/t` applies instead.
    pub shutdown_delay_secs: u32,
    /// MAC address for Wake-on-LAN, e.g. "AA:BB:CC:DD:EE:FF". Required: this
    /// is how the endpoint comes back up after a graceful shutdown, since
    /// its NIC standby power (and so its WOL listener) survives a clean
    /// ACPI shutdown even though the machine looks fully off.
    pub mac_address: String,
    /// Optional per-endpoint override for SSH connection retry budget (seconds).
    /// If not specified, falls back to `thresholds.ssh_connect_retry_secs`.
    #[serde(default)]
    pub ssh_connect_retry_secs: Option<u64>,
}

impl Endpoint {
    pub fn effective_ssh_connect_retry_secs(&self, default_secs: u64) -> u64 {
        self.ssh_connect_retry_secs.unwrap_or(default_secs)
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct WatchdogConfig {
    pub device: String,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("reading config file {:?}", path.as_ref()))?;
        let cfg: Config = toml::from_str(&raw).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn strict_inverter_checks(&self) -> bool {
        self.strict_inverter_checks || self.modbus.strict_inverter_checks
    }

    fn validate(&self) -> Result<()> {
        let t = &self.thresholds;
        if !(0.0..=100.0).contains(&t.inverter_cutoff_soc) || !(0.0..=100.0).contains(&t.low_battery_soc) {
            bail!(
                "thresholds.low_battery_soc ({}) and inverter_cutoff_soc ({}) are percentages: 0-100",
                t.low_battery_soc,
                t.inverter_cutoff_soc
            );
        }
        if !(1.0..=400.0).contains(&t.grid_lost_voltage) {
            bail!(
                "thresholds.grid_lost_voltage is {:.1} V -- expected 1.0 to 400.0 V",
                t.grid_lost_voltage
            );
        }
        if t.on_battery_debounce_secs > 3600 {
            bail!(
                "thresholds.on_battery_debounce_secs is {} s -- must be <= 3600",
                t.on_battery_debounce_secs
            );
        }
        if t.recovery_debounce_secs > 3600 {
            bail!(
                "thresholds.recovery_debounce_secs is {} s -- must be <= 3600",
                t.recovery_debounce_secs
            );
        }
        if t.stagger_secs > 300 {
            bail!(
                "thresholds.stagger_secs is {} s -- must be <= 300",
                t.stagger_secs
            );
        }
        if t.ssh_connect_retry_secs > 1800 {
            bail!(
                "thresholds.ssh_connect_retry_secs is {} s -- must be <= 1800",
                t.ssh_connect_retry_secs
            );
        }
        // The watchdog must be fed more often than its timeout (30 s for the
        // N2840's iTCO_wdt). The longest gap between feeds is one poll
        // interval plus up to 15 s of settings reads at a (re)connect (four
        // required reads, then an optional one that times out at 3 s) --
        // 25 s with 10 s polling.
        if !(1..=10).contains(&self.modbus.poll_interval_secs) {
            bail!(
                "modbus.poll_interval_secs is {} -- must be 1-10, or the watchdog \
                 (30 s) could reboot the box between polls",
                self.modbus.poll_interval_secs
            );
        }
        // Checked now, not at the first real recovery -- a typo here would
        // otherwise only show up as a failed wake-up after an outage.
        self.wol_broadcast_addr.parse::<std::net::SocketAddr>().map_err(|_| {
            anyhow::anyhow!(
                "wol_broadcast_addr {:?} is not an IP:port, e.g. \"192.168.1.255:9\"",
                self.wol_broadcast_addr
            )
        })?;
        if !(10..=1800).contains(&self.proxmox.vm_shutdown_timeout_secs) {
            bail!(
                "proxmox.vm_shutdown_timeout_secs is {} -- must be 10-1800",
                self.proxmox.vm_shutdown_timeout_secs
            );
        }
        if self.proxmox.method == ProxmoxMethod::VmsThenPoweroff {
            let total_wol_window = u64::from(t.wol_resend_count) * t.wol_resend_interval_secs;
            if total_wol_window < self.proxmox.vm_shutdown_timeout_secs {
                bail!(
                    "WOL window ({} resends x {}s = {}s) is shorter than proxmox.vm_shutdown_timeout_secs ({}s) -- \
                     the marker could be cleared before the host finishes powering off",
                    t.wol_resend_count,
                    t.wol_resend_interval_secs,
                    total_wol_window,
                    self.proxmox.vm_shutdown_timeout_secs
                );
            }
        }
        let mut names = std::collections::HashSet::new();
        for ep in &self.endpoints {
            crate::wol::parse_mac(&ep.mac_address)
                .with_context(|| format!("endpoint {:?}: mac_address", ep.name))?;
            if let Some(r) = ep.ssh_connect_retry_secs {
                if r > 1800 {
                    bail!(
                        "endpoint {:?}: ssh_connect_retry_secs is {} s -- must be <= 1800",
                        ep.name,
                        r
                    );
                }
            }
            if !names.insert(ep.name.as_str()) {
                bail!("two endpoints are named {:?} -- names must be unique", ep.name);
            }
        }
        if self.thresholds.low_battery_soc <= self.thresholds.inverter_cutoff_soc {
            bail!(
                "thresholds.low_battery_soc ({:.1}%) must be higher than \
                 thresholds.inverter_cutoff_soc ({:.1}%) -- as configured, the \
                 inverter would cut its own output at or before we've even \
                 started the shutdown sequence, which defeats the entire point \
                 of this software. Refusing to start.",
                self.thresholds.low_battery_soc,
                self.thresholds.inverter_cutoff_soc
            );
        }
        if self.endpoints.is_empty() {
            bail!("no [[endpoints]] configured -- nothing to shut down or wake");
        }
        Ok(())
    }

    /// Problems with the SSH key files that would make a shutdown fail when
    /// it matters. Reported at startup (not fatal: the other endpoints are
    /// still worth protecting).
    pub fn ssh_key_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for ep in &self.endpoints {
            if !seen.insert(ep.ssh_key_path.as_str()) {
                continue;
            }
            match std::fs::metadata(&ep.ssh_key_path) {
                Err(e) => out.push(format!(
                    "SSH key {} (endpoint {}) can't be read: {} -- its shutdown will fail",
                    ep.ssh_key_path, ep.name, e
                )),
                #[cfg(unix)]
                Ok(m) => {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = m.permissions().mode() & 0o777;
                    if mode & 0o077 != 0 {
                        out.push(format!(
                            "SSH key {} has permissions {:o} -- ssh refuses keys readable by \
                             others; run: chmod 600 {}",
                            ep.ssh_key_path, mode, ep.ssh_key_path
                        ));
                    }
                }
                #[cfg(not(unix))]
                Ok(_) => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_loads() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/bridge.toml.example");
        let cfg = Config::load(path).expect("example config should load and validate");
        assert_eq!(cfg.wol_broadcast_addr, "192.168.1.255:9");
        assert_eq!(cfg.endpoints.len(), 4);
        assert_eq!(cfg.state_file, "/var/lib/modbus-ups-bridge/shutdown_fired");
    }

    #[test]
    fn level2_test_config_loads() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/bridge-test.toml");
        let cfg = Config::load(path).expect("test/bridge-test.toml should load and validate");
        assert_eq!(cfg.state_file, "/tmp/mub-test/shutdown_fired");
        assert!(cfg.watchdog.is_none(), "a test run must never hold the watchdog");
        assert_eq!(cfg.ssh_known_hosts_file.as_deref(), Some("/tmp/mub-test/known_hosts"));
        assert_eq!(cfg.proxmox.method, ProxmoxMethod::Poweroff);

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/bridge-test-vms.toml");
        let cfg = Config::load(path).expect("test/bridge-test-vms.toml should load and validate");
        assert!(cfg.watchdog.is_none(), "a test run must never hold the watchdog");
        assert_eq!(cfg.proxmox.method, ProxmoxMethod::VmsThenPoweroff);
    }

    fn example_with(from: &str, to: &str) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/bridge.toml.example");
        let raw = std::fs::read_to_string(path).unwrap();
        let changed = raw.replacen(from, to, 1);
        assert_ne!(changed, raw, "example config no longer contains {from:?}");
        changed
    }

    fn validate_err(toml_text: &str) -> String {
        let cfg: Config = toml::from_str(toml_text).expect("should parse");
        format!("{:#}", cfg.validate().expect_err("should fail validation"))
    }

    #[test]
    fn proxmox_section_is_optional_and_defaults_to_poweroff() {
        // The example without its [proxmox] section, as an older config would be.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/bridge.toml.example");
        let raw = std::fs::read_to_string(path).unwrap();
        let without = &raw[..raw.find("[proxmox]").expect("example has a [proxmox] section")];
        let cfg: Config = toml::from_str(without).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.proxmox.method, ProxmoxMethod::Poweroff);
        assert_eq!(cfg.proxmox.vm_shutdown_timeout_secs, 300);
        assert_eq!(cfg.ssh_known_hosts_file, None);
    }

    #[test]
    fn proxmox_vms_then_poweroff_can_be_selected() {
        let text = example_with("method = \"poweroff\"", "method = \"vms_then_poweroff\"");
        let cfg: Config = toml::from_str(&text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.proxmox.method, ProxmoxMethod::VmsThenPoweroff);
    }

    #[test]
    fn proxmox_bad_method_or_timeout_is_rejected() {
        let bad_method = example_with("method = \"poweroff\"", "method = \"invalid_method\"");
        assert!(toml::from_str::<Config>(&bad_method).is_err());
        let err = validate_err(&example_with(
            "vm_shutdown_timeout_secs = 300",
            "vm_shutdown_timeout_secs = 5",
        ));
        assert!(err.contains("vm_shutdown_timeout_secs"), "{err}");
    }

    #[test]
    fn bad_mac_is_rejected_at_startup() {
        let err = validate_err(&example_with("AA:BB:CC:DD:EE:03", "AA:BB:CC:DD:EE"));
        assert!(err.contains("proxmox-a"), "{err}");
    }

    #[test]
    fn wol_address_without_port_is_rejected_at_startup() {
        let err = validate_err(&example_with("\"192.168.1.255:9\"", "\"192.168.1.255\""));
        assert!(err.contains("wol_broadcast_addr"), "{err}");
    }

    #[test]
    fn poll_interval_beyond_watchdog_margin_is_rejected() {
        for bad in ["poll_interval_secs = 0", "poll_interval_secs = 30"] {
            let err = validate_err(&example_with("poll_interval_secs = 5", bad));
            assert!(err.contains("poll_interval_secs"), "{err}");
        }
    }

    #[test]
    fn duplicate_endpoint_names_are_rejected() {
        let err = validate_err(&example_with("name = \"workstation-2\"", "name = \"workstation-1\""));
        assert!(err.contains("workstation-1"), "{err}");
    }

    #[test]
    fn soc_outside_0_100_is_rejected() {
        let err = validate_err(&example_with("low_battery_soc = 30.0", "low_battery_soc = 130.0"));
        assert!(err.contains("0-100"), "{err}");
    }

    #[test]
    fn ssh_connect_retry_secs_beyond_limit_is_rejected() {
        let err = validate_err(&example_with(
            "ssh_connect_retry_secs = 300",
            "ssh_connect_retry_secs = 1900",
        ));
        assert!(err.contains("ssh_connect_retry_secs"), "{err}");

        let err_ep = validate_err(&example_with(
            "name = \"workstation-1\"",
            "name = \"workstation-1\"\nssh_connect_retry_secs = 1900",
        ));
        assert!(err_ep.contains("ssh_connect_retry_secs"), "{err_ep}");
    }

    #[test]
    fn endpoint_ssh_connect_retry_secs_can_override_global() {
        let text = example_with(
            "name = \"workstation-1\"",
            "name = \"workstation-1\"\nssh_connect_retry_secs = 600",
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.thresholds.ssh_connect_retry_secs, 300);
        assert_eq!(cfg.endpoints[0].ssh_connect_retry_secs, Some(600));
        assert_eq!(
            cfg.endpoints[0].effective_ssh_connect_retry_secs(cfg.thresholds.ssh_connect_retry_secs),
            600
        );
        assert_eq!(
            cfg.endpoints[1].effective_ssh_connect_retry_secs(cfg.thresholds.ssh_connect_retry_secs),
            300
        );
    }

    #[cfg(unix)]
    #[test]
    fn loose_ssh_key_permissions_are_reported() {
        use std::os::unix::fs::PermissionsExt;
        let key = std::env::temp_dir().join(format!("mub-key-{}", std::process::id()));
        std::fs::write(&key, "not a real key").unwrap();
        let mut cfg: Config = toml::from_str(&example_with(
            "/etc/modbus-ups-bridge/workstation_key",
            key.to_str().unwrap(),
        ))
        .unwrap();
        cfg.endpoints.truncate(1);

        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let problems = cfg.ssh_key_problems();
        assert!(problems.iter().any(|p| p.contains("chmod 600")), "{problems:?}");

        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(cfg.ssh_key_problems().is_empty());

        std::fs::remove_file(&key).unwrap();
        assert!(cfg.ssh_key_problems()[0].contains("can't be read"));
    }

    #[test]
    fn old_register_settings_are_rejected() {
        // The register map moved into src/modbus.rs. A config that still
        // sets it must fail loudly, not look as if editing it did anything.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/bridge.toml.example");
        let raw = std::fs::read_to_string(path).unwrap();
        let old = raw.replacen("poll_interval_secs = 5", "poll_interval_secs = 5\nreg_battery_soc = 184", 1);
        assert_ne!(old, raw, "example config no longer has poll_interval_secs = 5");
        let err = toml::from_str::<Config>(&old).unwrap_err().to_string();
        assert!(err.contains("reg_battery_soc"), "{err}");
    }

    #[test]
    fn misplaced_top_level_key_is_rejected() {
        // wol_broadcast_addr under [thresholds] must be an error naming the
        // stray key, not silently ignored.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/bridge.toml.example");
        let raw = std::fs::read_to_string(path).unwrap();
        let mut moved = String::new();
        for line in raw.lines().filter(|l| !l.starts_with("wol_broadcast_addr")) {
            moved.push_str(line);
            moved.push('\n');
            if line.starts_with("stagger_secs") {
                moved.push_str("wol_broadcast_addr = \"192.168.1.255:9\"\n");
            }
        }
        let err = toml::from_str::<Config>(&moved).unwrap_err().to_string();
        assert!(err.contains("wol_broadcast_addr"), "{err}");
    }

    #[test]
    fn grid_lost_voltage_outside_range_is_rejected() {
        let err = validate_err(&example_with("grid_lost_voltage = 100.0", "grid_lost_voltage = 0.0"));
        assert!(err.contains("grid_lost_voltage"), "{err}");
    }

    #[test]
    fn grid_relay_check_defaults_on_and_can_be_switched_off() {
        let cfg: Config = toml::from_str(&example_with("use_grid_relay = true", "use_grid_relay = false")).unwrap();
        assert!(!cfg.thresholds.use_grid_relay);
        // A config without the key keeps the safer behaviour.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/bridge.toml.example");
        let raw = std::fs::read_to_string(path).unwrap();
        let without: String = raw
            .lines()
            .filter(|l| !l.starts_with("use_grid_relay"))
            .map(|l| format!("{l}\n"))
            .collect();
        let cfg: Config = toml::from_str(&without).unwrap();
        assert!(cfg.thresholds.use_grid_relay);
    }

    #[test]
    fn strict_inverter_checks_line_in_the_example_can_be_uncommented() {
        // The documented way to switch it on must actually load: the line has
        // to sit above the first [section] header, where the example puts it.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/bridge.toml.example");
        let raw = std::fs::read_to_string(path).unwrap();
        let edited = raw.replace("# strict_inverter_checks = false", "strict_inverter_checks = true");
        assert_ne!(raw, edited, "the example must contain the commented line");
        let cfg: Config = toml::from_str(&edited).expect("uncommented example must parse");
        assert!(cfg.strict_inverter_checks());
        let cfg: Config = toml::from_str(&raw).unwrap();
        assert!(!cfg.strict_inverter_checks());
    }

    #[test]
    fn wol_window_shorter_than_vm_timeout_is_rejected() {
        let text = example_with("method = \"poweroff\"", "method = \"vms_then_poweroff\"")
            .replacen("wol_resend_count = 8", "wol_resend_count = 1", 1)
            .replacen("wol_resend_interval_secs = 120", "wol_resend_interval_secs = 10", 1);
        let cfg: Config = toml::from_str(&text).unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("WOL window"), "{err}");
    }

    #[test]
    fn strict_inverter_checks_defaults_to_false() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/bridge.toml.example");
        let cfg = Config::load(path).unwrap();
        assert!(!cfg.strict_inverter_checks());
    }

    #[test]
    fn strict_inverter_checks_can_be_enabled_at_top_level_or_in_modbus() {
        let toml_top = example_with(
            "wol_broadcast_addr = \"192.168.1.255:9\"",
            "wol_broadcast_addr = \"192.168.1.255:9\"\nstrict_inverter_checks = true",
        );
        let cfg: Config = toml::from_str(&toml_top).unwrap();
        assert!(cfg.strict_inverter_checks());

        let toml_modbus = example_with(
            "[modbus]\ndevice = \"/dev/ttyS0\"",
            "[modbus]\ndevice = \"/dev/ttyS0\"\nstrict_inverter_checks = true",
        );
        let cfg2: Config = toml::from_str(&toml_modbus).unwrap();
        assert!(cfg2.strict_inverter_checks());
    }
}

