use crate::config::{Config, Endpoint, EndpointKind, ProxmoxConfig, ProxmoxMethod};
use crate::persist::ShutdownMarker;
use anyhow::{anyhow, bail, Context, Result};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinSet;

/// Hard ceiling on a single SSH call, on top of ssh's own ConnectTimeout and
/// keepalives -- one wedged endpoint must not stall the rest of the sequence.
const SSH_TIMEOUT: Duration = Duration::from_secs(60);

/// What the shutdown sequence needs from the config.
#[derive(Debug, Clone)]
pub struct ShutdownOptions {
    pub stagger_secs: u64,
    pub ssh_connect_retry_secs: u64,
    pub proxmox: ProxmoxConfig,
    pub known_hosts_file: Option<String>,
}

impl ShutdownOptions {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            stagger_secs: cfg.thresholds.stagger_secs,
            ssh_connect_retry_secs: cfg.thresholds.ssh_connect_retry_secs,
            proxmox: cfg.proxmox.clone(),
            known_hosts_file: cfg.ssh_known_hosts_file.clone(),
        }
    }
}

const SSH_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Determines whether an SSH failure is a transient connection failure (e.g. host
/// is still booting from a recent wake-up, port 22 not yet open, or network establishing)
/// as opposed to a permanent failure like authentication refusal or bad syntax.
pub fn is_transient_connection_error(err: &anyhow::Error) -> bool {
    let s = format!("{:#}", err).to_lowercase();
    if s.contains("permission denied")
        || s.contains("host key verification failed")
        || s.contains("offending key")
        || s.contains("identification has changed")
    {
        return false;
    }
    s.contains("connection refused")
        || s.contains("timed out")
        || s.contains("timeout")
        || s.contains("no route to host")
        || s.contains("network is unreachable")
        || s.contains("connection reset")
        || s.contains("exited some(255)")
}

/// Runs the full site shutdown sequence in the order `endpoints` are
/// configured, starting one endpoint every `stagger_secs`.
///
/// Windows workstations and Proxmox `poweroff` endpoints get a single command
/// each, with automatic retry for transient connection errors (up to
/// `ssh_connect_retry_secs`, e.g. if still booting from a recent wake-up).
/// Proxmox hosts configured with `vms_then_poweroff` run in their own tasks
/// since their guest shutdowns take minutes.
///
/// `stop` turns true when recovery is confirmed: no further endpoints are
/// started, and any ongoing retry attempts are cancelled immediately.
pub async fn run_shutdown_sequence(
    endpoints: Vec<Endpoint>,
    opts: ShutdownOptions,
    marker: Option<ShutdownMarker>,
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
            if let Some(ref m) = marker {
                m.record_dispatched(&ep.name);
            }
            let opts = opts.clone();
            let mut stop_rx = stop.clone();
            hosts.spawn(async move {
                if let Err(e) = proxmox_vms_then_poweroff(&ep, &opts, &mut stop_rx).await {
                    log::error!("failed to shut down {}: {:#}", ep.name, e);
                }
            });
        } else {
            let res = shutdown_one(&ep, &opts, &mut stop).await;
            if *stop.borrow() {
                log::warn!("recovery confirmed -- not starting the remaining endpoints");
                break;
            }
            if let Some(ref m) = marker {
                m.record_dispatched(&ep.name);
            }
            if let Err(e) = res {
                log::error!("failed to shut down {}: {:#}", ep.name, e);
            }
        }
    }

    while hosts.join_next().await.is_some() {}
    if !*stop.borrow() {
        if let Some(ref m) = marker {
            m.mark_completed();
        }
    }
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

