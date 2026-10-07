# Sunsynk Modbus protocol versions

The bridge's register map (`src/modbus.rs`) was built from the Sunsynk/Deye
"Modbus RTU Protocol" document V117. This page records how it compares with
the other versions supplied, and what would have to change for each. All
three files are kept in [`docs/protocol/`](protocol/).

Compared on 2026-09-27: every register from 150 to 230 was checked line by
line, plus all the registers the bridge reads.

## Which document is which

| Copy in this repository | Original file name | Real version | Notes |
|---|---|---|---|
| [`protocol/Sunsynk-Modbus-Protocol-V117.pdf`](protocol/Sunsynk-Modbus-Protocol-V117.pdf) | *1.10 and 2.10 - UPS inverter Sunsynk Modbus registers.pdf* (project documentation, 8196 UZ Namangan SCADA) | **V117** (2021-04-08) | **The bridge is built on this one.** Its revision history ends at V117; "V115" is only an earlier entry in that history, not the document's version. |
| [`protocol/Sunsynk-Modbus-Protocol-V118-file-same-as-V117.pdf`](protocol/Sunsynk-Modbus-Protocol-V118-file-same-as-V117.pdf) | *Modbus protocol V118.pdf* | **V117** -- same document | Text identical to the V117 file, word for word; revision history also ends at V117 (no V118 entry). Only the PDF file differs (re-saved). Kept because it was supplied as "V118". |
| [`protocol/Sunsynk-Modbus-Protocol-V119.pdf`](protocol/Sunsynk-Modbus-Protocol-V119.pdf) | *Modbus Protocol 119.pdf* | **V119** (2024-02-19) | A real newer version. Its revision history lists changes to registers 28, 34 and 46 only -- but see register 54 below, which changed without being listed. |

## Registers the bridge uses

| Reg | Used for | V117 (project) = "V118" | V119 | Change needed for V119 |
|---|---|---|---|---|
| 0 | Device type check | 0x0300 = single-phase storage | 0x0300 = single-phase **low-voltage** storage; new 0x0600 = three-phase high-voltage | **None.** The 5 kW Sunsynk with a 48 V battery is low-voltage: still 0x0300 |
| 1 | Modbus address | 1-247 | same | None |
| 2 | Protocol version (**logged only**) | e.g. 0x0102 = 1.2 | same | None |
| 150 | **Grid voltage -- one of the two grid-lost signals** | 0.1 V | same | None |
| 194 | **Grid side relay status -- the other grid-lost signal** (optional: voltage alone if unreadable) | 0 open / 1 closed | same | None |
| 184 | **Battery SOC -- the shutdown trigger** | 1 %, 0-100 | same | None |
| 213 | Battery-mode check | 0 voltage / 1 capacity / 2 none | same | None |
| 217 | Inverter's own SOC cutoff check | 1 % | same | None |
| 220 | Inverter's own voltage cutoff | 0.01 V | same | None |
| 178 | Load power (**logged only**) | 1 W, signed | 1 W -- or **10 W** depending on register 54 | Optional (see below) |
| 190 | Battery power (**logged only**) | 1 W, signed | same entry, but listed under register 54 | Optional (see below) |
| 54 | Protocol hint (**logged only**) | "EEPROM initial enabled" -- a command register | **"AC power ratio"**: whether registers 167-172, 176-179, 185 and 190 are in 1 W or 10 W units | Optional (see below) |

## Conclusion

**The bridge works the same with V117 ("V118") and V119.** Everything that
decides shutdown and wake-up -- grid voltage (150), SOC (184), device type
(0) and the inverter's own cutoff settings (213, 217, 220) -- has the same
address, unit and meaning in both.

The only possible difference is in the **log**: on V119 firmware, load power
(`load=`) and battery power (`batt=`) may be reported in 10 W units, so the
log would show a tenth of the real value. No decision uses them.

## Register 54 in detail

- **V117:** "EEPROM initial enabled" -- *writing* 1 or 2 re-initialises the
  EEPROM. The bridge only ever reads (function 0x03) and never writes, so
  reading it is harmless.
- **V119:** "AC power ratio", read-only, range [0,2]. The document
  contradicts itself on which value means what: the register-54 cell says
  "10: ... 10 W unit, 1: ... 1 W unit", while the notes on the individual
  power registers say "10 W when register 54 = 1". Only the real unit can
  settle it.

## What the bridge does about it

At every connect it logs, next to the device-type line:

```
inverter: protocol version (reg 2) 0x0102 (1.2), reg 54 0, grid relay (reg 194) 1 -- see docs/protocol-versions.md
```

Any of the three shows `unreadable` if the firmware doesn't answer for it;
the cutoff checks carry on regardless. An unreadable register 194 also logs
a warning, because grid-loss detection then rests on the voltage alone.

**On delivery day:** note both values here, and compare `load=` in the
debug log (`RUST_LOG=debug`) with the load on the inverter's display. If
the log is 10x too small, the power registers are in 10 W units: multiply
`load_power_w` and `battery_power_w` by 10 in `ModbusClient::poll`
(`src/modbus.rs`) -- a one-line change, logging only.

| Delivery-day result | Value |
|---|---|
| Register 2 (protocol version) | |
| Register 54 | |
| Register 194 with grid on / grid breaker off | |
| `load=` in log vs. inverter display | |
