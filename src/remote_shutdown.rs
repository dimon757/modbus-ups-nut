use crate::config::{Config, Endpoint, EndpointKind, ProxmoxConfig, ProxmoxMethod};
use anyhow::{anyhow, bail, Context, Result};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinSet;

/// Hard ceiling on a single SSH call, on top of ssh's own ConnectTimeout and
/// keepalives -- one wedged endpoint must not stall the rest of the sequence.
const SSH_TIMEOUT: Duration = Duration::from_secs(60);

/// vms_then_poweroff: how often the VMs' power states are polled.
const VM_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What the shutdown sequence needs from the config.
#[derive(Debug, Clone)]
pub struct ShutdownOptions {
    pub stagger_secs: u64,
    pub proxmox: ProxmoxConfig,
    pub known_hosts_file: Option<String>,
}

impl ShutdownOptions {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            stagger_secs: cfg.thresholds.stagger_secs,
            proxmox: cfg.proxmox.clone(),
            known_hosts_file: cfg.ssh_known_hosts_file.clone(),
        }
    }
}

/// Runs the full site shutdown sequence in the order `endpoints` are
/// configured, starting one endpoint every `stagger_secs`.
///
/// Windows workstations get a native `shutdown` call, which shows the
/// logged-in user the usual countdown. Proxmox hosts are shut down by the
/// configured `ProxmoxMethod`; with `vms_then_poweroff` that takes minutes, so
/// each such host runs in its own task and the sequence moves on to the next
/// endpoint meanwhile.
///
/// `stop` turns true when recovery is confirmed: no further endpoints are
/// started, but hosts already shutting down are left to finish -- a host
/// abandoned half way would keep running with its VMs off, while one that
/// completes is brought back by the Wake-on-LAN rounds and its Autostart.
pub async fn run_shutdown_sequence(
    endpoints: Vec<Endpoint>,
    opts: ShutdownOptions,
    mut stop: watch::Receiver<bool>,
) {
    log::warn!("shutdown sequence starting: {} endpoint(s)", endpoints.len());
    let mut hosts = JoinSet::new();

    for (i, ep) in endpoints.into_iter().enumerate() {
        if i > 0 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(opts.stagger_secs)) => {}
                _ = stopped(&mut stop) => {}
            }
        }
        if *stop.borrow() {
            log::warn!("recovery confirmed -- not starting the remaining endpoints");
            break;
        }
        if ep.kind == EndpointKind::Proxmox && opts.proxmox.method == ProxmoxMethod::VmsThenPoweroff {
            let opts = opts.clone();
            hosts.spawn(async move {
                if let Err(e) = proxmox_vms_then_poweroff(&ep, &opts).await {
                    log::error!("failed to shut down {}: {:#}", ep.name, e);
                }
            });
        } else if let Err(e) = shutdown_one(&ep, &opts).await {
            // Log and continue -- one unreachable endpoint shouldn't stop us
            // from at least trying the rest.
            log::error!("failed to shut down {}: {:#}", ep.name, e);
        }
    }

    while hosts.join_next().await.is_some() {}
    log::warn!("shutdown sequence complete");
}

/// Resolves once `stop` is true. If the sender is gone without ever sending
/// (e.g. replaced by a newer sequence), it never resolves -- a dropped
/// sender must not cut the stagger delays short.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    if stop.wait_for(|s| *s).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// One SSH command per endpoint: Windows, and Proxmox with `poweroff`.
async fn shutdown_one(ep: &Endpoint, opts: &ShutdownOptions) -> Result<()> {
    log::warn!("shutting down {} ({}) via {:?}", ep.name, ep.host, ep.kind);
    ssh_exec(ep, opts, &remote_command(ep)).await?;
    log::info!("{}: shutdown command accepted", ep.name);
    Ok(())
}

fn remote_command(ep: &Endpoint) -> String {
    match ep.kind {
        // `/sbin/poweroff` runs systemd/pve-guests stop sequence (ordered guest
        // shutdown according to Proxmox configuration), then turns the host off.
        // The guest shutdowns can take far longer than SSH_TIMEOUT, so the whole
        // thing is detached with nohup and ssh returns as soon as it's started --
        // otherwise our timeout would kill the session and could interrupt shutdown.
        // Output goes to /tmp/ups-shutdown.log on the host for post-mortems.
        EndpointKind::Proxmox => format!(
            "nohup sh -c 'sleep {}; /sbin/poweroff' \
             > /tmp/ups-shutdown.log 2>&1 < /dev/null &",
            ep.shutdown_delay_secs
        ),
        EndpointKind::Windows => format!(
            "shutdown /s /t {} /c \"Inverter battery low, grid still down -- shutting down.\"",
            ep.shutdown_delay_secs
        ),
    }
}

