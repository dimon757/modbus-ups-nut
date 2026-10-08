#!/usr/bin/env python3
"""Automated runner for ALL Level 2 test scenarios (A through P).
Configuration:
  - 2 Windows 11 endpoints (ws-1: 10.99.0.1, ws-2: 10.99.0.2)
  - 1 Proxmox endpoint (proxmox: 10.99.0.3, method: vms_then_poweroff)
  - Config file: test/bridge.toml
"""

import os
import queue
import re
import signal
import subprocess
import sys
import threading
import time

TEST_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_DIR = os.path.dirname(TEST_DIR)
DIR = "/tmp/mub-test"
VENV_PYTHON = f"{DIR}/venv/bin/python"
BRIDGE_BIN = f"{REPO_DIR}/target/debug/modbus-ups-bridge"
CONFIG = f"{TEST_DIR}/bridge.toml"

class Color:
    GREEN = "\033[92m"
    RED = "\033[91m"
    YELLOW = "\033[93m"
    CYAN = "\033[96m"
    BOLD = "\033[1m"
    RESET = "\033[0m"

def log(msg, color=Color.RESET):
    print(f"{color}{msg}{Color.RESET}", flush=True)

class AllLevel2Runner:
    def __init__(self):
        self.socat_proc = None
        self.sim_proc = None
        self.wol_proc = None
        self.bridge_proc = None
        self.bridge_logs = []
        self.log_cursor = 0

    def stop_existing(self):
        my_pid = os.getpid()
        for pid_str in os.listdir("/proc"):
            if not pid_str.isdigit() or int(pid_str) == my_pid:
                continue
            try:
                with open(f"/proc/{pid_str}/cmdline", "rb") as f:
                    cmd = f.read().decode(errors="ignore")
                    if ("inverter_sim.py" in cmd or "wol_listen.py" in cmd or "target/debug/modbus-ups-bridge" in cmd) and "run_all_level2" not in cmd:
                        os.kill(int(pid_str), signal.SIGTERM)
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                pass
        time.sleep(0.5)

    def init_keys_and_dirs(self):
        os.makedirs(DIR, exist_ok=True)
        if not os.path.exists(VENV_PYTHON):
            log("--> Setting up virtualenv...", Color.CYAN)
            subprocess.run(["python3", "-m", "venv", f"{DIR}/venv"], check=True)
            subprocess.run([f"{DIR}/venv/bin/pip", "install", "-q", "-r", f"{TEST_DIR}/requirements.txt"], check=True)

        subprocess.run(["chmod", "+x", f"{TEST_DIR}/bin/ssh", f"{TEST_DIR}/pve-bin/qm", f"{TEST_DIR}/pve-bin/systemctl"], check=False)

        fake_key = f"{DIR}/fake_key"
        with open(fake_key, "w") as f:
            f.write("dummy key for level-2 tests\n")
        os.chmod(fake_key, 0o600)

        fake_hostkey = f"{DIR}/fake_hostkey"
        if os.path.exists(fake_hostkey):
            os.remove(fake_hostkey)
        if os.path.exists(f"{fake_hostkey}.pub"):
            os.remove(f"{fake_hostkey}.pub")
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", fake_hostkey], check=True)

        with open(f"{fake_hostkey}.pub") as f:
            pub_key = " ".join(f.read().split()[:2])

        with open(f"{DIR}/known_hosts", "w") as f:
            for h in ["10.99.0.1", "10.99.0.2", "10.99.0.3"]:
                f.write(f"{h} {pub_key}\n")

    def reset_env(self):
        with open(f"{DIR}/ssh.log", "w") as f:
            f.truncate(0)
        for f in ["ssh-fail", "ssh-hang", "shutdown_fired"]:
            p = f"{DIR}/{f}"
            if os.path.exists(p):
                os.remove(p)
        if os.path.exists(DIR):
            for f in os.listdir(DIR):
                if f.startswith("ssh-booting-") or f.startswith("ssh-flaky-") or f.startswith("ssh-already-scheduled-") or f.startswith("ssh-delay-"):
                    try:
                        os.remove(os.path.join(DIR, f))
                    except Exception:
                        pass
        # reset simulated proxmox vms on 10.99.0.3
        h_dir = f"{DIR}/proxmox/10.99.0.3"
        os.makedirs(h_dir, exist_ok=True)
        for f in os.listdir(h_dir):
            try:
                os.remove(os.path.join(h_dir, f))
            except Exception:
                pass
        with open(f"{DIR}/proxmox/10.99.0.3/vms", "w") as f:
            f.write("100|dc01|ok:3\n101|app server|ok:6\n")
        for i in [100, 101]:
            with open(f"{DIR}/proxmox/10.99.0.3/state_{i}", "w") as f:
                f.write("on\n")

    def start_cable(self):
        log("--> Checking virtual serial cable (socat)...", Color.CYAN)
        if not (os.path.exists(f"{DIR}/ttyINV") and os.path.exists(f"{DIR}/ttyBR")):
            subprocess.run(["bash", f"{TEST_DIR}/setup.sh"], check=True)
        log("    Virtual serial cable active.", Color.GREEN)

    def start_wol_listener(self):
        log("--> Starting Wake-on-LAN listener on 127.0.0.1:40009...", Color.CYAN)
        wol_log = open(f"{DIR}/wol.log", "w")
        self.wol_proc = subprocess.Popen(
            ["python3", f"{TEST_DIR}/wol_listen.py", "40009"],
            stdout=wol_log,
            stderr=subprocess.STDOUT
        )
        time.sleep(0.5)
        log("    WOL listener active.", Color.GREEN)

    def start_simulator(self):
        log("--> Starting Sunsynk inverter simulator...", Color.CYAN)
        self.sim_proc = subprocess.Popen(
            [VENV_PYTHON, f"{TEST_DIR}/inverter_sim.py", "--serial", f"{DIR}/ttyINV"],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            text=True
        )
        time.sleep(1)
        log("    Inverter simulator active.", Color.GREEN)

    def send_sim_cmd(self, cmd):
        log(f"    [sim] > {cmd}", Color.YELLOW)
        if self.sim_proc and self.sim_proc.stdin:
            self.sim_proc.stdin.write(f"{cmd}\n")
            self.sim_proc.stdin.flush()

    def start_bridge(self, config=CONFIG):
        cfg_name = os.path.basename(config)
        log(f"--> Starting modbus-ups-bridge ({cfg_name})...", Color.CYAN)
        env = os.environ.copy()
        env["PATH"] = f"{TEST_DIR}/bin:{env.get('PATH', '')}"
        env["RUST_LOG"] = "modbus_ups_bridge=debug,info"
        self.bridge_proc = subprocess.Popen(
            [BRIDGE_BIN, config],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1
        )
        self.bridge_logs = []
        self.log_cursor = 0

        def _reader():
            for line in iter(self.bridge_proc.stdout.readline, ''):
                l = line.rstrip()
                self.bridge_logs.append(l)

        self.reader_thread = threading.Thread(target=_reader, daemon=True)
        self.reader_thread.start()

    def wait_for_bridge_log(self, pattern, timeout=15, from_start=False):
        t0 = time.time()
        regex = re.compile(pattern)
        search_from = 0 if from_start else self.log_cursor
        while time.time() - t0 < timeout:
            for idx in range(search_from, len(self.bridge_logs)):
                line = self.bridge_logs[idx]
                if regex.search(line):
                    if not from_start:
                        self.log_cursor = idx + 1
                    return line
            if self.bridge_proc and self.bridge_proc.poll() is not None:
                logs = "\n".join(self.bridge_logs)
                raise RuntimeError(f"Bridge exited prematurely with code {self.bridge_proc.returncode}:\n{logs}")
            time.sleep(0.1)
        recent = "\n    ".join(self.bridge_logs[-15:])
        raise TimeoutError(f"Timed out waiting for pattern '{pattern}' after {timeout}s.\nRecent bridge logs:\n    {recent}")

    def stop_bridge(self):
        if self.bridge_proc:
            log("--> Stopping bridge...", Color.CYAN)
            self.bridge_proc.terminate()
            try:
                self.bridge_proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.bridge_proc.kill()
            self.bridge_proc = None

    def cleanup(self):
        self.stop_bridge()
        if self.sim_proc:
            try:
                self.sim_proc.terminate()
                self.sim_proc.wait(timeout=2)
            except Exception:
                self.sim_proc.kill()
        if self.wol_proc:
            try:
                self.wol_proc.terminate()
                self.wol_proc.wait(timeout=2)
            except Exception:
                self.wol_proc.kill()
        self.stop_existing()

    # --- ALL 22 SCENARIOS ---

    def test_scenario_a(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO A: Startup & Inverter Telemetry Polling (3 Endpoints)", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()

        line = self.wait_for_bridge_log(r"loaded config from .* \(3 endpoint\(s\)\)")
        log(f"  [OK] Confirmed loaded 3 endpoints: {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"inverter: device type 0x0300, battery mode 1")
        log(f"  [OK] Inverter handshake: {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V.*on_battery=false low_battery=false")
        log(f"  [OK] Telemetry polling verified: {line}", Color.GREEN)

        assert os.path.getsize(f"{DIR}/ssh.log") == 0
        assert not os.path.exists(f"{DIR}/shutdown_fired")
        log("--> SCENARIO A: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_b(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO B: Short Grid Blip (< 5s)", Color.BOLD)
        log("=======================================================", Color.BOLD)
        if not self.bridge_proc:
            self.send_sim_cmd("restore")
            self.send_sim_cmd("soc 80")
            self.start_bridge()
            self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")
        self.send_sim_cmd("outage")
        line = self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(1.5)
        self.send_sim_cmd("restore")
        line = self.wait_for_bridge_log(r"state: GridLostDebouncing -> Idle")
        log(f"  [OK] Debounced back to Idle: {line}", Color.GREEN)

        assert os.path.getsize(f"{DIR}/ssh.log") == 0
        assert not os.path.exists(f"{DIR}/shutdown_fired")
        log("--> SCENARIO B: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_c(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO C: Outage Without Low Battery", Color.BOLD)
        log("=======================================================", Color.BOLD)
        if not self.bridge_proc:
            self.send_sim_cmd("restore")
            self.send_sim_cmd("soc 80")
            self.start_bridge()
            self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")
        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        line = self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        log(f"  [OK] Debounced to OnBattery: {line}", Color.GREEN)

        self.send_sim_cmd("soc 50")
        time.sleep(2)
        self.send_sim_cmd("restore")
        line = self.wait_for_bridge_log(r"state: OnBattery -> RecoveryDebouncing")
        log(f"  [OK] Grid returned, debouncing: {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log(f"  [OK] Restored to Idle: {line}", Color.GREEN)

        assert os.path.getsize(f"{DIR}/ssh.log") == 0
        assert not os.path.exists(f"{DIR}/shutdown_fired")
        log("--> SCENARIO C: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_d(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO D: Full Outage -> Shutdown (2 Win11 + Proxmox vms_then_poweroff) -> Latch -> WoL", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)

        self.send_sim_cmd("soc 25")
        line = self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)
        log(f"  [OK] Shutdown trigger: {line}", Color.GREEN)

        assert os.path.exists(f"{DIR}/shutdown_fired")
        log("  [OK] Marker file created prior to remote commands", Color.GREEN)

        line = self.wait_for_bridge_log(r"shutdown sequence starting: 3 endpoint\(s\)", timeout=5)
        log(f"  [OK] {line}", Color.GREEN)

        # ws-1 then ws-2
        self.wait_for_bridge_log(r"shutting down ws-1", timeout=8)
        self.wait_for_bridge_log(r"shutting down ws-2", timeout=8)

        # proxmox: vms first, then poweroff
        line = self.wait_for_bridge_log(r"shutting down proxmox \(10\.99\.0\.3\) via Proxmox: VMs first, then poweroff", timeout=10)
        log(f"  [OK] Proxmox VM walk initiated: {line}", Color.GREEN)
        self.wait_for_bridge_log(r"proxmox: 2 VM\(s\) registered, 2 running: dc01, app server", timeout=10)
        self.wait_for_bridge_log(r"proxmox: host power-off scheduled via systemctl poweroff", timeout=25, from_start=True)

        line = self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)
        log(f"  [OK] Shutdown sequence finished: {line}", Color.GREEN)

        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert "10.99.0.1" in ssh_content, "ws-1 not contacted"
        assert "10.99.0.2" in ssh_content, "ws-2 not contacted"
        assert "10.99.0.3" in ssh_content, "proxmox not contacted"

        # Check marker file
        with open(f"{DIR}/shutdown_fired") as f:
            marker = f.read()
        assert "dispatched: ws-1" in marker
        assert "dispatched: ws-2" in marker
        assert "dispatched: proxmox" in marker
        assert "completed" in marker
        log("  [OK] Marker correctly logged all 3 endpoints dispatched and completed", Color.GREEN)

        # Latching check
        self.send_sim_cmd("soc 22")
        time.sleep(2)
        with open(f"{DIR}/ssh.log") as f:
            lines_before = len(f.readlines())
        self.send_sim_cmd("soc 21")
        time.sleep(2)
        with open(f"{DIR}/ssh.log") as f:
            lines_after = len(f.readlines())
        assert lines_before == lines_after, "Shutdown refired while latched!"
        log("  [OK] State is latched: battery drop caused no duplicate shutdown calls", Color.GREEN)

        # Recovery + WoL
        self.send_sim_cmd("restore")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing")
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)

        for round_num in range(1, 5):
            line = self.wait_for_bridge_log(rf"Wake-on-LAN round {round_num}/4", timeout=10)
            log(f"  [OK] {line}", Color.GREEN)

        time.sleep(1)
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "Marker file should be deleted after WOL"
        log("  [OK] Marker file cleanly removed after final WOL round", Color.GREEN)

        with open(f"{DIR}/wol.log") as f:
            wol_content = f.read()
        assert "AA:BB:CC:00:00:01" in wol_content
        assert "AA:BB:CC:00:00:02" in wol_content
        assert "AA:BB:CC:00:00:03" in wol_content
        log("  [OK] Wake-on-LAN magic packets confirmed received for all 3 targets", Color.GREEN)
        log("--> SCENARIO D: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_e(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO E: Grid flickers back, drops with SOC already low", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 35")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=35\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)

        self.send_sim_cmd("restore")
        self.wait_for_bridge_log(r"state: OnBattery -> RecoveryDebouncing")
        self.send_sim_cmd("soc 30")
        time.sleep(0.5)
        self.send_sim_cmd("outage")

        line = self.wait_for_bridge_log(r"firing shutdown sequence", timeout=8)
        log(f"  [OK] {line}", Color.GREEN)
        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=25)
        log("  [OK] All 3 endpoints safely shut down during grid flicker", Color.GREEN)
        log("--> SCENARIO E: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_f(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO F: Bridge restarts mid-outage -- remembers the shutdown", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=25)
        assert os.path.exists(f"{DIR}/shutdown_fired")

        self.stop_bridge()
        with open(f"{DIR}/ssh.log") as f:
            lines_before = len([l for l in f.read().splitlines() if l.strip()])

        self.start_bridge()
        line = self.wait_for_bridge_log(r"shutdown_fired exists: a previous run shut the endpoints down", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(2)
        with open(f"{DIR}/ssh.log") as f:
            lines_after = len([l for l in f.read().splitlines() if l.strip()])
        assert lines_before == lines_after, "No new SSH commands should fire on restart"
        log("  [OK] State is latched after restart: no duplicate shutdown", Color.GREEN)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=8)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        self.wait_for_bridge_log(r"Wake-on-LAN round 4/4", timeout=25)
        time.sleep(1)
        assert not os.path.exists(f"{DIR}/shutdown_fired")
        log("  [OK] Marker file deleted after WOL completed", Color.GREEN)
        log("--> SCENARIO F: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_f2(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO F2: Bridge restarts mid-sequence -- resumes remaining endpoints", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("outage")
        self.send_sim_cmd("soc 25")

        # Simulate marker created when ws-1 and ws-2 were dispatched, but bridge restarted before proxmox
        with open(f"{DIR}/shutdown_fired", "w") as f:
            f.write("# shutdown sequence in progress\ndispatched: ws-1\ndispatched: ws-2\n")

        self.start_bridge()
        line = self.wait_for_bridge_log(r"indicates incomplete shutdown: 2 endpoint\(s\) already dispatched, 1 remaining", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"resuming shutdown sequence for 1 remaining endpoint\(s\)", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=25)
        log("  [OK] Remaining shutdown sequence completed", Color.GREEN)

        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert "10.99.0.3" in ssh_content, "proxmox should have been dispatched"
        assert "10.99.0.1" not in ssh_content, "ws-1 should not be re-dispatched"
        assert "10.99.0.2" not in ssh_content, "ws-2 should not be re-dispatched"
        log("  [OK] Only proxmox was dispatched; no duplicate shutdown for ws-1/ws-2", Color.GREEN)

        with open(f"{DIR}/shutdown_fired") as f:
            marker_content = f.read()
        assert "completed" in marker_content
        log("  [OK] Marker file marked completed", Color.GREEN)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=8)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        self.wait_for_bridge_log(r"Wake-on-LAN round 4/4", timeout=25)
        time.sleep(1)
        assert not os.path.exists(f"{DIR}/shutdown_fired")
        log("--> SCENARIO F2: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_f3(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO F3: Bridge restarts mid-sequence during grid voltage flicker", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/shutdown_fired", "w") as f:
            f.write("# shutdown sequence in progress\ndispatched: ws-1\ndispatched: ws-2\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 25")
        time.sleep(0.5)

        self.start_bridge()
        line = self.wait_for_bridge_log(r"indicates incomplete shutdown: 2 endpoint\(s\) already dispatched, 1 remaining", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"grid currently up; holding 1 remaining shutdown\(s\) pending (?:recovery )?confirmation", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(2)
        with open(f"{DIR}/ssh.log") as f:
            assert len(f.read().strip()) == 0

        self.send_sim_cmd("outage")
        line = self.wait_for_bridge_log(r"grid (?:still|confirmed) down: resuming shutdown sequence for 1 remaining endpoint\(s\)", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=25)
        log("  [OK] Remaining shutdown sequence completed", Color.GREEN)

        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert "10.99.0.3" in ssh_content
        assert "10.99.0.1" not in ssh_content
        assert "10.99.0.2" not in ssh_content

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=8)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        self.wait_for_bridge_log(r"Wake-on-LAN round 4/4", timeout=25)
        time.sleep(1)
        assert not os.path.exists(f"{DIR}/shutdown_fired")
        log("--> SCENARIO F3: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_f4(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO F4: Failed endpoint omitted from marker & retried; Proxmox timing", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/ssh-fail", "w") as f:
            f.write("10.99.0.2\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"loaded config from .*bridge\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)

        self.wait_for_bridge_log(r"shutting down ws-1", timeout=8)
        self.wait_for_bridge_log(r"ws-2: initial connection failed .* continuing retries in background", timeout=10)

        # proxmox starts VM shutdown
        line = self.wait_for_bridge_log(r"shutting down proxmox \(10\.99\.0\.3\) via Proxmox: VMs first, then poweroff", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        with open(f"{DIR}/shutdown_fired") as f:
            marker_content = f.read()
        assert "dispatched: ws-1" in marker_content
        assert "dispatched: ws-2" not in marker_content
        assert "dispatched: proxmox" not in marker_content
        log("  [OK] Marker correctly excludes failed ws-2 and mid-flight proxmox", Color.GREEN)

        self.stop_bridge()
        if os.path.exists(f"{DIR}/ssh-fail"):
            os.remove(f"{DIR}/ssh-fail")

        self.start_bridge()
        line = self.wait_for_bridge_log(r"indicates incomplete shutdown: 1 endpoint\(s\) already dispatched, 2 remaining", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"resuming shutdown sequence for 2 remaining endpoint\(s\)", timeout=10)
        self.wait_for_bridge_log(r"ws-2: shutdown command accepted", timeout=8)
        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=30)
        log("  [OK] Shutdown sequence finished successfully", Color.GREEN)

        with open(f"{DIR}/shutdown_fired") as f:
            final_marker = f.read()
        assert "completed" in final_marker
        assert "dispatched: ws-2" in final_marker
        assert "dispatched: proxmox" in final_marker

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=10)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO F4: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_f5(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO F5: Failed endpoint retried after sequence finishes", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/ssh-fail", "w") as f:
            f.write("10.99.0.2\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        self.wait_for_bridge_log(r"shutting down ws-1", timeout=8)
        self.wait_for_bridge_log(r"ws-2: initial connection failed", timeout=10)
        self.wait_for_bridge_log(r"shutting down proxmox", timeout=10)
        self.wait_for_bridge_log(r"failed to shut down ws-2", timeout=15)

        line = self.wait_for_bridge_log(r"shutdown sequence finished: 2/3 endpoint\(s\) succeeded; marker left incomplete for retry on restart", timeout=25)
        log(f"  [OK] {line}", Color.GREEN)

        with open(f"{DIR}/shutdown_fired") as f:
            marker_content = f.read()
        assert "dispatched: ws-1" in marker_content
        assert "dispatched: proxmox" in marker_content
        assert "dispatched: ws-2" not in marker_content
        assert "completed" not in marker_content

        self.stop_bridge()
        if os.path.exists(f"{DIR}/ssh-fail"):
            os.remove(f"{DIR}/ssh-fail")

        self.start_bridge()
        line = self.wait_for_bridge_log(r"indicates incomplete shutdown: 2 endpoint\(s\) already dispatched, 1 remaining: \[\"ws-2\"\]", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"ws-2: shutdown command accepted", timeout=8)
        self.wait_for_bridge_log(r"shutdown sequence complete: all 1 endpoint\(s\) succeeded", timeout=10)

        with open(f"{DIR}/shutdown_fired") as f:
            final_marker = f.read()
        assert "completed" in final_marker
        assert "dispatched: ws-2" in final_marker

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=10)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO F5: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_g(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO G: Inverter goes silent -- no shutdown on missing data", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        if self.sim_proc:
            self.sim_proc.kill()
            self.sim_proc.wait()
            self.sim_proc = None

        line = self.wait_for_bridge_log(r"modbus poll failed: timed out reading register", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(5)
        assert not os.path.exists(f"{DIR}/shutdown_fired")
        with open(f"{DIR}/ssh.log") as f:
            assert len(f.read().strip()) == 0
        log("  [OK] Verified no shutdown triggered during silence", Color.GREEN)

        self.start_simulator()
        line = self.wait_for_bridge_log(r"inverter: device type 0x0300", timeout=15)
        log(f"  [OK] Reconnected to inverter: {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V", timeout=10)
        log(f"  [OK] Normal polling resumed: {line}", Color.GREEN)
        log("--> SCENARIO G: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_h(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO H: Garbage SOC reading -- treated as bad read", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)

        self.send_sim_cmd("set 184 150")
        line = self.wait_for_bridge_log(r"modbus poll failed: battery SOC register read 150", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(3)
        assert not os.path.exists(f"{DIR}/shutdown_fired")
        with open(f"{DIR}/ssh.log") as f:
            assert len(f.read().strip()) == 0
        log("  [OK] Verified garbage SOC did not cause shutdown", Color.GREEN)

        self.send_sim_cmd("soc 60")
        self.send_sim_cmd("restore")
        self.wait_for_bridge_log(r"state: OnBattery -> RecoveryDebouncing", timeout=10)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO H: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_k(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO K: New outage during WOL rounds -- rounds cancelled", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=25)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        self.wait_for_bridge_log(r"Wake-on-LAN round 1/4", timeout=10)

        # Outage during WOL
        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing", timeout=5)
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"cancelling pending Wake-on-LAN resends|firing shutdown sequence", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=25)
        assert os.path.exists(f"{DIR}/shutdown_fired")
        log("  [OK] Marker file re-created", Color.GREEN)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=10)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO K: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_l(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO L: One Endpoint Unreachable (Resilience)", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        with open(f"{DIR}/ssh-fail", "w") as f:
            f.write("10.99.0.2\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)

        line = self.wait_for_bridge_log(r"failed to shut down ws-2", timeout=25)
        log(f"  [OK] Caught expected failure: {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence (?:complete|finished)", timeout=35)
        with open(f"{DIR}/ssh.log") as f:
            content = f.read()
        assert "10.99.0.2 FAILED (simulated)" in content
        assert "10.99.0.3" in content, "proxmox was not contacted after ws-2 failed!"
        log("  [OK] Endpoint failure did not stop proxmox from shutting down", Color.GREEN)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing")
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO L: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_m(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO M: One endpoint hangs, grid returns mid-sequence", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/ssh-hang", "w") as f:
            f.write("10.99.0.2\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)

        t0 = time.time()
        hung = False
        while time.time() - t0 < 10:
            if os.path.exists(f"{DIR}/ssh.log"):
                with open(f"{DIR}/ssh.log") as f:
                    if "10.99.0.2 HANGING" in f.read():
                        hung = True
                        break
            time.sleep(0.2)
        assert hung, "ws-2 did not hang as expected"
        log("  [OK] ws-2 began hanging as expected", Color.GREEN)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")

        line = self.wait_for_bridge_log(r"recovery confirmed -- stopping the rest of the shutdown sequence", timeout=20)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"Wake-on-LAN round 1/4", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        log("    Waiting for ws-2 SSH 60s timeout to elapse...", Color.CYAN)
        line = self.wait_for_bridge_log(r"recovery confirmed -- not starting the remaining endpoints", timeout=80)
        log(f"  [OK] {line}", Color.GREEN)

        with open(f"{DIR}/ssh.log") as f:
            content = f.read()
        assert "10.99.0.3" not in content, "proxmox should not have been contacted"
        log("  [OK] Proxmox was spared after recovery was confirmed", Color.GREEN)
        log("--> SCENARIO M: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n1(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N1: VMs shut down, then poweroff", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"loaded config from .*bridge\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"shutting down proxmox \(10\.99\.0\.3\) via Proxmox: VMs first, then poweroff", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox: 2 VM\(s\) registered, 2 running: dc01, app server", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox: host power-off scheduled via systemctl poweroff", timeout=25, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)
        log("  [OK] Shutdown sequence complete", Color.GREEN)

        with open(f"{DIR}/proxmox/10.99.0.3/host") as f:
            h_pve = f.read()
        assert "poweroff via systemctl" in h_pve
        log("  [OK] Proxmox host recorded poweroff via systemctl", Color.GREEN)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=10)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO N1: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n2(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N2: Hung VM and VM without guest agent", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/proxmox/10.99.0.3/vms", "a") as f:
            f.write("109|stuck-vm|hang\n108|no-tools-vm|notools\n")
        with open(f"{DIR}/proxmox/10.99.0.3/state_109", "w") as f:
            f.write("on\n")
        with open(f"{DIR}/proxmox/10.99.0.3/state_108", "w") as f:
            f.write("on\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"loaded config from .*bridge\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"proxmox: guest shutdown of VM no-tools-vm failed", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox: VM no-tools-vm could not be shut down gracefully -- powering it off hard", timeout=25, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox: VM stuck-vm still running after 15 s -- powering it off hard", timeout=25, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox: host power-off scheduled via systemctl poweroff", timeout=15, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=10)
        log("  [OK] Sequence completed after hard power-off fallback", Color.GREEN)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=10)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO N2: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n3(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N3: systemctl poweroff refused; /sbin/poweroff fallback", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/proxmox/10.99.0.3/systemctl-refuse", "w") as f:
            f.write("refused\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"loaded config from .*bridge\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"proxmox: systemctl poweroff refused .* falling back to /sbin/poweroff", timeout=30)
        log(f"  [OK] Caught systemctl refusal and fell back to /sbin/poweroff: {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=20)

        with open(f"{DIR}/proxmox/10.99.0.3/host") as f:
            content = f.read()
        assert "poweroff via /sbin/poweroff" in content
        log("  [OK] Simulated host confirmed poweroff via /sbin/poweroff fallback", Color.GREEN)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=10)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO N3: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n4(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N4: Grid back while host is shutting down VMs (Safety Verification & VM Restart)", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/proxmox/10.99.0.3/vms", "a") as f:
            f.write("109|stuck-vm|hang\n")
        with open(f"{DIR}/proxmox/10.99.0.3/state_109", "w") as f:
            f.write("on\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"loaded config from .*bridge\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)

        line = self.wait_for_bridge_log(r"shutting down proxmox \(10\.99\.0\.3\) via Proxmox: VMs first, then poweroff", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox: guest shutdown requested for VM stuck-vm", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(2)
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")

        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=10)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)

        line = self.wait_for_bridge_log(r"recovery confirmed -- stopping the rest of the shutdown sequence", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(
            r"proxmox: recovery confirmed while VM shutdowns were in progress -- aborting local waits; no hard stop or host poweroff will be issued",
            timeout=10
        )
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(18)
        for log_line in self.bridge_logs:
            assert "powering it off hard" not in log_line
            assert "host power-off scheduled" not in log_line
            assert "qm stop" not in log_line
        log("  [OK] Bridge logs confirm no hard stops or host poweroff were scheduled", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox: restarting 2 stopped VM\(s\)", timeout=10, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox: VM dc01 \(id 100\) restarted", timeout=10, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox: VM app server \(id 101\) restarted", timeout=10, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)

        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert "qm stop" not in ssh_content
        assert "poweroff" not in ssh_content
        assert "qm start 100" in ssh_content
        assert "qm start 101" in ssh_content
        assert "qm start 109" not in ssh_content
        log("  [OK] Stopped VMs were restarted via qm start and host was preserved", Color.GREEN)
        assert not os.path.exists(f"{DIR}/proxmox/10.99.0.3/host")
        log("--> SCENARIO N4: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n5(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N5: Proxmox host unreachable", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/ssh-fail", "w") as f:
            f.write("10.99.0.3\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"loaded config from .*bridge\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        self.wait_for_bridge_log(r"shutting down ws-1", timeout=8)
        self.wait_for_bridge_log(r"shutting down ws-2", timeout=8)

        line = self.wait_for_bridge_log(r"failed to shut down proxmox: listing VMs: ssh to 10\.99\.0\.3 exited Some\(255\)", timeout=20)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence (?:complete|finished)", timeout=10)
        log("  [OK] ws-1 and ws-2 completed while proxmox failed gracefully", Color.GREEN)
        log("--> SCENARIO N5: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_o(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO O: Host Booting on Wakeup (SSH Connection Retry)", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/ssh-booting-10.99.0.1", "w") as f:
            f.write("2\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)

        line = self.wait_for_bridge_log(r"ws-1: initial connection failed .* continuing retries in background", timeout=8)
        log(f"  [OK] ws-1 moved to background retry: {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"shutting down ws-2", timeout=5)
        log(f"  [OK] ws-2 dispatched immediately without head-of-line blocking: {line}", Color.GREEN)
        self.wait_for_bridge_log(r"ws-2: shutdown command accepted", timeout=5)

        line = self.wait_for_bridge_log(r"ws-1: host finished booting; shutdown command accepted", timeout=12)
        log(f"  [OK] ws-1 recovered in background and succeeded: {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=30)
        log("  [OK] Shutdown sequence finished successfully with all endpoints completed", Color.GREEN)

        state_file = f"{DIR}/shutdown_fired"
        assert os.path.exists(state_file)
        with open(state_file) as f:
            marker_content = f.read()
        assert "dispatched: ws-1" in marker_content
        assert "dispatched: ws-2" in marker_content
        assert "dispatched: proxmox" in marker_content
        assert "completed" in marker_content

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing")
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO O: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_p(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO P: Server Slow to Boot -- Extended Per-Endpoint Retry Budget", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        # Simulate proxmox (10.99.0.3) booting slowly: 4 connection refused attempts (8 seconds)
        # proxmox has ssh_connect_retry_secs = 12
        with open(f"{DIR}/ssh-booting-10.99.0.3", "w") as f:
            f.write("4\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)

        self.wait_for_bridge_log(r"ws-1: shutdown command accepted", timeout=8)
        self.wait_for_bridge_log(r"ws-2: shutdown command accepted", timeout=8)

        line = self.wait_for_bridge_log(r"proxmox: SSH connection failed .* host may still be booting; retrying", timeout=10)
        log(f"  [OK] proxmox retrying in background with 12s budget: {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox: 2 VM\(s\) registered, 2 running", timeout=20)
        log(f"  [OK] proxmox finished booting and started VM shutdown: {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=35)

        state_file = f"{DIR}/shutdown_fired"
        assert os.path.exists(state_file)
        with open(state_file) as f:
            marker_content = f.read()
        assert "dispatched: ws-1" in marker_content
        assert "dispatched: ws-2" in marker_content
        assert "dispatched: proxmox" in marker_content
        assert "completed" in marker_content

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing")
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO P: PASSED", Color.GREEN + Color.BOLD)


    def test_scenario_q(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO Q: Grid back while a slow VM is still shutting down -- it is restarted too", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        with open(f"{DIR}/proxmox/10.99.0.3/vms", "a") as f:
            f.write("110|slow-vm|ok:14\n")
        with open(f"{DIR}/proxmox/10.99.0.3/state_110", "w") as f:
            f.write("on\n")
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"loaded config from .*bridge\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"proxmox: guest shutdown requested for VM slow-vm", timeout=15)
        time.sleep(1)
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")

        line = self.wait_for_bridge_log(r"recovery confirmed while VM shutdowns were in progress", timeout=25)
        log(f"  [OK] {line}", Color.GREEN)
        # slow-vm is still shutting down at this moment (it takes 14 s). One look
        # would find it "running" and miss it; the watch must catch it later.
        line = self.wait_for_bridge_log(r"proxmox: VM slow-vm \(id 110\) restarted", timeout=30)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox: restart watch finished: 3 VM\(s\) restarted", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        for i in (100, 101, 110):
            with open(f"{DIR}/proxmox/10.99.0.3/state_{i}") as f:
                assert f.read().strip() == "on", f"VM {i} must be running again"
        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert "qm start 110" in ssh_content
        assert "qm stop" not in ssh_content
        assert not os.path.exists(f"{DIR}/proxmox/10.99.0.3/host")
        log("  [OK] All three VMs are running again; no hard stop, host left up", Color.GREEN)
        log("--> SCENARIO Q: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_r(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO R: sshd drops the VM shutdown logins -- retried, nothing hard-stopped", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        with open(f"{DIR}/ssh-flaky-qm-shutdown-10.99.0.3", "w") as f:
            f.write("2\n")
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"loaded config from .*bridge\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"proxmox: SSH connection for `.*qm shutdown.*` failed", timeout=20)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox: host power-off scheduled via systemctl poweroff", timeout=40, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)

        for l in self.bridge_logs:
            assert "powering it off hard" not in l, l
            assert "will be powered off" not in l, l
        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert ssh_content.count("DROPPED (simulated)") == 2, ssh_content
        assert "qm stop" not in ssh_content
        for i in (100, 101):
            with open(f"{DIR}/proxmox/10.99.0.3/state_{i}") as f:
                assert f.read().strip() == "off", f"VM {i} must have been shut down gracefully"
        log("  [OK] Two dropped logins were retried; both VMs shut down gracefully, none hard-stopped", Color.GREEN)
        log("--> SCENARIO R: PASSED", Color.GREEN + Color.BOLD)

    def _strict_config(self):
        path = f"{DIR}/bridge-strict.toml"
        with open(CONFIG) as f:
            text = f.read()
        marker = 'wol_broadcast_addr = "127.0.0.1:40009"'
        assert marker in text
        text = text.replace(marker, marker + "\nstrict_inverter_checks = true", 1)
        with open(path, "w") as f:
            f.write(text)
        return path

    def test_scenario_s(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO S: Inverter settings problems -- default mode keeps protecting the site", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        # (a) A device that is not the expected inverter: its data cannot be trusted.
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.send_sim_cmd("set 0 0x0500")
        self.start_bridge()
        line = self.wait_for_bridge_log(r"wrong device type 0x0500.*refusing to operate", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        time.sleep(3)
        assert not [l for l in self.bridge_logs if "soc=" in l and "grid=" in l], "must not poll a device it cannot trust"
        self.send_sim_cmd("set 0 0x0300")
        line = self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V", timeout=15)
        log(f"  [OK] Right device again, monitoring starts: {line}", Color.GREEN)
        self.stop_bridge()

        # (b) The cutoff on the inverter differs from the config: logged, but still protecting.
        self.reset_env()
        self.send_sim_cmd("set 217 15")
        self.start_bridge()
        line = self.wait_for_bridge_log(r"inverter settings: config inverter_cutoff_soc is 20% but the inverter is set to 15%", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"monitoring continues anyway", timeout=5)
        log(f"  [OK] {line}", Color.GREEN)
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V", timeout=10)
        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        line = self.wait_for_bridge_log(r"firing shutdown sequence", timeout=8)
        log(f"  [OK] Outage with a low battery still shuts the site down: {line}", Color.GREEN)
        self.stop_bridge()
        self.send_sim_cmd("set 217 20")
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")

        # (c) The inverter answers the polls but not the settings registers: one
        # clean reconnect, then monitoring without the check -- and the check is
        # tried again (here: once the registers answer) instead of being given up.
        self.reset_env()
        self.send_sim_cmd("mute 0 213 217 220")
        self.start_bridge()
        line = self.wait_for_bridge_log(r"inverter settings unreadable \(1 in a row\) -- reconnecting", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"monitoring WITHOUT a settings check", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V", timeout=10)
        log(f"  [OK] The site is being monitored regardless: {line}", Color.GREEN)
        self.send_sim_cmd("unmute")
        line = self.wait_for_bridge_log(r"trying the inverter settings check again", timeout=40)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"inverter: device type 0x0300", timeout=20)
        log(f"  [OK] Check completes once the inverter answers: {line}", Color.GREEN)
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V", timeout=10)
        log("--> SCENARIO S: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_t(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO T: strict_inverter_checks = true -- refuses instead of running unchecked", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        strict = self._strict_config()

        # (a) A settings error stops monitoring until it is fixed.
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.send_sim_cmd("set 217 15")
        self.start_bridge(config=strict)
        line = self.wait_for_bridge_log(r"strict_inverter_checks is enabled and the inverter settings have errors -- refusing to operate", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        time.sleep(3)
        assert not [l for l in self.bridge_logs if "soc=" in l and "grid=" in l], "strict mode must not monitor"
        self.send_sim_cmd("set 217 20")
        line = self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V", timeout=15)
        log(f"  [OK] Fixed on the inverter: monitoring starts: {line}", Color.GREEN)
        self.stop_bridge()

        # (b) Unreadable settings are never run unchecked in strict mode.
        self.reset_env()
        self.send_sim_cmd("mute 0 213 217 220")
        self.start_bridge(config=strict)
        self.wait_for_bridge_log(r"inverter settings unreadable \(2 in a row\)", timeout=30)
        time.sleep(3)
        assert not [l for l in self.bridge_logs if "WITHOUT a settings check" in l], "strict mode must keep retrying"
        assert not [l for l in self.bridge_logs if "soc=" in l and "grid=" in l]
        self.send_sim_cmd("unmute")
        line = self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V", timeout=30)
        log(f"  [OK] Registers answer again: monitoring starts: {line}", Color.GREEN)
        log("--> SCENARIO T: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_u(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO U: Sagging grid -- voltage still 150 V, but the grid relay is open", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V relay=closed")

        # A low voltage reading alone, with the relay still closed, is above
        # grid_lost_voltage (100 V): nothing may happen.
        self.send_sim_cmd("grid 150")
        self.wait_for_bridge_log(r"soc=80\.0% grid=150\.0V relay=closed")
        time.sleep(7)
        states = [l for l in self.bridge_logs[self.log_cursor:] if "state:" in l]
        assert not states, f"150 V with the relay closed must not count as an outage: {states}"
        log("  [OK] 150 V with the relay closed is not an outage", Color.GREEN)

        # The inverter lets go of the grid: voltage still reads 150 V, relay opens.
        self.send_sim_cmd("relay 0")
        line = self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        log(f"  [OK] {line}", Color.GREEN)

        self.send_sim_cmd("soc 25")
        line = self.wait_for_bridge_log(r"firing shutdown sequence", timeout=8)
        log(f"  [OK] {line}", Color.GREEN)
        assert os.path.exists(f"{DIR}/shutdown_fired"), "Marker file must exist after the shutdown fired"
        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=60)

        # Relay closes again (voltage still 150 V): recovery starts.
        self.send_sim_cmd("relay 1")
        line = self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing")
        log(f"  [OK] {line}", Color.GREEN)
        self.send_sim_cmd("restore")
        line = self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"Wake-on-LAN round 1/4", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        log("--> SCENARIO U: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_v(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO V: Comms loss on battery -- fail-safe shutdown fired", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        # Grid drops, battery still healthy (SOC 80%)
        self.send_sim_cmd("outage")
        line = self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        log(f"  [OK] {line}", Color.GREEN)

        # Inverter RS485 communication is lost while running on battery
        log("--> Killing inverter simulator to simulate comms loss during outage...", Color.CYAN)
        if self.sim_proc:
            self.sim_proc.kill()
            self.sim_proc.wait()
            self.sim_proc = None

        # After comms_loss_shutdown_secs (5 s), the fail-safe must fire
        line = self.wait_for_bridge_log(r"battery state unknown, firing shutdown sequence", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"shutdown sequence starting: 3 endpoint\(s\)", timeout=5)
        log(f"  [OK] {line}", Color.GREEN)

        assert os.path.exists(f"{DIR}/shutdown_fired"), "Marker file must exist after fail-safe shutdown fired"
        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=45)
        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert "10.99.0.1" in ssh_content and "10.99.0.2" in ssh_content and "10.99.0.3" in ssh_content
        log("  [OK] All endpoints dispatched in fail-safe shutdown", Color.GREEN)

        # Simulator returns with grid back (default 230 V, SOC 80%)
        log("--> Restarting inverter simulator (grid restored)...", Color.CYAN)
        self.start_simulator()
        self.wait_for_bridge_log(r"inverter: device type 0x0300", timeout=15)
        line = self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"Wake-on-LAN round 1/4", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        log("--> SCENARIO V: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_w(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO W: Windows shutdown already scheduled (error 1190) accepted", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        # Simulate Windows returning error 1190 for ws-1
        with open(f"{DIR}/ssh-already-scheduled-10.99.0.1", "w") as f:
            f.write("1\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=8)

        # ws-1 reports error 1190, bridge treats it as accepted
        line = self.wait_for_bridge_log(r"ws-1: a shutdown is already scheduled on the host \(error 1190\) -- treating as accepted", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        # ws-2 and proxmox proceed normally
        self.wait_for_bridge_log(r"ws-2: shutdown command accepted", timeout=10)
        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=45)

        with open(f"{DIR}/shutdown_fired") as f:
            marker = f.read()
        assert "completed" in marker
        assert "dispatched: ws-1" in marker
        log("  [OK] ws-1 recorded as dispatched in manifest despite error 1190", Color.GREEN)

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=10)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO W: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_x(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO X: Restart mid-sequence, inverter silent -- remaining endpoint still shut down", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        with open(f"{DIR}/ssh-fail", "w") as f:
            f.write("10.99.0.2\n")
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")
        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"shutdown sequence finished: 2/3 endpoint\(s\) succeeded; marker left incomplete for retry on restart", timeout=45)

        # The bridge restarts (watchdog, power blip) and the inverter does not answer:
        # nothing can confirm that the grid is down, so the waiting endpoint must go
        # after comms_loss_shutdown_secs (5 s in the test config), not never.
        self.stop_bridge()
        os.remove(f"{DIR}/ssh-fail")
        if self.sim_proc:
            self.sim_proc.kill()
            self.sim_proc.wait()
            self.sim_proc = None
        self.start_bridge()
        line = self.wait_for_bridge_log(r"indicates incomplete shutdown: 2 endpoint\(s\) already dispatched, 1 remaining", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"no valid inverter reading for \d+ s after a restart that interrupted a shutdown", timeout=20)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"inverter silent: resuming shutdown sequence for 1 remaining endpoint\(s\)", timeout=5)
        log(f"  [OK] {line}", Color.GREEN)
        self.wait_for_bridge_log(r"ws-2: shutdown command accepted", timeout=15)
        self.wait_for_bridge_log(r"shutdown sequence complete: all 1 endpoint\(s\) succeeded", timeout=10)
        log("  [OK] The endpoint the previous run never reached was shut down without any reading", Color.GREEN)

        # Inverter comes back with the grid up: normal recovery.
        self.start_simulator()
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=20)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log("--> SCENARIO X: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_y(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO Y: Restart mid-sequence, grid up then inverter silent -- waiting endpoint spared", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        # Simulate marker created when ws-1 and ws-2 were dispatched, 1 remaining (proxmox)
        with open(f"{DIR}/shutdown_fired", "w") as f:
            f.write("# shutdown sequence in progress\ndispatched: ws-1\ndispatched: ws-2\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        line = self.wait_for_bridge_log(r"indicates incomplete shutdown: 2 endpoint\(s\) already dispatched, 1 remaining", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"grid currently up; holding 1 remaining shutdown\(s\) pending (?:recovery )?confirmation", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        # Before recovery debounce (10 s) completes, kill the simulator.
        # The inverter is now silent, but the last reading saw the grid was UP.
        # The comms loss fail-safe must NOT fire for the waiting endpoint!
        if self.sim_proc:
            self.sim_proc.kill()
            self.sim_proc.wait()
            self.sim_proc = None

        self.wait_for_bridge_log(r"modbus poll failed: .* -- reconnecting", timeout=10)

        # Sleep well past comms_loss_shutdown_secs (5 s in test config)
        time.sleep(8)

        # Verify no shutdown command was issued during this silence
        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert "10.99.0.3" not in ssh_content, "proxmox must not be dispatched when last reading saw grid up"

        for b_line in self.bridge_logs:
            assert "inverter silent: resuming shutdown sequence" not in b_line, "fail-safe must not fire when grid was up"
        log("  [OK] Waiting endpoint spared despite inverter silence because grid was seen up", Color.GREEN)

        # Inverter comes back with grid up: normal recovery completes and spares the endpoint
        self.start_simulator()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        line = self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=20)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"recovery confirmed: 1 remaining endpoint\(s\) were spared from shutdown", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        self.wait_for_bridge_log(r"Wake-on-LAN round 4/4", timeout=25)
        time.sleep(1)
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "marker file must be cleared after recovery"

        with open(f"{DIR}/ssh.log") as f:
            assert "10.99.0.3" not in f.read(), "proxmox was never dispatched"
        log("--> SCENARIO Y: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_z(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO Z: Endpoint finishes shutdown after recovery confirmed -- marker write skipped", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        # Configure ws-1 to delay in SSH by 28s so it completes after recovery and WOL clear the marker
        with open(f"{DIR}/ssh-delay-10.99.0.1", "w") as f:
            f.write("28\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)
        self.wait_for_bridge_log(r"shutting down ws-1 \(10\.99\.0\.1\) via Windows", timeout=5)

        # Restore grid immediately while ws-1 is still sleeping in ssh (28 s delay)
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=8)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)

        line = self.wait_for_bridge_log(r"recovery confirmed -- stopping the rest of the shutdown sequence", timeout=5)
        log(f"  [OK] {line}", Color.GREEN)
        self.wait_for_bridge_log(r"Wake-on-LAN round 1/4", timeout=5)

        # Wait for Wake-on-LAN rounds to complete and clear the marker
        self.wait_for_bridge_log(r"Wake-on-LAN round 4/4", timeout=25)
        time.sleep(1)

        # Now ws-1's delayed SSH completes after recovery was confirmed and marker was cleared
        line = self.wait_for_bridge_log(r"ws-1: shutdown command accepted", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"recovery confirmed -- not starting the remaining endpoints", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(1)

        # Assert marker file does NOT exist and was not resurrected
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "marker file must not be resurrected after recovery"
        log("  [OK] Marker file was not resurrected by late endpoint completion", Color.GREEN)

        # Further endpoints were spared
        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert "10.99.0.2" not in ssh_content, "ws-2 must not be contacted"
        assert "10.99.0.3" not in ssh_content, "proxmox must not be contacted"

        # Restart bridge to confirm clean state
        self.stop_bridge()
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V", timeout=10)
        for b_line in self.bridge_logs:
            assert "indicates incomplete shutdown" not in b_line, "bridge must not see residual marker"
        log("--> SCENARIO Z: PASSED", Color.GREEN + Color.BOLD)

    def run(self, targets=None):
        all_scenarios = [
            ("a", "Scenario A", self.test_scenario_a),
            ("b", "Scenario B", self.test_scenario_b),
            ("c", "Scenario C", self.test_scenario_c),
            ("d", "Scenario D", self.test_scenario_d),
            ("e", "Scenario E", self.test_scenario_e),
            ("f", "Scenario F", self.test_scenario_f),
            ("f2", "Scenario F2", self.test_scenario_f2),
            ("f3", "Scenario F3", self.test_scenario_f3),
            ("f4", "Scenario F4", self.test_scenario_f4),
            ("f5", "Scenario F5", self.test_scenario_f5),
            ("g", "Scenario G", self.test_scenario_g),
            ("h", "Scenario H", self.test_scenario_h),
            ("k", "Scenario K", self.test_scenario_k),
            ("l", "Scenario L", self.test_scenario_l),
            ("m", "Scenario M", self.test_scenario_m),
            ("n1", "Scenario N1", self.test_scenario_n1),
            ("n2", "Scenario N2", self.test_scenario_n2),
            ("n3", "Scenario N3", self.test_scenario_n3),
            ("n4", "Scenario N4", self.test_scenario_n4),
            ("n5", "Scenario N5", self.test_scenario_n5),
            ("o", "Scenario O", self.test_scenario_o),
            ("p", "Scenario P", self.test_scenario_p),
            ("q", "Scenario Q", self.test_scenario_q),
            ("r", "Scenario R", self.test_scenario_r),
            ("s", "Scenario S", self.test_scenario_s),
            ("t", "Scenario T", self.test_scenario_t),
            ("u", "Scenario U", self.test_scenario_u),
            ("v", "Scenario V", self.test_scenario_v),
            ("w", "Scenario W", self.test_scenario_w),
            ("x", "Scenario X", self.test_scenario_x),
            ("y", "Scenario Y", self.test_scenario_y),
            ("z", "Scenario Z", self.test_scenario_z),
        ]

        if targets:
            target_set = set(t.lower() for t in targets)
            scenarios = [s for s in all_scenarios if s[0] in target_set]
        else:
            scenarios = all_scenarios

        try:
            self.stop_existing()
            self.init_keys_and_dirs()
            self.start_cable()
            self.start_wol_listener()
            self.start_simulator()

            passed = 0
            for code, name, test_fn in scenarios:
                try:
                    test_fn()
                    passed += 1
                except Exception as ex:
                    log(f"--> {name}: FAILED: {ex}", Color.RED + Color.BOLD)
                    raise

            log("\n=======================================================", Color.BOLD)
            log(f"ALL SELECTED LEVEL-2 SCENARIOS ({passed}/{len(scenarios)}) PASSED SUCCESSFULLY!", Color.GREEN + Color.BOLD)
            log("Config: 2 Windows 11 endpoints, 1 Proxmox endpoint (vms_then_poweroff)", Color.CYAN + Color.BOLD)
            log("=======================================================", Color.BOLD)
        finally:
            self.cleanup()

if __name__ == "__main__":
    targets = sys.argv[1:]
    runner = AllLevel2Runner()
    runner.run(targets)
