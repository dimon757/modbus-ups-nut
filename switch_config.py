#!/usr/bin/env python3
"""Switch bridge.toml configuration between 1-Windows-11 and 3-endpoints (2 Windows 11 + Proxmox)."""
import os
import shutil
import sys

REPO_DIR = os.path.dirname(os.path.abspath(__file__))
TEST_DIR = os.path.join(REPO_DIR, "test")

def show_status():
    p = os.path.join(TEST_DIR, "bridge.toml")
    if not os.path.exists(p):
        print(f"File not found: {p}")
        return
    print(f"Current active configuration in {p}:")
    with open(p, "r", encoding="utf-8") as f:
        for line in f:
            line_str = line.strip()
            if any(line_str.startswith(k) for k in ("name =", "kind =", "method =")):
                print(f"  {line_str}")

def switch_to(mode):
    if mode in ("1win", "1", "single"):
        src_test = os.path.join(TEST_DIR, "bridge-1win.toml")
        src_root = os.path.join(REPO_DIR, "bridge-1win.toml")
        shutil.copyfile(src_test, os.path.join(TEST_DIR, "bridge.toml"))
        if os.path.exists(src_root):
            shutil.copyfile(src_root, os.path.join(REPO_DIR, "bridge.toml"))
        print("Switched configuration to: 1 Windows 11 endpoint (no Proxmox)")
    elif mode in ("3ep", "3", "default", "revert"):
        src_test = os.path.join(TEST_DIR, "bridge-3endpoints.toml")
        src_root = os.path.join(REPO_DIR, "bridge-3endpoints.toml")
        shutil.copyfile(src_test, os.path.join(TEST_DIR, "bridge.toml"))
        if os.path.exists(src_root):
            shutil.copyfile(src_root, os.path.join(REPO_DIR, "bridge.toml"))
        print("Switched configuration to: 2 Windows 11 endpoints + 1 Proxmox (vms_then_poweroff)")
    elif mode == "status":
        show_status()
    else:
        print("Usage: python switch_config.py [1win | 3ep | status]")
        sys.exit(1)

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python switch_config.py [1win | 3ep | status]")
        show_status()
        sys.exit(0)
    switch_to(sys.argv[1].lower())