/// A VM as listed by `qm list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vm {
    pub id: u32,
    pub name: String,
}

/// `vms_then_poweroff`: the bridge shuts the host's VMs down itself, then the
/// host. Every step is logged, and nothing is left for a human: a VM that
/// doesn't stop in time is powered off hard -- on battery, the alternative
/// is the inverter cutting the whole host a little later.
async fn proxmox_vms_then_poweroff(ep: &Endpoint, opts: &ShutdownOptions) -> Result<()> {
    log::warn!(
        "shutting down {} ({}) via Proxmox: VMs first, then poweroff",
        ep.name,
        ep.host
    );

    // 1. Which VMs exist, and which are running -- read from the host now,
    //    not from a list that can go stale.
    let listing = ssh_exec(ep, opts, "qm list")
        .await
        .context("listing VMs")?;
    let vms = parse_qmlist(&listing);
    let running = if vms.is_empty() {
        Vec::new()
    } else {
        running_vms(ep, opts, &vms).await.context("reading VM power states")?
    };
    log::info!(
        "{}: {} VM(s) registered, {} running{}",
        ep.name,
        vms.len(),
        running.len(),
        names_suffix(&running)
    );

    // 2. Ask every running VM to shut down (QEMU guest agent / ACPI), all at once --
    //    the battery doesn't leave time for one after the other.
    let mut refused = Vec::new();
    for vm in &running {
        match ssh_exec(ep, opts, &format!("qm shutdown {}", vm.id)).await {
            Ok(out) if looks_like_failure(&out) => {
                log::warn!(
                    "{}: VM {} refused a guest shutdown ({}) -- will be powered off",
                    ep.name,
                    vm.name,
                    out.trim()
                );
                refused.push(vm.clone());
            }
            Ok(_) => log::info!("{}: guest shutdown requested for VM {}", ep.name, vm.name),
            Err(e) => {
                log::warn!(
                    "{}: guest shutdown of VM {} failed ({:#}) -- will be powered off",
                    ep.name,
                    vm.name,
                    e
                );
                refused.push(vm.clone());
            }
        }
    }

    // 3. Wait for them, up to vm_shutdown_timeout_secs.
    let mut pending: Vec<Vm> = running
        .iter()
        .filter(|vm| !refused.contains(vm))
        .cloned()
        .collect();
    let deadline = Instant::now() + Duration::from_secs(opts.proxmox.vm_shutdown_timeout_secs);
    while !pending.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(VM_POLL_INTERVAL).await;
        match running_vms(ep, opts, &pending).await {
            Ok(still_on) => {
                for vm in pending.iter().filter(|vm| !still_on.contains(vm)) {
                    log::info!("{}: VM {} is off", ep.name, vm.name);
                }
                pending = still_on;
            }
            // Keep going: the host may just be slow to answer.
            Err(e) => log::warn!("{}: could not poll VM states: {:#}", ep.name, e),
        }
    }

    // 4. Whatever is still running now gets powered off hard: VMs that
    //    didn't stop in time, and those that refused the guest shutdown
    //    (no point waiting for those).
    for vm in &pending {
        log::error!(
            "{}: VM {} still running after {} s -- powering it off hard",
            ep.name,
            vm.name,
            opts.proxmox.vm_shutdown_timeout_secs
        );
        power_off_hard(ep, opts, vm).await;
    }
    for vm in &refused {
        log::error!(
            "{}: VM {} could not be shut down gracefully -- powering it off hard",
            ep.name,
            vm.name
        );
        power_off_hard(ep, opts, vm).await;
    }

    // 5. The host. `systemctl poweroff` is the standard systemd command; if it's refused
    //    (e.g. systemctl fails or permission issue), the VMs are down by now, so
    //    a plain /sbin/poweroff is the safe fallback.
    let delay = ep.shutdown_delay_secs.max(10);
    match ssh_exec(ep, opts, "systemctl poweroff").await {
        Ok(_) => log::warn!("{}: host power-off scheduled via systemctl poweroff", ep.name),
        Err(e) => {
            log::error!(
                "{}: systemctl poweroff refused ({:#}) -- falling back to /sbin/poweroff",
                ep.name,
                e
            );
            let fallback = format!(
                "nohup sh -c 'sleep {}; /sbin/poweroff' > /dev/null 2>&1 < /dev/null &",
                delay
            );
            ssh_exec(ep, opts, &fallback)
                .await
                .context("fallback /sbin/poweroff")?;
            log::warn!("{}: host power-off scheduled via /sbin/poweroff ({} s)", ep.name, delay);
        }
    }
    Ok(())
}

