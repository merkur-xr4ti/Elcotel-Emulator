# Elcotel Series 5 board: hardware reference

There is no schematic. Everything here was worked out from how the firmware
(PCM 05.04.04 and 05.09.00) uses the hardware, then checked against PNM Plus
and Elcotel Manager sessions and by running the firmware in the emulator. The
board straps and aux relays are the least certain parts.

## Processor and clock

Zilog Z180 (Hitachi HD64180) at 6.144 MHz. The on-chip peripherals are used
as follows:

| Part | Use |
|---|---|
| MMU | logical E000-FFFF and B000-DFFF are mapped onto the battery RAM; B000-DFFF is switched to the voice ROM while a prompt's index is read |
| PRT1 | TOUT pacing for the voice codec (one CVSD bit per 4 TOUT edges, 19.2 kbit/s) |
| DMA1 | streams voice ROM bytes to port 40 (the CVSD codec) |
| DMA0 | memory-to-memory copies |
| ASCI0 | serial link to the 73K212 modem (remote programming) |
| INT1 | MSM6242 clock interrupt: the firmware's 64 Hz system tick while awake |

## Memory map (physical)

| Range | What |
|---|---|
| 00000-0FFFF | program ROM, 27C512 (64 KB) |
| 10000-1FFFF | voice ROM window, 27C010 (128 KB); port A3 bit 0 picks the 64 KB half |
| 20000-27FFF | low RAM / upgrade area |
| 28000-29FFF | EEPROM, 8 KB (written only while port A4 bit 0 is 1) |
| 30000-3FFFF | battery-backed RAM, 64 KB |

The firmware sets CBAR = EB, CBR = 30, BBR = 25, so logical B000-DFFF is
physical 30000-32FFF and E000-FFFF is 3E000-3FFFF.

### Options PNM Plus programs (battery RAM, logical addresses)

| Address | What |
|---|---|
| C000 | signature `RATES FOR PCM 5` once options are programmed |
| C0CC | telemetry bypass code (length, then digits) |
| C0D5 | station id (PNM logon) |
| C0DB | PNM password |
| C0E6-C0EF | payphone number |
| C240-C24F | option switches (C242 bit 2: refuse out-of-state coin calls; C243 bit 1: answer supervision) |
| C250 + type | call routing by PNM call type (0 = refused) |
| BFFE, CFFE | checksums |
| E013 bit 2 | factory-default rates in use |

## Inputs

### Port 60: hook, line, ring, keypad and coins

The main status port, read many times a second by the main loop.

| Bit | Name | Meaning |
|---|---|---|
| 0 | hook | 1 while the handset is on the cradle. Lifting it powers the board up |
| 1 | loop current | the line feeds current while the phone is off hook; if it stays away the firmware takes the call as ended by the far end |
| 2 | loop current reversed | the line reversed polarity: the far end answered (acted on only with answer supervision on) |
| 3 | ring detect | ringing voltage on the line; wakes the sleeping board and is counted to answer calls |
| 4 | DTMF valid | the keypad's DTMF receiver has a digit ready (code on port 40 bits 0-3) |
| 5 | nickel | coin switch, one pulse per nickel |
| 6 | dime | coin switch, one pulse per dime |
| 7 | quarter | coin switch, one pulse per quarter |

### Port 61: relay feedback, housing, line energy

| Bit | Name | Meaning |
|---|---|---|
| 0 | relay feedback | the line sequencer waits for it after operating the line relays (up to about 5 s) |
| 4 | housing closed | coin housing / door switch; opening it is reported as an alarm |
| 6 | line energy detect | call-progress energy detector, counted while listening for an answer or voice |

Bits 1, 2, 3, 5 and 7 are never read.

### Port 40: keypad digit, tone detectors, straps

| Bit | Name | Meaning |
|---|---|---|
| 0-3 | DTMF code | keypad digit: 1-9, 0A = 0, 0B = `*`, 0C = `#`, 0D-0F = A-C, 00 = D |
| 4 | tone detector A | call-progress tones (busy / SIT) |
| 5 | board strap | read with port A3 bit 7 when the phone goes idle |
| 6 | board strap | read at start-up; chooses a line setting |
| 7 | tone detector B | second call-progress detector |

### Other inputs

| Port | Meaning |
|---|---|
| 41 | line voltage ADC: about 60 hex on hook (around 48 V), 10-18 hex off hook with loop current; tells a working line from a dead one |
| A3 bit 7 | board strap, read when the phone goes idle |
| A5 bit 7 | line interface OK (1 = fine); six 0 readings in a row make the firmware step through four line-interface settings on port A1 bits 0-1 |
| A6 bit 7 | program (reset) button, 0 while pressed. Held while the handset is lifted at power-up, it resets the phone to factory defaults |
| B8 | reading it switches the board off to save power |
| B0 | strobe; the value read is ignored |

## Outputs

| Port | Drives | Details |
|---|---|---|
| 60 | control latch | line and hook relays; bit 3 pulses during the coin relay sequence |
| 61 | voice codec control | switches the CVSD decoder on (92 hex while a prompt plays) |
| A0 | audio routing | which audio goes to the handset and the line, and which tone detectors listen |
| A1 | coin relay | a rising bit 3 pulses the relay; bit 2 chooses collect (1) or refund (0); bits 0-1 are the line-interface setting |
| A2 | aux relays | pulsed around coin and line operations |
| A3 | voice ROM half | bit 0 picks the 64 KB half the CPU and DMA see |
| A4 | EEPROM write enable | bit 0 set only while settings are written |
| A8 | line control | bit 0 puts the DTMF dialer on the line; bit 3 mutes the line while a prompt plays; bit 2 is a line option |
| 63 | aux latch | cleared at start-up, never set |
| 40 | CVSD data | the voice byte stream (from DMA1) |

## Other chips

| Ports | Chip | Use |
|---|---|---|
| 80-8F | OKI MSM6242 real-time clock | time digits and control; 64 Hz tick while awake, and its slow pulse wakes the sleeping board for hourly jobs |
| C1 / C2 | TDK 73K212 modem | C1 selects a register, C2 reads or writes it. The firmware dials with its tone generator (register 3 = DTMF code with bit 4 set, then the transmitter on in CR0) and uses it as a 300 / 1200 bit/s modem for remote programming. Register 2 reports carrier and call progress |
| 00-3F | Z180 internal registers | memory mapping, timers, serial ports, DMA |

## Voice ROM

Two 64 KB pages. Each starts with an index of 4-byte entries: start offset
(little-endian, within the page) and length in bytes. The number of entries
is the first entry's start divided by 4 (at most 128). The audio is 1-bit
CVSD, least significant bit first, at 19,200 bits per second. The firmware's
prompt routine starts at 63A3 with the prompt number in A.