/// Executes an SSH command on an endpoint with retry for transient connection
/// errors up to `ssh_connect_retry_secs`.
async fn ssh_exec_with_retry(
    ep: &Endpoint,
    opts: &ShutdownOptions,
    remote_cmd: &str,
    timeout: Duration,
    stop: &mut watch::Receiver<bool>,
) -> Result<String> {
    let start = tokio::time::Instant::now();
    let retry_budget = Duration::from_secs(opts.ssh_connect_retry_secs);

    loop {
        if *stop.borrow() {
            bail!("recovery confirmed -- cancelled shutdown on {}", ep.name);
        }
        match ssh_exec_timeout(ep, opts, remote_cmd, timeout).await {
            Ok(out) => return Ok(out),
            Err(e) => {
                let elapsed = start.elapsed();
                if !is_transient_connection_error(&e) || elapsed >= retry_budget || *stop.borrow() {
                    return Err(e);
                }
                let remaining = retry_budget.saturating_sub(elapsed);
                log::warn!(
                    "{}: SSH connection failed ({}) -- host may still be booting; retrying in {:?} ({}s retry budget remaining)",
                    ep.name,
                    e,
                    SSH_RETRY_INTERVAL,
                    remaining.as_secs()
                );
                tokio::select! {
                    _ = tokio::time::sleep(SSH_RETRY_INTERVAL) => {}
                    _ = stopped(stop) => {
                        bail!("recovery confirmed -- cancelled shutdown on {}", ep.name);
                    }
                }
            }
        }
    }
}