async fn power_off_hard(ep: &Endpoint, opts: &ShutdownOptions, vm: &Vm) {
    if let Err(e) = ssh_exec(ep, opts, &format!("qm stop {}", vm.id)).await {
        log::error!("{}: hard power-off of VM {} failed: {:#}", ep.name, vm.name, e);
    }
}

/// The subset of `vms` whose power state is "running". One SSH call for
/// all of them. A VM whose state can't be read counts as running.
async fn running_vms(ep: &Endpoint, opts: &ShutdownOptions, vms: &[Vm]) -> Result<Vec<Vm>> {
    let out = ssh_exec(ep, opts, &power_states_command(vms)).await?;
    Ok(select_running(vms, &out))
}

fn power_states_command(vms: &[Vm]) -> String {
    let ids: Vec<String> = vms.iter().map(|vm| vm.id.to_string()).collect();
    format!(
        "for id in {}; do echo \"$id $(qm status $id)\"; done",
        ids.join(" ")
    )
}

/// Parses `qm list` output.
/// Example lines:
///       VMID NAME                 STATUS     MEM(MB)    BOOTDISK(GB) PID       
///        100 dc01                 running    2048              32.00 1234      
///        101 app server (prod)    running    4096              50.00 5678      
///        102 db01                 stopped    8192             100.00 0         
pub fn parse_qmlist(out: &str) -> Vec<Vm> {
    out.lines()
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() < 3 {
                return None;
            }
            let id: u32 = tokens[0].parse().ok()?;
            let status_idx = tokens[1..]
                .iter()
                .position(|&t| matches!(t, "running" | "stopped" | "paused" | "suspended" | "prelaunch"))?;
            if status_idx == 0 {
                return None;
            }
            let name = tokens[1..=status_idx].join(" ");
            Some(Vm { id, name })
        })
        .collect()
}

/// From `power_states_command` output (`<id> status: running` per line), the VMs
/// that are still running. Missing or unreadable ones count as running -- they'll be
/// powered off hard rather than silently left running.
pub fn select_running(vms: &[Vm], out: &str) -> Vec<Vm> {
    vms.iter()
        .filter(|vm| {
            let state = out.lines().find_map(|line| {
                let (id, state) = line.trim().split_once(' ')?;
                (id == vm.id.to_string()).then(|| state.trim())
            });
            match state {
                Some(s) if s.contains("stopped") => false,
                _ => true,
            }
        })
        .cloned()
        .collect()
}

/// qm doesn't always use its exit code for failures; it prints them.
fn looks_like_failure(out: &str) -> bool {
    let o = out.to_lowercase();
    o.contains("not running")
        || o.contains("error")
        || o.contains("failed")
        || o.contains("not supported")
        || o.contains("refused")
        || o.contains("agent")
}

fn names_suffix(vms: &[Vm]) -> String {
    if vms.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = vms.iter().map(|vm| vm.name.as_str()).collect();
        format!(": {}", names.join(", "))
    }
}

