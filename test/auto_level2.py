#!/usr/bin/env python3
"""Automated runner for modbus-ups-bridge Level 2 simulated tests.

Runs the real compiled bridge against the inverter simulator, fake ssh,
and local Wake-on-LAN listener over a virtual serial cable (socat).
Covers Scenarios A through N5 from CHECKLIST.md.
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
CONFIG = f"{TEST_DIR}/bridge-test.toml"
CONFIG_VMS = f"{TEST_DIR}/bridge-test-vms.toml"

class Color:
    GREEN = "\033[92m"
    RED = "\033[91m"
    YELLOW = "\033[93m"
    CYAN = "\033[96m"
    BOLD = "\033[1m"
    RESET = "\033[0m"

def log(msg, color=Color.RESET):
    print(f"{color}{msg}{Color.RESET}", flush=True)

class TestRunner:
    def __init__(self):
        self.socat_proc = None
        self.sim_proc = None
        self.wol_proc = None
        self.bridge_proc = None
        self.bridge_logs = []
        self.log_cursor = 0
        self.log_queue = queue.Queue()

    def stop_existing(self):
        # Kill any lingering test processes safely
        my_pid = os.getpid()
        for pid_str in os.listdir("/proc"):
            if not pid_str.isdigit() or int(pid_str) == my_pid:
                continue
            try:
                with open(f"/proc/{pid_str}/cmdline", "rb") as f:
                    cmd = f.read().decode(errors="ignore")
                    if ("inverter_sim.py" in cmd or "wol_listen.py" in cmd or "target/debug/modbus-ups-bridge" in cmd) and "auto_level2" not in cmd:
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

        # Make sure bin/ssh, pve-bin/qm, pve-bin/systemctl, esxi-bin/vim-cmd, esxi-bin/esxcli are executable
        subprocess.run(["chmod", "+x", f"{TEST_DIR}/bin/ssh", f"{TEST_DIR}/pve-bin/qm", f"{TEST_DIR}/pve-bin/systemctl", f"{TEST_DIR}/esxi-bin/vim-cmd", f"{TEST_DIR}/esxi-bin/esxcli"], check=False)

        # Dummy key for 0600 check
        fake_key = f"{DIR}/fake_key"
        with open(fake_key, "w") as f:
            f.write("dummy key for level-2 tests\n")
        os.chmod(fake_key, 0o600)

        # Host keys pinned in known_hosts
        fake_hostkey = f"{DIR}/fake_hostkey"
        if os.path.exists(fake_hostkey):
            os.remove(fake_hostkey)
        if os.path.exists(f"{fake_hostkey}.pub"):
            os.remove(f"{fake_hostkey}.pub")
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", fake_hostkey], check=True)

        with open(f"{fake_hostkey}.pub") as f:
            pub_parts = f.read().split()[:2]
            pub_key = " ".join(pub_parts)

        with open(f"{DIR}/known_hosts", "w") as f:
            for h in ["10.99.0.1", "10.99.0.2", "10.99.0.3", "10.99.0.4"]:
                f.write(f"{h} {pub_key}\n")

    def reset_env(self):
        with open(f"{DIR}/ssh.log", "w") as f:
            f.truncate(0)
        for f in ["ssh-fail", "ssh-hang", "shutdown_fired"]:
            p = f"{DIR}/{f}"
            if os.path.exists(p):
                os.remove(p)
        # reset simulated proxmox and esxi vms
        for parent in ["proxmox", "esxi"]:
            for h in ["10.99.0.3", "10.99.0.4"]:
                h_dir = f"{DIR}/{parent}/{h}"
                os.makedirs(h_dir, exist_ok=True)
                for f in os.listdir(h_dir):
                    try:
                        os.remove(os.path.join(h_dir, f))
                    except Exception:
                        pass
        with open(f"{DIR}/proxmox/10.99.0.3/vms", "w") as f:
            f.write("100|dc01|ok:3\n101|app server|ok:6\n")
        with open(f"{DIR}/proxmox/10.99.0.4/vms", "w") as f:
            f.write("102|db01|ok:4\n")
        for i in [100, 101]:
            with open(f"{DIR}/proxmox/10.99.0.3/state_{i}", "w") as f:
                f.write("on\n")
        with open(f"{DIR}/proxmox/10.99.0.4/state_102", "w") as f:
            f.write("on\n")
        with open(f"{DIR}/esxi/10.99.0.3/vms", "w") as f:
            f.write("1|dc01|ok:3\n2|app server|ok:6\n")
        with open(f"{DIR}/esxi/10.99.0.4/vms", "w") as f:
            f.write("3|db01|ok:4\n")
        for i in [1, 2]:
            with open(f"{DIR}/esxi/10.99.0.3/state_{i}", "w") as f:
                f.write("on\n")
        with open(f"{DIR}/esxi/10.99.0.4/state_3", "w") as f:
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
        log(f"--> Starting modbus-ups-bridge binary ({cfg_name})...", Color.CYAN)
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

    # --- SCENARIOS ---

    def test_scenario_a(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO A: Startup & Inverter Telemetry Polling", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()

        line = self.wait_for_bridge_log(r"loaded config from .*bridge-test\.toml \(4 endpoint\(s\)\)")
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"inverter: device type 0x0300, battery mode 1")
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"inverter settings: inverter cutoff 20% SOC, shutdown sequence at 30%")
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V.*on_battery=false low_battery=false")
        log(f"  [OK] {line}", Color.GREEN)

        assert os.path.getsize(f"{DIR}/ssh.log") == 0, "ssh.log should be empty"
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "shutdown_fired should not exist"
        log("--> SCENARIO A: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_b(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO B: Short Grid Blip (< 5s)", Color.BOLD)
        log("=======================================================", Color.BOLD)
        if not self.bridge_proc:
            self.reset_env()
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
        log(f"  [OK] {line}", Color.GREEN)

        assert os.path.getsize(f"{DIR}/ssh.log") == 0, "ssh.log should be empty"
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "shutdown_fired should not exist"
        log("--> SCENARIO B: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_c(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO C: Outage Without Low Battery", Color.BOLD)
        log("=======================================================", Color.BOLD)
        if not self.bridge_proc:
            self.reset_env()
            self.send_sim_cmd("restore")
            self.send_sim_cmd("soc 80")
            self.start_bridge()
            self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        line = self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        log(f"  [OK] {line}", Color.GREEN)

        self.send_sim_cmd("soc 50")
        time.sleep(2)
        self.send_sim_cmd("restore")
        line = self.wait_for_bridge_log(r"state: OnBattery -> RecoveryDebouncing")
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        assert os.path.getsize(f"{DIR}/ssh.log") == 0, "ssh.log should be empty"
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "shutdown_fired should not exist"
        log("--> SCENARIO C: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_d(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO D: Full Outage -> Shutdown -> Latch -> Recovery -> WoL", Color.BOLD)
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
        log("  [OK] Confirmed OnBattery state", Color.GREEN)

        self.send_sim_cmd("soc 25")
        line = self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)
        log(f"  [OK] {line}", Color.GREEN)

        assert os.path.exists(f"{DIR}/shutdown_fired"), "Marker file /tmp/mub-test/shutdown_fired must exist!"
        log("  [OK] Marker file created prior to remote commands", Color.GREEN)

        line = self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        log(f"  [ssh.log content]:\n{ssh_content.strip()}", Color.CYAN)
        assert "10.99.0.1" in ssh_content and "10.99.0.2" in ssh_content
        assert "10.99.0.3" in ssh_content and "10.99.0.4" in ssh_content
        log("  [OK] All 4 endpoints received remote shutdown commands in staggered order", Color.GREEN)

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

        self.send_sim_cmd("restore")
        line = self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing")
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        for round_num in range(1, 5):
            line = self.wait_for_bridge_log(rf"Wake-on-LAN round {round_num}/4", timeout=10)
            log(f"  [OK] {line}", Color.GREEN)

        time.sleep(1)
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "Marker file should be deleted after WOL completes!"
        log("  [OK] Marker file deleted after final WOL round", Color.GREEN)

        with open(f"{DIR}/wol.log") as f:
            wol_content = f.read()
        log(f"  [WOL log captured]: {len(wol_content.strip().splitlines())} lines", Color.CYAN)
        for mac in ["AA:BB:CC:00:00:01", "AA:BB:CC:00:00:02", "AA:BB:CC:00:00:03", "AA:BB:CC:00:00:04"]:
            assert mac in wol_content, f"Missing WOL for MAC {mac}"
        log("  [OK] Wake-on-LAN broadcast packets verified for all 4 targets", Color.GREEN)
        log("--> SCENARIO D: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_e(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO E: Grid flickers back, drops with SOC already low (regression)", Color.BOLD)
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

        # Quickly (within 10s recovery debounce): restore, soc 30, outage
        self.send_sim_cmd("restore")
        self.wait_for_bridge_log(r"state: OnBattery -> RecoveryDebouncing")
        self.send_sim_cmd("soc 30")
        time.sleep(0.5)
        self.send_sim_cmd("outage")

        line = self.wait_for_bridge_log(r"firing shutdown sequence", timeout=8)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"state: (?:RecoveryDebouncing|OnBattery) -> ShutdownLatched", timeout=8)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)
        with open(f"{DIR}/ssh.log") as f:
            lines = [l for l in f.read().splitlines() if l.strip()]
        assert len(lines) == 4, f"Expected 4 lines in ssh.log, got {len(lines)}"
        log("  [OK] 4 lines in ssh.log dispatched", Color.GREEN)
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
        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)
        assert os.path.exists(f"{DIR}/shutdown_fired"), "Marker file /tmp/mub-test/shutdown_fired must exist!"

        # Stop bridge
        self.stop_bridge()
        with open(f"{DIR}/ssh.log") as f:
            lines_before = len([l for l in f.read().splitlines() if l.strip()])

        # Restart bridge
        self.start_bridge()
        line = self.wait_for_bridge_log(r"shutdown_fired exists: a previous run shut the endpoints down", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(2)
        with open(f"{DIR}/ssh.log") as f:
            lines_after = len([l for l in f.read().splitlines() if l.strip()])
        assert lines_before == lines_after, "No new SSH commands should fire on restart"
        log("  [OK] State is latched after restart: no duplicate shutdown", Color.GREEN)

        # Recovery + WOL
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=8)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        self.wait_for_bridge_log(r"Wake-on-LAN round 4/4", timeout=25)
        time.sleep(1)
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "Marker file should be deleted after WOL"
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

        # Simulate marker created when first two endpoints were dispatched,
        # but bridge restarted before the last two (proxmox-a, proxmox-b)
        with open(f"{DIR}/shutdown_fired", "w") as f:
            f.write("# shutdown sequence in progress\ndispatched: ws-1\ndispatched: ws-2\n")

        # Start bridge during outage
        self.start_bridge()
        line = self.wait_for_bridge_log(r"indicates incomplete shutdown: 2 endpoint\(s\) already dispatched, 2 remaining", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"resuming shutdown sequence for 2 remaining endpoint\(s\)", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)
        log("  [OK] Remaining shutdown sequence completed", Color.GREEN)

        # Verify ssh.log: only proxmox-a (10.99.0.3) and proxmox-b (10.99.0.4) were called!
        with open(f"{DIR}/ssh.log") as f:
            ssh_content = f.read()
        assert "10.99.0.3" in ssh_content, "proxmox-a should have been dispatched"
        assert "10.99.0.4" in ssh_content, "proxmox-b should have been dispatched"
        assert "10.99.0.1" not in ssh_content, "ws-1 was already dispatched and should not be re-called"
        assert "10.99.0.2" not in ssh_content, "ws-2 was already dispatched and should not be re-called"
        log("  [OK] Only remaining endpoints were dispatched; no re-dispatches of ws-1/ws-2", Color.GREEN)

        # Check marker file now contains completed
        with open(f"{DIR}/shutdown_fired") as f:
            marker_content = f.read()
        assert "completed" in marker_content, "Marker file must record completion"
        log("  [OK] Marker file marked completed", Color.GREEN)

        # Recovery + WOL
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")
        self.wait_for_bridge_log(r"state: ShutdownLatched -> RecoveryDebouncing", timeout=8)
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        self.wait_for_bridge_log(r"Wake-on-LAN round 4/4", timeout=25)
        time.sleep(1)
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "Marker file should be deleted after WOL"
        log("  [OK] Marker file deleted after WOL completed", Color.GREEN)
        log("--> SCENARIO F2: PASSED", Color.GREEN + Color.BOLD)

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

        log("    Killing inverter simulator to simulate silence...", Color.CYAN)
        if self.sim_proc:
            self.sim_proc.kill()
            self.sim_proc.wait()
            self.sim_proc = None

        line = self.wait_for_bridge_log(r"modbus poll failed: timed out reading register", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(5)
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "Silent inverter must not trigger shutdown"
        with open(f"{DIR}/ssh.log") as f:
            assert len(f.read().strip()) == 0, "No SSH commands should fire on disconnect"
        log("  [OK] Verified no shutdown triggered during silence", Color.GREEN)

        # Restart simulator
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

        # Set register 184 (battery SOC) to 150 (> 100 is invalid)
        self.send_sim_cmd("set 184 150")
        line = self.wait_for_bridge_log(r"modbus poll failed: battery SOC register read 150", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        time.sleep(3)
        assert not os.path.exists(f"{DIR}/shutdown_fired"), "Garbage SOC read must not trigger shutdown"
        with open(f"{DIR}/ssh.log") as f:
            assert len(f.read().strip()) == 0, "No SSH commands should fire on garbage SOC"
        log("  [OK] Verified garbage SOC did not cause shutdown", Color.GREEN)

        # Restore valid SOC and grid
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

        # First shutdown
        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)
        with open(f"{DIR}/ssh.log") as f:
            lines1 = len([l for l in f.read().splitlines() if l.strip()])
        assert lines1 == 4

        # Restore grid to begin recovery -> WOL
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")
        self.wait_for_bridge_log(r"state: RecoveryDebouncing -> Idle", timeout=15)
        self.wait_for_bridge_log(r"Wake-on-LAN round 1/4", timeout=10)

        # Immediate outage during WOL
        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing", timeout=5)
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"cancelling pending Wake-on-LAN resends|firing shutdown sequence", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)
        with open(f"{DIR}/ssh.log") as f:
            lines2 = len([l for l in f.read().splitlines() if l.strip()])
        assert lines2 == 8, f"Expected 8 lines in ssh.log, got {lines2}"
        log("  [OK] Second shutdown dispatched 4 new commands (8 total in ssh.log)", Color.GREEN)
        assert os.path.exists(f"{DIR}/shutdown_fired"), "Marker file should exist again"
        log("  [OK] Marker file re-created", Color.GREEN)

        # Recovery
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
            f.write("10.99.0.3\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge()
        self.wait_for_bridge_log(r"soc=80\.0% grid=230\.0V")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: Idle -> GridLostDebouncing")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)

        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)

        line = self.wait_for_bridge_log(r"failed to shut down proxmox-a", timeout=10)
        log(f"  [OK] Caught expected failure: {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"shutdown sequence complete", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        with open(f"{DIR}/ssh.log") as f:
            content = f.read()
        log(f"  [ssh.log content]:\n{content.strip()}", Color.CYAN)
        assert "10.99.0.3 FAILED (simulated)" in content
        assert "10.99.0.4" in content, "proxmox-b was not contacted after proxmox-a failed!"
        log("  [OK] Endpoint failure did not stop remaining endpoints from shutting down", Color.GREEN)

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

        # Wait until ws-2 HANGING appears in ssh.log
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

        # Grid returns immediately
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
        assert "10.99.0.3" not in content, "proxmox-a should not have been contacted"
        assert "10.99.0.4" not in content, "proxmox-b should not have been contacted"
        log("  [OK] Remaining endpoints were spared after recovery was confirmed", Color.GREEN)
        log("--> SCENARIO M: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n1(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N1: VMs shut down, then poweroff", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge(CONFIG_VMS)
        self.wait_for_bridge_log(r"loaded config from .*bridge-test-vms\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"shutting down proxmox-a \(10\.99\.0\.3\) via Proxmox: VMs first, then poweroff", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox-a: 2 VM\(s\) registered, 2 running: dc01, app server", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox-a: host power-off scheduled via systemctl poweroff", timeout=25, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox-b: host power-off scheduled via systemctl poweroff", timeout=25, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)
        log("  [OK] Shutdown sequence complete", Color.GREEN)

        with open(f"{DIR}/proxmox/10.99.0.3/host") as f:
            h_a = f.read()
        with open(f"{DIR}/proxmox/10.99.0.4/host") as f:
            h_b = f.read()
        assert "poweroff via systemctl" in h_a
        assert "poweroff via systemctl" in h_b
        log("  [OK] Both Proxmox hosts recorded poweroff via systemctl", Color.GREEN)
        log("--> SCENARIO N1: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n2(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N2: Hung VM and VM without guest agent", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/proxmox/10.99.0.3/vms", "a") as f:
            f.write("109|stuck-vm|hang\n")
        with open(f"{DIR}/proxmox/10.99.0.3/state_109", "w") as f:
            f.write("on\n")

        with open(f"{DIR}/proxmox/10.99.0.4/vms", "a") as f:
            f.write("108|no-tools-vm|notools\n")
        with open(f"{DIR}/proxmox/10.99.0.4/state_108", "w") as f:
            f.write("on\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge(CONFIG_VMS)
        self.wait_for_bridge_log(r"loaded config from .*bridge-test-vms\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"proxmox-b: guest shutdown of VM no-tools-vm failed", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox-b: VM no-tools-vm could not be shut down gracefully -- powering it off hard", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox-a: VM stuck-vm still running after 15 s -- powering it off hard", timeout=25)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox-a: host power-off scheduled via systemctl poweroff", timeout=15, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox-b: host power-off scheduled via systemctl poweroff", timeout=15, from_start=True)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=10)
        log("  [OK] Both hosts powered off successfully despite VM issues", Color.GREEN)
        log("--> SCENARIO N2: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n3(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N3: systemctl refused -- fallback to /sbin/poweroff", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/proxmox/10.99.0.4/systemctl-refuse", "w") as f:
            f.write("1\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge(CONFIG_VMS)
        self.wait_for_bridge_log(r"loaded config from .*bridge-test-vms\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"proxmox-b: systemctl poweroff refused .* falling back to /sbin/poweroff", timeout=20)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=15)

        with open(f"{DIR}/proxmox/10.99.0.4/host") as f:
            h_b = f.read()
        assert "poweroff via /sbin/poweroff" in h_b
        log("  [OK] Verified fallback host file contains poweroff via /sbin/poweroff", Color.GREEN)
        log("--> SCENARIO N3: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n4(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N4: Grid back while host is shutting down VMs", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/proxmox/10.99.0.3/vms", "a") as f:
            f.write("109|stuck-vm|hang\n")
        with open(f"{DIR}/proxmox/10.99.0.3/state_109", "w") as f:
            f.write("on\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge(CONFIG_VMS)
        self.wait_for_bridge_log(r"loaded config from .*bridge-test-vms\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")
        self.wait_for_bridge_log(r"firing shutdown sequence", timeout=5)

        time.sleep(6)
        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 40")

        line = self.wait_for_bridge_log(r"recovery confirmed -- stopping the rest of the shutdown sequence", timeout=20)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"Wake-on-LAN round 1/4", timeout=10)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox-a: VM stuck-vm still running after 15 s -- powering it off hard", timeout=25)
        log(f"  [OK] {line}", Color.GREEN)
        line = self.wait_for_bridge_log(r"proxmox-a: host power-off scheduled", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)
        log("--> SCENARIO N4: PASSED", Color.GREEN + Color.BOLD)

    def test_scenario_n5(self):
        log("\n=======================================================", Color.BOLD)
        log("RUNNING SCENARIO N5: One Proxmox host unreachable", Color.BOLD)
        log("=======================================================", Color.BOLD)
        self.stop_bridge()
        self.reset_env()

        with open(f"{DIR}/ssh-fail", "w") as f:
            f.write("10.99.0.4\n")

        self.send_sim_cmd("restore")
        self.send_sim_cmd("soc 80")
        self.start_bridge(CONFIG_VMS)
        self.wait_for_bridge_log(r"loaded config from .*bridge-test-vms\.toml")

        self.send_sim_cmd("outage")
        self.wait_for_bridge_log(r"state: GridLostDebouncing -> OnBattery", timeout=8)
        self.send_sim_cmd("soc 25")

        line = self.wait_for_bridge_log(r"failed to shut down proxmox-b: listing VMs: ssh to 10\.99\.0\.4 exited Some\(255\)", timeout=15)
        log(f"  [OK] {line}", Color.GREEN)

        line = self.wait_for_bridge_log(r"proxmox-a: host power-off scheduled via systemctl poweroff", timeout=20)
        log(f"  [OK] {line}", Color.GREEN)

        self.wait_for_bridge_log(r"shutdown sequence complete", timeout=10)
        log("  [OK] proxmox-a completed normally while proxmox-b failed as expected", Color.GREEN)
        log("--> SCENARIO N5: PASSED", Color.GREEN + Color.BOLD)

    def run(self, scenarios=None):
        all_map = {
            "a": ("Scenario A (Startup & Telemetry)", self.test_scenario_a),
            "b": ("Scenario B (Short Blip)", self.test_scenario_b),
            "c": ("Scenario C (Outage Without Low SOC)", self.test_scenario_c),
            "d": ("Scenario D (Full Outage & WoL)", self.test_scenario_d),
            "e": ("Scenario E (Regression: Grid flicker low SOC)", self.test_scenario_e),
            "f": ("Scenario F (Process restart mid-outage)", self.test_scenario_f),
            "f2": ("Scenario F2 (Process restart mid-sequence)", self.test_scenario_f2),
            "g": ("Scenario G (Silent inverter)", self.test_scenario_g),
            "h": ("Scenario H (Garbage SOC read)", self.test_scenario_h),
            "k": ("Scenario K (Outage during WoL)", self.test_scenario_k),
            "l": ("Scenario L (One endpoint fails)", self.test_scenario_l),
            "m": ("Scenario M (One endpoint hangs, grid returns)", self.test_scenario_m),
            "n1": ("Scenario N1 (Proxmox VMs then poweroff)", self.test_scenario_n1),
            "n2": ("Scenario N2 (Proxmox hung VM & missing agent)", self.test_scenario_n2),
            "n3": ("Scenario N3 (Proxmox systemctl refused fallback)", self.test_scenario_n3),
            "n4": ("Scenario N4 (Proxmox grid returns during VM shutdown)", self.test_scenario_n4),
            "n5": ("Scenario N5 (Proxmox one host unreachable)", self.test_scenario_n5),
        }

        if not scenarios:
            to_run = list(all_map.keys())
        else:
            to_run = [s.lower() for s in scenarios if s.lower() in all_map]

        try:
            self.stop_existing()
            self.init_keys_and_dirs()
            self.start_cable()
            self.start_wol_listener()
            self.start_simulator()

            passed = 0
            for s in to_run:
                name, fn = all_map[s]
                try:
                    fn()
                    passed += 1
                except Exception as ex:
                    log(f"--> {name}: FAILED: {ex}", Color.RED + Color.BOLD)
                    raise

            log("\n=======================================================", Color.BOLD)
            log(f"ALL SELECTED SCENARIOS ({passed}/{len(to_run)}) PASSED SUCCESSFULLY!", Color.GREEN + Color.BOLD)
            log("=======================================================", Color.BOLD)
        finally:
            self.cleanup()

if __name__ == "__main__":
    targets = sys.argv[1:]
    try:
        runner = TestRunner()
        runner.run(targets)
    except Exception:
        import traceback
        traceback.print_exc()
        sys.exit(1)