/// One SSH command per endpoint: Windows, and Proxmox with `poweroff`.
async fn shutdown_one(
    ep: &Endpoint,
    opts: &ShutdownOptions,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    log::warn!("shutting down {} ({}) via {:?}", ep.name, ep.host, ep.kind);
    ssh_exec_with_retry(ep, opts, &remote_command(ep), SSH_TIMEOUT, stop).await?;
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
async fn proxmox_vms_then_poweroff(
    ep: &Endpoint,
    opts: &ShutdownOptions,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    log::warn!(
        "shutting down {} ({}) via Proxmox: VMs first, then poweroff",
        ep.name,
        ep.host
    );

    // 1. Which VMs exist, and which are running -- read from the host now,
    //    not from a list that can go stale. If the host is still booting from a
    //    recent wake-up, this initial query retries until sshd is up.
    let listing = ssh_exec_with_retry(ep, opts, "qm list", SSH_TIMEOUT, stop)
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

    // 2. Ask every running VM to shut down in parallel (QEMU guest agent / ACPI)
    //    with Proxmox's native --timeout.
    //    On real Proxmox VE, `qm shutdown` blocks until the VM stops or times out;
    //    running them concurrently via JoinSet ensures all VMs start shutting down
    //    at once, rather than waiting one after the other on battery.
    let timeout_secs = opts.proxmox.vm_shutdown_timeout_secs;
    let ssh_timeout = Duration::from_secs(timeout_secs.saturating_add(15));
    let mut tasks = JoinSet::new();

    for vm in &running {
        let vm = vm.clone();
        let ep = ep.clone();
        let opts = opts.clone();
        log::info!("{}: guest shutdown requested for VM {}", ep.name, vm.name);
        tasks.spawn(async move {
            let cmd = qm_shutdown_command(vm.id, timeout_secs);
            let res = ssh_exec_timeout(&ep, &opts, &cmd, ssh_timeout).await;
            (vm, res)
        });
    }

    let mut refused = Vec::new();
    let mut pending = Vec::new();

    while let Some(res) = tasks.join_next().await {
        match res {
            Ok((vm, Ok(out))) => {
                if looks_like_failure(&out) {
                    log::warn!(
                        "{}: VM {} refused a guest shutdown ({}) -- will be powered off",
                        ep.name,
                        vm.name,
                        out.trim()
                    );
                    refused.push(vm);
                } else {
                    log::info!("{}: VM {} is off", ep.name, vm.name);
                }
            }
            Ok((vm, Err(e))) => {
                let err_str = format!("{:#}", e);
                if is_timeout(&err_str) && !is_guest_refusal(&err_str) {
                    pending.push(vm);
                } else {
                    log::warn!(
                        "{}: guest shutdown of VM {} failed ({:#}) -- will be powered off",
                        ep.name,
                        vm.name,
                        e
                    );
                    refused.push(vm);
                }
            }
            Err(e) => {
                log::error!("{}: VM shutdown task failed: {:#}", ep.name, e);
            }
        }
    }

    // 3. Whatever is still running now gets powered off hard: VMs that
    //    didn't stop in time (timed out), and those that refused the guest
    //    shutdown (no point waiting for those).
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

    // 5. The host. `systemctl poweroff --no-block` is the standard systemd command;
    //    --no-block ensures systemctl returns immediately and doesn't hold the SSH session
    //    until network termination cuts it off abruptly (exiting 255).
    //    If it's refused (e.g. systemctl fails or permission issue), the VMs are down by now, so
    //    a plain /sbin/poweroff is the safe fallback.
    let delay = ep.shutdown_delay_secs.max(10);
    match ssh_exec(ep, opts, "systemctl poweroff --no-block").await {
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

pub fn qm_shutdown_command(vmid: u32, timeout_secs: u64) -> String {
    format!("qm shutdown {} --timeout {}", vmid, timeout_secs)
}

fn is_timeout(err_str: &str) -> bool {
    let s = err_str.to_lowercase();
    s.contains("timed out") || s.contains("timeout")
}

fn is_guest_refusal(err_str: &str) -> bool {
    let s = err_str.to_lowercase();
    s.contains("agent")
        || s.contains("not running")
        || s.contains("not supported")
        || s.contains("refused")
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
    ssh_exec_timeout(ep, opts, remote_cmd, SSH_TIMEOUT).await
}

async fn ssh_exec_timeout(
    ep: &Endpoint,
    opts: &ShutdownOptions,
    remote_cmd: &str,
    timeout: Duration,
) -> Result<String> {
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

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow!("ssh to {} timed out after {:?}", ep.host, timeout))?
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

    #[test]
    fn qm_shutdown_command_includes_timeout() {
        assert_eq!(qm_shutdown_command(100, 180), "qm shutdown 100 --timeout 180");
        assert_eq!(qm_shutdown_command(101, 15), "qm shutdown 101 --timeout 15");
    }

    #[test]
    fn distinguishes_timeout_from_guest_refusal() {
        assert!(is_guest_refusal("QEMU guest agent is not running"));
        assert!(is_guest_refusal("VM 108 qmp command 'guest-ping' failed - got timeout\nQEMU guest agent is not running"));
        assert!(!is_guest_refusal("VM 109 shutdown timed out"));
        assert!(is_timeout("VM 109 shutdown timed out"));
        assert!(is_timeout("ssh to 10.99.0.3 timed out after 30s"));
        assert!(!is_timeout("QEMU guest agent is not running"));
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

    #[test]
    fn identifies_transient_connection_errors() {
        let err_refused = anyhow!("ssh to 10.99.0.1 exited Some(255): stderr=ssh: connect to host 10.99.0.1 port 22: Connection refused");
        assert!(is_transient_connection_error(&err_refused));

        let err_timeout = anyhow!("ssh to 10.99.0.1 exited Some(255): stderr=ssh: connect to host 10.99.0.1 port 22: Connection timed out");
        assert!(is_transient_connection_error(&err_timeout));

        let err_wall_timeout = anyhow!("ssh to 10.99.0.1 timed out after 60s");
        assert!(is_transient_connection_error(&err_wall_timeout));

        let err_no_route = anyhow!("ssh to 10.99.0.1 exited Some(255): stderr=ssh: connect to host 10.99.0.1 port 22: No route to host");
        assert!(is_transient_connection_error(&err_no_route));

        let err_reset = anyhow!("ssh to 10.99.0.1 exited Some(255): stderr=Connection reset by peer");
        assert!(is_transient_connection_error(&err_reset));

        let err_perm = anyhow!("ssh to 10.99.0.1 exited Some(255): stderr=root@10.99.0.1: Permission denied (publickey).");
        assert!(!is_transient_connection_error(&err_perm));

        let err_hostkey = anyhow!("ssh to 10.99.0.1 exited Some(255): stderr=@ Host key verification failed.");
        assert!(!is_transient_connection_error(&err_hostkey));

        let err_cmd = anyhow!("ssh to 10.99.0.1 exited Some(1): stderr=bash: invalid command");
        assert!(!is_transient_connection_error(&err_cmd));
    }
}