/// Runs one command on an endpoint over SSH and returns its stdout. Async so
/// it never blocks the (single-threaded) runtime: the poll loop and watchdog
/// feed keep running while this waits on the network. Host keys are pinned
/// (`StrictHostKeyChecking=yes`): an unknown or changed key fails the call
/// rather than being trusted.
async fn ssh_exec(ep: &Endpoint, opts: &ShutdownOptions, remote_cmd: &str) -> Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-i",
        &ep.ssh_key_path,
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=3",
    ]);
    if let Some(file) = &opts.known_hosts_file {
        cmd.arg("-o").arg(format!("UserKnownHostsFile={}", file));
    }
    cmd.arg(format!("{}@{}", ep.ssh_user, ep.host))
        .arg(remote_cmd)
        .kill_on_drop(true);

    let output = tokio::time::timeout(SSH_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("ssh to {} timed out after {:?}", ep.host, SSH_TIMEOUT))?
        .context("spawning ssh")?;

    if !output.status.success() {
        bail!(
            "ssh to {} exited {:?}: stderr={}",
            ep.host,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Endpoints whose host key isn't in the known_hosts file ssh will use.
/// With pinned host keys, their shutdown would fail -- reported at startup.
pub fn known_host_problems(cfg: &Config) -> Vec<String> {
    let file = cfg.ssh_known_hosts_file.clone().unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        format!("{}/.ssh/known_hosts", home)
    });
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for ep in &cfg.endpoints {
        if !seen.insert(ep.host.as_str()) {
            continue;
        }
        let found = std::process::Command::new("ssh-keygen")
            .args(["-F", &ep.host, "-f", &file])
            .output();
        match found {
            Ok(o) if o.status.success() => {}
            Ok(_) => out.push(format!(
                "host key of {} ({}) is not in {} -- its shutdown will fail; record it \
                 first (see docs/installation.md, step 8)",
                ep.name, ep.host, file
            )),
            Err(e) => out.push(format!("cannot run ssh-keygen to check {}: {}", file, e)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(kind: EndpointKind) -> Endpoint {
        Endpoint {
            name: "test".into(),
            kind,
            host: "10.0.0.1".into(),
            ssh_user: "root".into(),
            ssh_key_path: "/dev/null".into(),
            shutdown_delay_secs: 10,
            mac_address: "AA:BB:CC:DD:EE:FF".into(),
        }
    }

    fn vm(id: u32, name: &str) -> Vm {
        Vm { id, name: name.into() }
    }

    #[test]
    fn proxmox_command_runs_poweroff_detached() {
        let cmd = remote_command(&endpoint(EndpointKind::Proxmox));
        assert_eq!(
            cmd,
            "nohup sh -c 'sleep 10; /sbin/poweroff' \
             > /tmp/ups-shutdown.log 2>&1 < /dev/null &"
        );
        assert!(!cmd.contains("systemctl"));
    }

    #[test]
    fn parses_qmlist_including_names_with_spaces() {
        let out = "\
      VMID NAME                 STATUS     MEM(MB)    BOOTDISK(GB) PID       
       100 dc01                 running    2048              32.00 1234      
       101 app server (prod)    running    4096              50.00 5678      
       102 db01                 stopped    8192             100.00 0         
";
        assert_eq!(
            parse_qmlist(out),
            vec![vm(100, "dc01"), vm(101, "app server (prod)"), vm(102, "db01")]
        );
        assert!(parse_qmlist("      VMID NAME                 STATUS     MEM(MB)\n").is_empty());
    }

    #[test]
    fn power_states_command_covers_every_vm() {
        assert_eq!(
            power_states_command(&[vm(100, "a"), vm(101, "b")]),
            "for id in 100 101; do echo \"$id $(qm status $id)\"; done"
        );
    }

    #[test]
    fn selects_running_vms_and_treats_unknown_as_running() {
        let vms = [vm(100, "a"), vm(101, "b"), vm(102, "c"), vm(103, "d")];
        let out = "100 status: running\n101 status: stopped\n102 status: paused\n";
        // 103 missing from the output -> counted as running (fail safe).
        // 102 paused -> not stopped, counted as running.
        assert_eq!(select_running(&vms, out), vec![vm(100, "a"), vm(102, "c"), vm(103, "d")]);
    }

    #[test]
    fn recognises_qm_failure_text() {
        assert!(looks_like_failure("QEMU guest agent is not running"));
        assert!(looks_like_failure("VM 100 not running"));
        assert!(looks_like_failure("command 'qm shutdown 100' failed: exit code 1"));
        assert!(!looks_like_failure(""));
    }

    #[tokio::test]
    async fn stop_signal_resolves_on_true_but_not_on_a_dropped_sender() {
        let (tx, mut rx) = watch::channel(false);
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_millis(100), stopped(&mut rx))
            .await
            .expect("resolves once stop is true");

        let (tx, mut rx) = watch::channel(false);
        drop(tx);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stopped(&mut rx))
                .await
                .is_err(),
            "a dropped sender must not end the stagger wait"
        );
    }
}
