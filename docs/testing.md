# Testing the emulator

## Unit tests

```
cargo test
```

Nine tests: the Z180 CPU core, the software modem (Bell 103 and Bell 212A),
the MSM6242 clock, the SIP audio codecs and the PNM rate file reader. The rate
file tests use the first `.R94` in `rates/` and are skipped without one.

## Scenario tests

The test modes run the real firmware headless and print what it does. They
use the dumps in `rom/` and `voice/` (and the rate file in `rates/` for
the rate tests). `ELCOTEL_ROM=<file name or path>` picks a program ROM.

| Command | What it checks |
|---|---|
| `--selftest` | a full paid call: lift, dial, pay, and the firmware places it to a local SIP test peer |
| `--ratetest` | loads the rate file over the emulated modem the way PNM Plus does, then places calls and reports the price, prompts and dialed digits |
| `--presettest` | a ROM with built-in rates (PCM 05.09.00) installs them |
| `--telemtest` | telemetry entry with the phone's bypass code at several typing speeds |
| `--incomingtest` | an incoming SIP call rings the phone and is answered |
| `--modemtest` | a modem dials in over SIP at 300 or 1200 bit/s and logs on |
| `--coverage [report.txt]` | runs all the scenarios plus every remote command byte, then reports code, I/O and memory coverage |
| `--trace <secs> [actions]` | a scripted run printing I/O changes, events and `PRINT` output |

`--trace` actions are `[@seconds:]name=value`: `hook=0` / `hook=1`,
`keys=5551234`, `coin=n` / `d` / `q`, `prog=1`, `ring=1`, `in60=04` (force an
input port), plus `pc=63A3` to print the registers at an address. Example:

```
elcotel-emu --trace 20 @1:hook=0 @2:keys=5551234 @6:coin=q @15:hook=1
```

## Coverage reached

From `--coverage` with the scenarios and the remote-command sweep:

| Firmware | Instructions run | Share of the code |
|---|---|---|
| PCM 05.04.04 (stock) | 9,526 of 25,970 | 36.7% |
| PCM 05.09.00 (custom) | 9,210 of 18,870 | 48.8% |

All 28 I/O ports the firmware uses have a meaning that fits the code (see
[hardware.md](hardware.md)). Most of the code that has not run is reached
only from remote commands with specific data, diagnostics, or hardware
faults the emulator does not produce. The Coverage tab shows the same figures
live, with the largest regions not yet run and why.
