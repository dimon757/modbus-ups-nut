#!/usr/bin/env python3
"""Checks inverter_sim.py itself, without the bridge or socat.

Starts the simulator's register map on a local TCP port (RTU framing), reads
every register back with a real Modbus client exactly as the bridge would
(function 0x03, slave 1, the document's addresses), then changes values
through the simulator's own commands and reads them again. Catches the
classic off-by-one-register mistake before it can confuse a test run.

    python3 self_check.py      # prints PASS / FAIL, exit code 0 / 1
"""

import asyncio
import sys

from pymodbus import FramerType
from pymodbus.client import AsyncModbusTcpClient
from pymodbus.server import StartAsyncTcpServer

import inverter_sim as sim

PORT = 50200


async def read(client, addr):
    rr = await client.read_holding_registers(addr, count=1, device_id=sim.SLAVE_ID)
    if rr.isError():
        raise RuntimeError(f"register {addr}: {rr}")
    return rr.registers[0]


async def main():
    inv = sim.Inverter()
    server = asyncio.create_task(
        StartAsyncTcpServer(inv.device(), framer=FramerType.RTU, address=("127.0.0.1", PORT))
    )
    await asyncio.sleep(0.5)

    client = AsyncModbusTcpClient("127.0.0.1", port=PORT, framer=FramerType.RTU)
    await client.connect()
    failures = []

    def check(what, got, want):
        ok = got == want
        print(f"  {'ok  ' if ok else 'FAIL'} {what}: got {got}, want {want}")
        if not ok:
            failures.append(what)

    print("starting values:")
    for addr, (want, desc) in sim.REGISTERS.items():
        check(f"reg {addr} ({desc})", await read(client, addr), want)
    # Neighbours must stay 0, or the map is shifted by one.
    for addr in (149, 151, 183, 185):
        check(f"reg {addr} (unused neighbour)", await read(client, addr), 0)

    print("after simulator commands:")
    for line, addr, want in [
        ("outage", 150, 0),
        ("grid 229.5", 150, 2295),
        ("soc 25", 184, 25),
        ("batt -1200", 190, 0x10000 - 1200),
        ("set 217 30", 217, 30),
        ("set 0 0x0500", 0, 0x0500),
    ]:
        sim.handle(inv, line)
        check(f"`{line}` -> reg {addr}", await read(client, addr), want)

    client.close()
    server.cancel()
    print("PASS" if not failures else f"FAIL ({len(failures)})")
    return not failures


if __name__ == "__main__":
    sys.exit(0 if asyncio.run(main()) else 1)
