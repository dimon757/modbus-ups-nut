#!/usr/bin/env python3
"""Sunsynk inverter simulator for level-2 testing of modbus-ups-bridge.

Answers Modbus RTU requests (slave id 1) on a serial port -- normally one end
of the socat virtual cable made by setup.sh -- with the same registers the
bridge reads, and lets you change them from the keyboard while it runs.

    inverter_sim.py --serial /tmp/mub-test/ttyINV     # what setup.sh prints
    inverter_sim.py --tcp 5020                        # RTU framing over TCP,
                                                      # for self_check.py

Type `help` at the prompt for the commands.
"""

import argparse
import asyncio
import sys

from pymodbus import FramerType
from pymodbus.server import StartAsyncSerialServer, StartAsyncTcpServer
from pymodbus.simulator import DataType, SimData, SimDevice

SLAVE_ID = 1
REGISTER_COUNT = 300

# Register -> (starting value, description). Addresses and units from the
# Sunsynk/Deye "Modbus RTU Protocol" document (V117). Deliberately a separate
# copy of the map in src/modbus.rs: the simulator plays the inverter on its
# own, so a wrong address in the bridge shows up as a failed test instead of
# being copied along.
REGISTERS = {
    0: (0x0300, "device type (0x0300 = single-phase storage)"),
    2: (0x0102, "protocol version (0x0102 = 1.2, the document's example)"),
    54: (0, "V119: AC power ratio (1 W / 10 W units); V117: EEPROM init"),
    150: (2300, "grid voltage L1-N, 0.1 V"),
    178: (500, "load total power, W, signed"),
    184: (80, "battery SOC, %"),
    190: (0, "battery power, W, signed (sign convention unverified)"),
    213: (1, "battery mode: 0 voltage / 1 capacity / 2 none"),
    217: (20, "battery capacity ShutDown (cutoff), %"),
    220: (4600, "battery voltage ShutDown, 0.01 V"),
}

HELP = """  grid <volts>     grid voltage, e.g. `grid 0` (outage), `grid 230`
  soc <percent>    battery SOC, e.g. `soc 25`
  load <watts>     load power
  batt <watts>     battery power (Deye convention: + discharging, - charging)
  outage           same as `grid 0`
  restore          same as `grid 230`
  set <reg> <val>  any register; <val> may be hex (0x0300) or negative
  show             current values
  help             this text
  quit             stop the simulator (Ctrl+C / Ctrl+D also work)
"""


class Inverter:
    """The simulated register map. `regs` is the single source of truth: the
    console edits it, and every Modbus request is answered from it."""

    def __init__(self):
        self.regs = [0] * REGISTER_COUNT
        for addr, (value, _) in REGISTERS.items():
            self.regs[addr] = value

    def write(self, addr, value):
        self.regs[addr] = value & 0xFFFF

    def read(self, addr):
        return self.regs[addr]

    async def on_request(self, _func_code, start_address, address, count, registers, _values):
        # Called by pymodbus before it answers a request: copy the current
        # values into its register array. Returning None lets it proceed.
        offset = address - start_address
        registers[offset : offset + count] = self.regs[address : address + count]
        return None

    def device(self):
        return SimDevice(
            id=SLAVE_ID,
            simdata=[
                SimData(
                    address=0,
                    count=REGISTER_COUNT,
                    values=0,
                    datatype=DataType.REGISTERS,
                )
            ],
            action=self.on_request,
        )


def show(inv):
    for addr, (_, desc) in REGISTERS.items():
        raw = inv.read(addr)
        signed = raw - 0x10000 if raw >= 0x8000 else raw
        extra = f" ({signed})" if signed != raw else ""
        print(f"  {addr:>3} = {raw:>5} {raw:#06x}{extra:<8}  {desc}")


def parse_int(text):
    return int(text, 0)


def handle(inv, line):
    """Apply one command. Returns False to quit."""
    parts = line.split()
    if not parts:
        return True
    cmd, args = parts[0].lower(), parts[1:]
    try:
        if cmd in ("quit", "exit"):
            return False
        elif cmd == "help":
            print(HELP, end="")
        elif cmd == "show":
            show(inv)
        elif cmd == "outage":
            inv.write(150, 0)
            print("  grid 0.0 V")
        elif cmd == "restore":
            inv.write(150, 2300)
            print("  grid 230.0 V")
        elif cmd == "grid" and len(args) == 1:
            volts = float(args[0])
            inv.write(150, round(volts * 10))
            print(f"  grid {volts:.1f} V")
        elif cmd == "soc" and len(args) == 1:
            inv.write(184, parse_int(args[0]))
            print(f"  SOC {parse_int(args[0])} %")
        elif cmd == "load" and len(args) == 1:
            inv.write(178, parse_int(args[0]))
            print(f"  load {parse_int(args[0])} W")
        elif cmd == "batt" and len(args) == 1:
            inv.write(190, parse_int(args[0]))
            print(f"  battery {parse_int(args[0])} W")
        elif cmd == "set" and len(args) == 2:
            addr, value = parse_int(args[0]), parse_int(args[1])
            if not 0 <= addr < REGISTER_COUNT:
                raise ValueError("register must be 0-299")
            inv.write(addr, value)
            print(f"  register {addr} = {value & 0xFFFF}")
        else:
            print(f"  unknown command {line.strip()!r} -- type `help`")
    except ValueError as e:
        print(f"  bad value: {e}")
    return True


async def console(inv):
    loop = asyncio.get_running_loop()
    print("inverter simulator ready -- type `help` for commands")
    show(inv)
    while True:
        print("sim> ", end="", flush=True)
        line = await loop.run_in_executor(None, sys.stdin.readline)
        if not line or not handle(inv, line):
            return


async def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    where = ap.add_mutually_exclusive_group(required=True)
    where.add_argument("--serial", help="serial device, e.g. /tmp/mub-test/ttyINV")
    where.add_argument("--tcp", type=int, metavar="PORT", help="RTU over TCP on 127.0.0.1:PORT")
    ap.add_argument("--no-console", action="store_true", help="serve only, no prompt")
    args = ap.parse_args()

    inv = Inverter()
    if args.serial:
        server = StartAsyncSerialServer(
            inv.device(), framer=FramerType.RTU, port=args.serial, baudrate=9600,
            bytesize=8, parity="N", stopbits=1,
        )
        print(f"serving slave {SLAVE_ID} on {args.serial} (9600 8N1)")
    else:
        server = StartAsyncTcpServer(inv.device(), framer=FramerType.RTU, address=("127.0.0.1", args.tcp))
        print(f"serving slave {SLAVE_ID} on 127.0.0.1:{args.tcp} (RTU framing)")

    server_task = asyncio.create_task(server)
    if args.no_console:
        await server_task
    else:
        await console(inv)
        server_task.cancel()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
