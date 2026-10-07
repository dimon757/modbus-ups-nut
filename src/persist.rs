use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Status of the on-disk shutdown marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownState {
    /// No marker exists on disk.
    NotSet,
    /// A shutdown sequence was in progress; endpoints in `dispatched` were already sent commands.
    Incomplete { dispatched: Vec<String> },
    /// The entire sequence completed.
    Completed,
}

/// On-disk record that the shutdown sequence was fired and the matching
/// Wake-on-LAN round has not finished yet.
///
/// The bridge runs from the inverter's output like the endpoints, so a hard
/// cutoff (or a watchdog reboot, or a crash) restarts it with no memory of
/// having shut everything down -- and without this file it would never send
/// Wake-on-LAN on recovery. Written and fsynced before the sequence starts;
/// removed once the last Wake-on-LAN resend has gone out.
///
/// If restarted mid-sequence, the file records which endpoints were already
/// dispatched so the remaining ones can be shut down without re-sending
/// commands to the ones already stopped.
///
/// Failures are logged, never fatal: losing the marker must not stop a
/// shutdown from going ahead.
#[derive(Debug, Clone)]
pub struct ShutdownMarker {
    path: PathBuf,
}

impl ShutdownMarker {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn is_set(&self) -> bool {
        self.path.exists()
    }

