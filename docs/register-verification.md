# Verifying the Sunsynk Modbus registers on site

The bridge's register addresses come from the protocol document (see
`src/modbus.rs` and [protocol-versions.md](protocol-versions.md)). A
document is not the inverter: before trusting the bridge, read each register
**directly from the real inverter** with an independent tool and compare it
with what the inverter's own display shows. This takes about 30 minutes.

If every row of the sheet at the end matches, the addresses are right. If a
value is wrong, the section "If a register is wrong" says what to change.

## Safety first

- **Read only.** Every command here reads (Modbus function 0x03). `mbpoll`
  **writes** if you put numbers after the device name -- never do that. Some
  registers are commands (on V117 firmware, writing register 54 re-initialises
  the inverter's EEPROM).
- **Stop the bridge** so only one program uses the serial port:
  `sudo systemctl stop modbus-ups-bridge`. This also disarms the watchdog
  cleanly, so the box won't reboot while you work. Start it again at the end.
- The test in step 6 switches the inverter's **grid breaker** off. The
  inverter then runs the load from the battery; don't do it with a nearly
  empty battery, and warn anyone using the four machines.

## What you need

- The bridge box (N2840, Debian 13) connected to the inverter's RS485
  Modbus port, as in the README ("Target platform").
- The inverter's display or app, to compare values.
- The tool `mbpoll`: `sudo apt install mbpoll` (Debian 13 has version 1.5.2).
- This document's sheet (end of page) to write the results on.

Find out first:
- **Which serial port** the inverter is wired to: `dmesg | grep ttyS` --
  usually `/dev/ttyS0` or `/dev/ttyS1`. The commands below use `/dev/ttyS0`;
  replace it if yours differs.
- **The inverter's Modbus address** (slave id) from its communication
  settings -- normally `1`. The commands use `-a 1`.

## The command, explained

Every read in this document has this form:

```bash
mbpoll -m rtu -a 1 -b 9600 -P none -t 4 -0 -1 -r 184 /dev/ttyS0
```

| Part | Meaning | Why it matters |
|---|---|---|
| `-m rtu` | Modbus RTU over a serial line | |
| `-a 1` | Inverter's Modbus address (slave id) | Wrong value → no answer |
| `-b 9600` | 9600 baud | `mbpoll` defaults to 19200 → no answer |
| `-P none` | No parity (8N1) | `mbpoll` defaults to **even** parity → no answer or garbage |
| `-t 4` | Holding registers, function 0x03 | Use `-t 4:int16` for signed values, `-t 4:hex` for hex |
| `-0` | **Count registers from 0**, as the protocol document does | **Without it, "184" reads register 183** -- the classic off-by-one mistake |
| `-1` | Read once and stop | Without it, it keeps polling every second (stop with Ctrl+C) |
| `-r 184` | Register(s) to read; a list works too: `-r 150,184,190` | |
| `-c 3` | (optional) Read 3 registers starting at `-r` | |

The answer looks like this -- the number in brackets is the register, then
its raw value:

```
-- Polling slave 1...
[184]: 	80
```

## Step 1 -- Is anyone answering? (registers 0-2)

```bash
mbpoll -m rtu -a 1 -b 9600 -P none -t 4 -0 -1 -r 0 -c 3 /dev/ttyS0
```

| Register | Expected | Meaning |
|---|---|---|
| `[0]` | **768** (= 0x0300) | Device type: single-phase low-voltage storage inverter |
| `[1]` | **1** (your slave id) | The inverter's own Modbus address |
| `[2]` | e.g. 258 (= 0x0102) | Protocol version -- write it on the sheet |

**768 at `[0]` together with your slave id at `[1]` proves** that the port,
wiring, baud rate, parity, slave id *and* the register numbering are all
right. If this step fails, go to Troubleshooting -- the rest can't work yet.

## Step 2 -- Battery SOC (register 184) and the shift test

Read 184 together with its neighbours:

```bash
mbpoll -m rtu -a 1 -b 9600 -P none -t 4 -0 -1 -r 183 -c 3 /dev/ttyS0
```

| Register | Meaning | Compare with the display | Example |
|---|---|---|---|
| `[183]` | Battery voltage, 0.01 V | Battery voltage | 5120 → 51.20 V |
| `[184]` | **Battery SOC, %** | **Battery SOC** | 80 → 80 % |
| `[185]` | Battery status | -- | |

This is also the **shift test**: 184 must be the SOC (0-100) and 183 the
battery voltage (thousands). If the SOC shows up at `[185]`, or `[184]`
looks like a voltage, every address is off by one.

## Step 3 -- Grid voltage (register 150)

```bash
mbpoll -m rtu -a 1 -b 9600 -P none -t 4 -0 -1 -r 150 /dev/ttyS0
```

Divide by 10: `2301` → 230.1 V. It must match the grid voltage on the display
(within a volt or two). This is the register that starts the whole shutdown
path; step 6 checks it without grid.

## Step 4 -- Power (registers 178 and 190)

These are signed, so use `int16`:

```bash
mbpoll -m rtu -a 1 -b 9600 -P none -t 4:int16 -0 -1 -r 178,190 /dev/ttyS0
```

| Register | Meaning | Compare with the display |
|---|---|---|
| `[178]` | Load power, W | Load power |
| `[190]` | Battery power, W | Battery power; note the **sign** while charging vs discharging |

If `[178]` is about **ten times smaller** than the display, this firmware
reports power in 10 W units (protocol V119, register 54 -- see
[protocol-versions.md](protocol-versions.md)). Both values are only logged
by the bridge, never used for decisions.

## Step 5 -- The inverter's own battery settings (213, 217, 220)

```bash
mbpoll -m rtu -a 1 -b 9600 -P none -t 4 -0 -1 -r 213,217,220,54 /dev/ttyS0
```

| Register | Meaning | Compare with the inverter's battery settings |
|---|---|---|
| `[213]` | How the battery is managed: 0 = by voltage, 1 = by capacity (%), 2 = no battery | The battery-mode choice (capacity / voltage / no battery). **Should be 1**, see README "What you must verify" |
| `[217]` | Shutdown SOC, % | The battery **Shutdown %** setting -- must equal `inverter_cutoff_soc` in `bridge.toml` |
| `[220]` | Shutdown voltage, 0.01 V | The battery **Shutdown V** setting: 4600 → 46.00 V |
| `[54]` | V119: power units; V117: EEPROM flag | Nothing -- write it on the sheet |

A good confirmation that 217 is really the shutdown setting: change the
Shutdown % on the inverter by one step, read `[217]` again, then set it back.

## Step 6 -- Grid lost (register 150 again)

Only this proves the bridge will notice an outage. Switch the inverter's
**grid breaker off**, wait 10 s, read register 150 again:

```bash
mbpoll -m rtu -a 1 -b 9600 -P none -t 4 -0 -1 -r 150 /dev/ttyS0
```

It must be **below 1000** (= 100.0 V, the bridge's `grid_lost_voltage`),
normally close to 0. Switch the grid breaker back on and check it returns to
~2300.

## Step 7 -- Cross-check with the bridge

Start the bridge in the foreground with debug logging, so each poll is
printed:

```bash
sudo RUST_LOG=debug /usr/local/bin/modbus-ups-bridge /etc/modbus-ups-bridge/bridge.toml
```

This is the real bridge with the real config. With the grid on, it only
watches; stop it with **Ctrl+C** (which also disarms the watchdog cleanly).
Expect:

```
inverter: device type 0x0300, battery mode 1, cutoff 20% / 46.00 V
inverter: protocol version (reg 2) 0x0102 (1.2), reg 54 0 -- see docs/protocol-versions.md
inverter settings: inverter cutoff 20% SOC, shutdown sequence at 30% -- 10 points of margin
soc=80.0% grid=230.1V load=500W batt=-300W on_battery=false low_battery=false
```

`soc`, `grid`, `load` and `batt` must agree with what `mbpoll` read in
steps 2-4. Then start the service again:

```bash
sudo systemctl start modbus-ups-bridge
```

## Troubleshooting

| What you see | Likely cause | What to do |
|---|---|---|
| Nothing after `-- Polling slave 1...`, or a timeout, for every register | No communication | Check in this order: the right `/dev/ttyS*`; the port set to **RS485** in the BIOS (not RS232/RS422); A and B swapped (swap them -- the classic fix); `-b 9600 -P none` typed; the slave id |
| Still nothing, wiring certainly right | The board doesn't switch the RS485 transmit direction by itself | Add `-R` (or try `-F`) to the command. If that helps, the bridge needs the same -- tell the developer; see the board's manual |
| Timeouts only with a different `-a` | Wrong slave id | Use the address shown in the inverter's communication settings |
| `[0]` isn't 768 | Not the Sunsynk storage inverter on that port, or a different model | Check which device is wired to the port |
| SOC appears at `[185]` instead of `[184]`, or values look shifted | Off by one | Make sure `-0` is in the command. If it is, the firmware counts differently -- see below |
| Values match the display only roughly | Normal for power (it changes every second) | Compare over a few reads |
| An error mentioning "Illegal data address" | That register doesn't exist on this firmware | Note it; see below |

## If a register is wrong

The register map is defined in one place: the constants at the top of
`src/modbus.rs`. To change an address or a scale:

1. Edit the constant in `src/modbus.rs`.
2. Update the same register in the test simulator, `test/inverter_sim.py`
   (it keeps its own copy on purpose).
3. Run the tests -- `cargo test`, then the level-2 scenarios (see
   [TESTING.md](../TESTING.md)).
4. Rebuild (`cargo build --release`), copy the binary to `/usr/local/bin/`,
   `sudo systemctl restart modbus-ups-bridge`, and repeat step 7.

## Result sheet

Date: ______________ Inverter serial no.: ______________ Firmware: ______________

| Step | Register | Expected | Read with mbpoll | Inverter display | OK? |
|---|---|---|---|---|---|
| 1 | 0 device type | 768 (0x0300) | | -- | |
| 1 | 1 Modbus address | slave id (1) | | | |
| 1 | 2 protocol version | e.g. 258 | | -- | |
| 2 | 183 battery voltage | = display ×100 | | | |
| 2 | 184 SOC | = display | | | |
| 3 | 150 grid voltage | = display ×10 | | | |
| 4 | 178 load power | = display (or /10) | | | |
| 4 | 190 battery power | = display; sign when charging: ____ | | | |
| 5 | 213 battery mode | 1 | | | |
| 5 | 217 shutdown SOC | = display, = `inverter_cutoff_soc` | | | |
| 5 | 220 shutdown voltage | = display ×100 | | | |
| 5 | 54 | -- (record only) | | -- | |
| 6 | 150 with grid breaker off | < 1000 | | | |
| 7 | bridge log agrees with mbpoll | yes | | -- | |

Copy the values of registers 2 and 54 into the table at the end of
[protocol-versions.md](protocol-versions.md).
