use crate::config::WatchdogConfig;
use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;

/// Feeds a Linux hardware/software watchdog device (e.g. /dev/watchdog from the
/// Intel TCO watchdog, iTCO_wdt) once per poll loop. If this process hangs --
/// stuck serial read, deadlock, whatever -- the watchdog stops being fed and
/// the board reboots itself rather than silently stopping shutdown
/// signalling for all four machines.
///
/// Requires the watchdog module loaded and no other process (e.g. the
/// systemd watchdog itself) already holding the device.
///
/// Closing the device does NOT stop the watchdog: the kernel treats it as an
/// "unexpected close" and still reboots when the timeout runs out -- that's
/// what makes a crash reboot the box. A deliberate stop must call
/// `disarm()`, which sends the magic 'V' first.
pub struct Watchdog {
    file: Option<std::fs::File>,
}

impl Watchdog {
    pub fn open(cfg: Option<&WatchdogConfig>) -> Result<Self> {
        let file = match cfg {
            Some(c) => Some(
                OpenOptions::new()
                    .write(true)
                    .open(&c.device)
                    .with_context(|| format!("opening watchdog device {}", c.device))?,
            ),
            None => None,
        };
        Ok(Self { file })
    }

    /// A second handle on the same open device, so a stop request can
    /// disarm it while the main loop still owns the original.
    pub fn try_clone(&self) -> Result<Self> {
        let file = match &self.file {
            Some(f) => Some(f.try_clone().context("duplicating watchdog handle")?),
            None => None,
        };
        Ok(Self { file })
    }

    pub fn feed(&mut self) {
        if let Some(f) = self.file.as_mut() {
            if let Err(e) = f.write_all(b"\0") {
                log::error!("failed to feed watchdog: {}", e);
            }
        }
    }

    /// Magic close: tells the driver this close is deliberate, so it stops
    /// the countdown instead of rebooting. Only for a requested stop.
    pub fn disarm(mut self) {
        if let Some(f) = self.file.as_mut() {
            match f.write_all(b"V") {
                Ok(()) => log::info!("watchdog disarmed"),
                Err(e) => log::error!("failed to disarm watchdog -- the box may reboot: {}", e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feeds_then_disarms_through_a_clone_with_magic_v() {
        // A plain file stands in for /dev/watchdog: we check the bytes the
        // driver would see. Feed = '\0'; a requested stop = 'V' (magic close),
        // sent through the clone the stop handler holds.
        let path = std::env::temp_dir().join(format!("mub-wdt-{}", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        let cfg = WatchdogConfig { device: path.to_str().unwrap().into() };

        let mut wdt = Watchdog::open(Some(&cfg)).unwrap();
        let for_stop = wdt.try_clone().unwrap();
        wdt.feed();
        wdt.feed();
        for_stop.disarm();
        drop(wdt);

        assert_eq!(std::fs::read(&path).unwrap(), b"\0\0V");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn without_a_watchdog_configured_everything_is_a_no_op() {
        let mut wdt = Watchdog::open(None).unwrap();
        wdt.feed();
        wdt.try_clone().unwrap().disarm();
    }
}
