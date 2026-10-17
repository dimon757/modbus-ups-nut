# Example Configurations

This directory provides ready-to-use configuration files for common site topologies:

| File | Topology | Endpoints |
|---|---|---|
| [`bridge-1proxmox-2win11.toml`](bridge-1proxmox-2win11.toml) | 1 Proxmox VE Host + 2 Windows 11 Workstations | `workstation-1`, `workstation-2`, `proxmox-1` |
| [`bridge-2proxmox-2win11.toml`](bridge-2proxmox-2win11.toml) | 2 Proxmox VE Hosts + 2 Windows 11 Workstations | `workstation-1`, `workstation-2`, `proxmox-a`, `proxmox-b` |
| [`bridge-1win11.toml`](bridge-1win11.toml) | Single Windows 11 Workstation | `workstation-1` (no Proxmox) |

## Quick Start

1. Choose the template matching your site topology.
2. Copy it to `/etc/modbus-ups-bridge/bridge.toml` with restricted permissions:
   ```bash
   sudo install -d -m 700 /etc/modbus-ups-bridge
   sudo install -m 600 docs/example-configs/bridge-1proxmox-2win11.toml /etc/modbus-ups-bridge/bridge.toml
   ```
3. Edit the file to fill in your site-specific values:
   - `wol_broadcast_addr`: subnet broadcast on your endpoints network interface (e.g., `192.168.1.255:9`).
   - `[modbus] device`: RS485 serial port (e.g., `/dev/ttyS0`).
   - `[thresholds] low_battery_soc` and `inverter_cutoff_soc`: match your inverter settings.
   - `[[endpoints]]`: IP addresses, MAC addresses, and SSH key paths.
4. Validate the configuration:
   ```bash
   /usr/local/bin/modbus-ups-bridge --config /etc/modbus-ups-bridge/bridge.toml
   ```
