use anyhow::{Context, Result};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// On-disk record that the shutdown sequence was fired and the matching
/// Wake-on-LAN round has not finished yet.
///
/// The bridge runs from the inverter's output like the endpoints, so a hard
/// cutoff (or a watchdog reboot, or a crash) restarts it with no memory of
/// having shut everything down -- and without this file it would never send
/// Wake-on-LAN on recovery. Written and fsynced before the sequence starts;
/// removed once the last Wake-on-LAN resend has gone out.
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

    pub fn set(&self) {
        if let Err(e) = self.try_set() {
            log::error!("failed to write shutdown marker {:?}: {:#}", self.path, e);
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
        f.write_all(b"shutdown sequence fired; Wake-on-LAN pending on recovery\n")
            .context("writing file")?;
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
}