    pub fn state(&self) -> ShutdownState {
        if !self.is_set() {
            return ShutdownState::NotSet;
        }
        let content = match fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) => {
                log::error!("failed to read shutdown marker {:?}: {}", self.path, e);
                return ShutdownState::Completed;
            }
        };

        let mut dispatched = Vec::new();
        let mut completed = false;

        for line in content.lines() {
            let line = line.trim();
            if line == "completed" {
                completed = true;
            } else if let Some(name) = line.strip_prefix("dispatched: ") {
                dispatched.push(name.trim().to_string());
            } else if line == "shutdown sequence fired; Wake-on-LAN pending on recovery" {
                completed = true;
            }
        }

        if completed {
            ShutdownState::Completed
        } else {
            ShutdownState::Incomplete { dispatched }
        }
    }

    pub fn set(&self) {
        if let Err(e) = self.try_set() {
            log::error!("failed to write shutdown marker {:?}: {:#}", self.path, e);
        }
    }

    pub fn record_dispatched(&self, endpoint_name: &str) {
        if let Err(e) = self.try_record_dispatched(endpoint_name) {
            log::error!(
                "failed to record dispatched endpoint {} in {:?}: {:#}",
                endpoint_name,
                self.path,
                e
            );
        }
    }

    pub fn mark_completed(&self) {
        if let Err(e) = self.try_mark_completed() {
            log::error!(
                "failed to mark shutdown completed in {:?}: {:#}",
                self.path,
                e
            );
        }
    }

    pub fn clear(&self) {
        match fs::remove_file(&self.path) {
            Ok(()) => {
                if let Err(e) = sync_parent(&self.path) {
                    log::error!("failed to sync after removing {:?}: {:#}", self.path, e);
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => log::error!("failed to remove shutdown marker {:?}: {}", self.path, e),
        }
    }

    fn try_set(&self) -> Result<()> {
        if let Some(dir) = parent_dir(&self.path) {
            fs::create_dir_all(dir).with_context(|| format!("creating {:?}", dir))?;
        }
        let mut f = fs::File::create(&self.path).context("creating file")?;
        f.write_all(b"# shutdown sequence in progress\n")
            .context("writing file")?;
        f.sync_all().context("syncing file")?;
        sync_parent(&self.path)
    }

    fn try_record_dispatched(&self, endpoint_name: &str) -> Result<()> {
        if let Some(dir) = parent_dir(&self.path) {
            fs::create_dir_all(dir).with_context(|| format!("creating {:?}", dir))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .context("opening marker file for append")?;
        writeln!(f, "dispatched: {}", endpoint_name.trim()).context("writing dispatched endpoint")?;
        f.sync_all().context("syncing file")?;
        sync_parent(&self.path)
    }

    fn try_mark_completed(&self) -> Result<()> {
        if let Some(dir) = parent_dir(&self.path) {
            fs::create_dir_all(dir).with_context(|| format!("creating {:?}", dir))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .context("opening marker file for append")?;
        writeln!(f, "completed").context("writing completed marker")?;
        f.sync_all().context("syncing file")?;
        sync_parent(&self.path)
    }
}

fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent().filter(|p| !p.as_os_str().is_empty())
}

/// fsync the directory so the file's creation/removal itself survives a
/// power cut, not just its contents.
fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = parent_dir(path).unwrap_or(Path::new("."));
        fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("syncing directory {:?}", dir))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_is_set_clear_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mub-marker-{}", std::process::id()));
        let marker = ShutdownMarker::new(dir.join("sub").join("shutdown_fired"));

        assert!(!marker.is_set());
        marker.set(); // also creates the missing directory
        assert!(marker.is_set());

        // A fresh instance (as after a reboot) sees the same state.
        assert!(ShutdownMarker::new(dir.join("sub").join("shutdown_fired")).is_set());

        marker.clear();
        assert!(!marker.is_set());
        marker.clear(); // clearing an absent marker is a no-op

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_transitions_incomplete_and_completed() {
        let dir = std::env::temp_dir().join(format!("mub-marker-state-{}", std::process::id()));
        let marker = ShutdownMarker::new(dir.join("sub").join("shutdown_fired"));

        assert_eq!(marker.state(), ShutdownState::NotSet);

        marker.set();
        assert_eq!(
            marker.state(),
            ShutdownState::Incomplete { dispatched: vec![] }
        );

        marker.record_dispatched("ws-1");
        assert_eq!(
            marker.state(),
            ShutdownState::Incomplete {
                dispatched: vec!["ws-1".into()]
            }
        );

        marker.record_dispatched("pve-1");
        assert_eq!(
            marker.state(),
            ShutdownState::Incomplete {
                dispatched: vec!["ws-1".into(), "pve-1".into()]
            }
        );

        marker.mark_completed();
        assert_eq!(marker.state(), ShutdownState::Completed);

        marker.clear();
        assert_eq!(marker.state(), ShutdownState::NotSet);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_marker_parses_as_completed() {
        let dir = std::env::temp_dir().join(format!("mub-marker-legacy-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker_file = dir.join("shutdown_fired");
        fs::write(
            &marker_file,
            "shutdown sequence fired; Wake-on-LAN pending on recovery\n",
        )
        .unwrap();

        let marker = ShutdownMarker::new(&marker_file);
        assert_eq!(marker.state(), ShutdownState::Completed);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_endpoint_omitted_from_marker_is_not_marked_dispatched() {
        let dir = std::env::temp_dir().join(format!("mub-marker-failed-{}", std::process::id()));
        let marker = ShutdownMarker::new(dir.join("sub").join("shutdown_fired"));

        marker.set();
        // ep1 succeeds
        marker.record_dispatched("ep1");
        // ep2 fails -> record_dispatched is NOT called
        // ep3 succeeds
        marker.record_dispatched("ep3");

        let state = marker.state();
        match state {
            ShutdownState::Incomplete { dispatched } => {
                assert!(dispatched.contains(&"ep1".to_string()));
                assert!(!dispatched.contains(&"ep2".to_string()));
                assert!(dispatched.contains(&"ep3".to_string()));
            }
            _ => panic!("expected Incomplete state"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_without_completed_remains_incomplete_for_restart() {
        let dir = std::env::temp_dir().join(format!("mub-marker-restart-{}", std::process::id()));
        let marker = ShutdownMarker::new(dir.join("shutdown_fired"));

        marker.set();
        marker.record_dispatched("ws-1");
        marker.record_dispatched("proxmox-a");
        marker.record_dispatched("proxmox-b");
        // ws-2 failed, so mark_completed was NOT called

        assert_eq!(
            marker.state(),
            ShutdownState::Incomplete {
                dispatched: vec!["ws-1".into(), "proxmox-a".into(), "proxmox-b".into()]
            }
        );

        // Later restart retries ws-2, which now succeeds and marks completed
        marker.record_dispatched("ws-2");
        marker.mark_completed();

        assert_eq!(marker.state(), ShutdownState::Completed);

        let _ = fs::remove_dir_all(&dir);
    }
}

