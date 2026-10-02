#!/usr/bin/env python3
"""Prints every Wake-on-LAN magic packet the bridge sends during a test run.

The test config points `wol_broadcast_addr` at 127.0.0.1:40009 instead of
the LAN, so nothing on the real network is woken; this listens there,
checks each packet is a valid magic packet and prints the MAC it targets.
A blank line separates rounds (packets more than 1 s apart).

    python3 wol_listen.py [port]      # default 40009, no root needed
"""

import socket
import sys
import time

port = int(sys.argv[1]) if len(sys.argv) > 1 else 40009
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.bind(("127.0.0.1", port))
print(f"listening for Wake-on-LAN on 127.0.0.1:{port} -- Ctrl+C to stop", flush=True)

last = 0.0
count = 0
try:
    while True:
        data, _ = sock.recvfrom(1024)
        now = time.monotonic()
        if last and now - last > 1.0:
            print(flush=True)
        last = now
        count += 1
        mac = data[6:12]
        valid = len(data) == 102 and data[:6] == b"\xff" * 6 and data[6:] == mac * 16
        mac_text = ":".join(f"{b:02X}" for b in mac)
        status = "ok" if valid else f"INVALID ({len(data)} bytes)"
        print(f"{time.strftime('%H:%M:%S')}  #{count:<3} WOL -> {mac_text}  {status}", flush=True)
except KeyboardInterrupt:
    print(f"\n{count} packet(s) received")
