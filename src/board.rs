//! The payphone's circuit board as the firmware sees it.
//!
//! Physical memory (Z180 20-bit addresses):
//!   0x00000-0x0FFFF  program EPROM (27C512)
//!   0x10000-0x1FFFF  voice EPROM window (27C010, half picked by port 0xA3 bit 0)
//!   0x20000-0x27FFF  RAM (download area for RAM-loaded firmware)
//!   0x28000-0x29FFF  rates & options EEPROM (AT28C64, writes enabled by port 0xA4)
//!   0x30000-0x3FFFF  battery-backed RAM (work area, options copy)
//!
//! I/O ports (external, A7..A0):
//!   0x40  W: CVSD voice data (fed by DMA1)   R: bits0-3 DTMF receiver code
//!   0x41  R: analog level (battery / line), compared by the firmware
//!   0x60  R: b0 hook (1 = on-hook), b3 aux switch, b4 DTMF valid (StD),
//!            b5 nickel, b6 dime, b7 quarter     W: control latch
//!   0x61  R: b0 coin relay feedback, b6/b7 line status   W: voice codec control
//!   0x80-0x8F  real-time clock (MSM6242)
//!   0xA0-0xA3, 0xA8  output latches (relays, audio routing, voice page)
//!   0xA4  W: EEPROM write enable
//!   0xA6  R: b7 = 1 normally, 0 while the program (reset) button is held
//!   0xB8  R: switches the board off (with IOSTP set)
//!   0xC1/0xC2  TDK 73K212 modem: register address / data

use crate::cvsd::Cvsd;
use crate::rtc::Rtc;
use crate::z180::{Board, Z180};
use std::collections::{BTreeMap, VecDeque};

pub const CPU_HZ: u64 = 6_144_000;
/// CVSD bit rate (TOUT of PRT1 divided by two on the board).
pub const VOICE_BIT_RATE: u32 = 19_200;

#[derive(Default, Clone, Debug)]
pub struct PortStat {
    pub reads: u64,
    pub writes: u64,
    pub last_read: u8,
    pub last_write: u8,
    pub pcs: Vec<u16>,
    pub write_values: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Prompt(u8),
    DtmfOut(char),
    LineSeized(bool),
    /// Coin relay pulse: true = collect into the cash box, false = refund.
    CoinRelay(bool),
}

/// TDK 73K212 single-chip modem (only what the firmware uses).
#[derive(Clone, Debug, Default)]
pub struct Modem {
    pub addr: u8,
    pub regs: [u8; 8],
    pub dtmf_active: Option<char>,
}

pub fn dtmf_char(code: u8) -> char {
    match code & 0x0F {
        1..=9 => (b'0' + (code & 0x0F)) as char,
        0x0A => '0',
        0x0B => '*',
        0x0C => '#',
        0x0D => 'A',
        0x0E => 'B',
        0x0F => 'C',
        _ => 'D',
    }
}

pub fn dtmf_code(c: char) -> u8 {
    match c {
        '1'..='9' => c as u8 - b'0',
        '0' => 0x0A,
        '*' => 0x0B,
        '#' => 0x0C,
        'A' => 0x0D,
        'B' => 0x0E,
        'C' => 0x0F,
        _ => 0x00,
    }
}

/// Physical range covered by the memory map: low RAM, EEPROM, battery RAM.
pub const MEM_AGE_BASE: u32 = 0x20000;
pub const MEM_AGE_LEN: usize = 0x20000;

/// Last read / write time of each byte (see PayBoard::mem_age).
pub struct MemAge {
    pub rd: Vec<u32>,
    pub wr: Vec<u32>,
}

impl MemAge {
    pub fn new() -> Box<MemAge> {
        Box::new(MemAge { rd: vec![0; MEM_AGE_LEN], wr: vec![0; MEM_AGE_LEN] })
    }
}

/// The current time in mem_age ticks.
pub fn mem_tick(cycles: u64) -> u32 {
    (cycles >> 14) as u32 + 1
}

pub struct PayBoard {
    pub rom: Vec<u8>,
    pub voice: Vec<u8>,
    pub ram_lo: Vec<u8>,
    pub ram: Vec<u8>,
    pub eeprom: Vec<u8>,
    pub eeprom_dirty: bool,
    /// The far end hung up: the line gives no loop current until the
    /// handset goes back on hook.
    pub loop_open: bool,
    /// The far end answered and the line reversed its polarity (answer
    /// supervision): loop current shows on bit 2 instead of bit 1.
    pub line_reversed: bool,
    /// Memory touched since start (coverage view): bit0 read, bit1 written,
    /// one byte per physical address 0x00000-0x3FFFF.
    pub mem_seen: Option<Vec<u8>>,
    /// When each physical byte 0x20000-0x3FFFF (RAM, EEPROM, battery RAM)
    /// was last read / written, in ticks of 2^14 CPU clocks (0 = never).
    pub mem_age: Option<Box<MemAge>>,
    pub rtc: Rtc,
    /// Raw input port values (bits the person drives are merged in on read).
    pub inputs: [u8; 256],
    pub outputs: [u8; 256],
    pub rom_latch: u8,
    pub stats: BTreeMap<u16, PortStat>,
    pub cur_pc: u16,
    pub cycles: u64,
    pub serial_out: [Vec<u8>; 2],
    pub serial_in: [VecDeque<u8>; 2],
    pub power_off: bool,
    pub watch_writes: Vec<u16>,
    pub modem: Modem,
    pub events: Vec<Event>,

    // --- things the person does ---
    pub program_button: bool,
    pub ring_signal: bool,
    /// Where the ring detector shows up: (port, mask, active_low).
    pub ring_bit: (u8, u8, bool),
    /// Ring detector output at 20 Hz (true) or as a steady level (false).
    pub ring_pulsed: bool,
    ring_phase: u64,
    /// DTMF code currently held on the keypad and remaining time in cycles.
    key: Option<(u8, u64, u32)>,
    /// (code, minimum time, press id); code 0xFF = gap between keys.
    key_queue: VecDeque<(u8, u64, u32)>,
    /// Press ids whose key is still held down by the person.
    held: Vec<u32>,
    next_key_id: u32,
    /// No new key reaches the board before this cycle (board waking up).
    pub keys_blocked_until: u64,
    coin_pulse: Option<(u8, u64)>,
    coin_queue: VecDeque<u8>,
    pub coin_count: [u32; 3],

    // --- voice ---
    dreq: bool,
    tout_count: u32,
    pub cvsd: Cvsd,
    /// Decoded voice samples at VOICE_BIT_RATE, for the audio output.
    pub voice_pcm: VecDeque<f32>,
    pub voice_bytes: u64,
}

impl PayBoard {
    pub fn new(rom: Vec<u8>, voice: Vec<u8>) -> Self {
        let mut rom = rom;
        rom.resize(0x10000, 0xFF);
        let mut voice = voice;
        voice.resize(0x20000, 0xFF);
        let mut inputs = [0u8; 256];
        inputs[0x60] = 0x01; // on-hook
                             // b4 set = upper housing closed: telemetry then asks for the bypass code
        inputs[0x61] = 0x91;
        inputs[0x41] = 0x60;
        inputs[0xA5] = 0x80;
        inputs[0xA6] = 0x80;
        PayBoard {
            rom,
            voice,
            ram_lo: vec![0; 0x8000],
            ram: vec![0; 0x10000],
            eeprom: vec![0xFF; 0x2000],
            eeprom_dirty: false,
            loop_open: false,
            line_reversed: false,
            mem_seen: None,
            mem_age: None,
            rtc: Rtc::new(CPU_HZ),
            inputs,
            outputs: [0; 256],
            rom_latch: 0,
            stats: BTreeMap::new(),
            cur_pc: 0,
            cycles: 0,
            serial_out: [Vec::new(), Vec::new()],
            serial_in: [Default::default(), Default::default()],
            power_off: false,
            watch_writes: Vec::new(),
            modem: Modem::default(),
            events: Vec::new(),
            program_button: false,
            ring_signal: false,
            // ring detector output: port 0x60 bit 3 (found by tracing the firmware)
            ring_bit: (0x60, 0x08, false),
            ring_pulsed: false,
            ring_phase: 0,
            key: None,
            key_queue: VecDeque::new(),
            held: Vec::new(),
            next_key_id: 1,
            keys_blocked_until: 0,
            coin_pulse: None,
            coin_queue: VecDeque::new(),
            coin_count: [0; 3],
            dreq: false,
            tout_count: 0,
            cvsd: Cvsd::new(),
            voice_pcm: VecDeque::new(),
            voice_bytes: 0,
        }
    }

    // ----- person-facing controls -----
    pub fn on_hook(&self) -> bool {
        self.inputs[0x60] & 1 != 0
    }
    pub fn set_hook(&mut self, on_hook: bool) {
        if on_hook {
            self.inputs[0x60] |= 1;
            self.loop_open = false;
            self.line_reversed = false;
        } else {
            self.inputs[0x60] &= !1;
        }
    }
    /// A short tap (typed digits, scripts).
    pub fn press_key(&mut self, c: char) {
        let code = dtmf_code(c);
        let id = self.next_key_id;
        self.next_key_id += 1;
        self.key_queue.push_back((code, CPU_HZ * 90 / 1000, id));
    }
    /// Key pushed down: it stays pressed (at least 70 ms) until key_release.
    pub fn key_hold(&mut self, c: char) -> u32 {
        let code = dtmf_code(c);
        let id = self.next_key_id;
        self.next_key_id += 1;
        self.held.push(id);
        self.key_queue.push_back((code, CPU_HZ * 70 / 1000, id));
        id
    }
    pub fn key_release(&mut self, id: u32) {
        self.held.retain(|h| *h != id);
    }
    /// Keys waiting to reach the board.
    pub fn keys_pending(&self) -> usize {
        self.key_queue.iter().filter(|k| k.0 != 0xFF).count()
    }
    pub fn drop_coin(&mut self, which: usize) {
        self.coin_queue.push_back(which as u8);
    }
    /// Ring voltage present right now (2 s on, 4 s off cadence).
    pub fn ring_now(&self) -> bool {
        if !self.ring_signal || self.ring_phase >= CPU_HZ * 2 {
            return false;
        }
        if self.ring_pulsed {
            (self.ring_phase / (CPU_HZ / 40)).is_multiple_of(2)
        } else {
            true
        }
    }
    pub fn line_seized(&self) -> bool {
        self.outputs[0xA8] & 0x01 != 0
    }
    pub fn key_down(&self) -> Option<char> {
        self.key.map(|(c, _, _)| dtmf_char(c))
    }

    fn stat(&mut self, port: u16) -> &mut PortStat {
        let pc = self.cur_pc;
        let s = self.stats.entry(port).or_default();
        if s.pcs.len() < 12 && !s.pcs.contains(&pc) {
            s.pcs.push(pc);
        }
        s
    }

    fn read_60(&self) -> u8 {
        let mut v = self.inputs[0x60] & 0x0F;
        // b1: loop current. Off hook the line feeds current; without it
        // (bits 1 and 2 both low for a while) the firmware takes the call as
        // ended by the far end, settles the coins and drops the line, so a
        // paid call would never reach its overtime warning.
        // b2 instead of b1 once the far end answered with a polarity reversal
        // (routine B615 compares both bits with the last state; it is only
        // acted on when the options enable answer supervision).
        if v & 1 == 0 && !self.loop_open {
            v |= if self.line_reversed { 0x04 } else { 0x02 };
        }
        if self.key.is_some() {
            v |= 0x10;
        }
        if let Some((w, _)) = self.coin_pulse {
            if w < 3 {
                v |= 0x20 << w;
            }
        }
        v
    }

    fn read_40(&self) -> u8 {
        let mut v = self.inputs[0x40] & 0xF0;
        match self.key {
            Some((code, _, _)) => v |= code & 0x0F,
            None => v |= self.inputs[0x40] & 0x0F,
        }
        v
    }

    fn modem_read(&mut self) -> u8 {
        let a = (self.modem.addr & 7) as usize;
        match a {
            2 => self.inputs[0xC2],
            _ => self.modem.regs[a],
        }
    }

    fn modem_write(&mut self, v: u8) {
        let a = (self.modem.addr & 7) as usize;
        let old = self.modem.regs[a];
        self.modem.regs[a] = v;
        // The firmware dials a digit by loading the tone register (DTMF code
        // | 0x10) and then switching the transmitter on in CR0 (0x30 -> 0x32)
        // for about 90 ms. Repeated digits only show as a new TX-on edge.
        if a == 0 && old & 0x02 == 0 && v & 0x02 != 0 && self.modem.regs[3] & 0x10 != 0 {
            let c = dtmf_char(self.modem.regs[3]);
            self.events.push(Event::DtmfOut(c));
            self.modem.dtmf_active = Some(c);
        } else if a == 0 && v & 0x02 == 0 {
            self.modem.dtmf_active = None;
        }
    }

    /// Called before each CPU instruction (cheap checks only).
    #[inline]
    pub fn on_step(&mut self, z: &Z180) {
        if z.cpu.pc == 0x63A3 {
            self.events.push(Event::Prompt(z.cpu.a));
        }
    }

    pub fn on_power_off(&mut self) {
        self.voice_pcm.clear();
        self.modem.dtmf_active = None;
    }

    /// Time passing while the board is switched off.
    pub fn sleep_tick(&mut self, cycles: u32) {
        self.cycles += cycles as u64;
        self.rtc.tick(cycles);
        self.advance_person(cycles, false);
    }

    fn advance_person(&mut self, cycles: u32, awake: bool) {
        let c = cycles as u64;
        if self.ring_signal {
            self.ring_phase = (self.ring_phase + c) % (CPU_HZ * 6);
        } else {
            self.ring_phase = 0;
        }
        // keypad: each press lasts its minimum time (or as long as it is
        // held), then a 70 ms gap so the DTMF receiver sees separate digits.
        match self.key {
            Some((code, left, id)) => {
                if left > c {
                    self.key = Some((code, left - c, id));
                } else if self.held.contains(&id) {
                    self.key = Some((code, 0, id));
                } else {
                    self.key = None;
                    self.key_queue.push_front((0xFF, CPU_HZ * 70 / 1000, 0));
                }
            }
            None => {
                if let Some((code, len, _)) = self.key_queue.front_mut() {
                    if *code == 0xFF {
                        if *len <= c {
                            self.key_queue.pop_front();
                        } else {
                            *len -= c;
                        }
                    } else if awake && self.cycles >= self.keys_blocked_until {
                        self.key = self.key_queue.pop_front();
                    }
                }
            }
        }
        // coins: ~40 ms pulse, at least 800 ms between coins (the firmware misses faster ones)
        match self.coin_pulse {
            Some((w, left)) => {
                if left <= c {
                    self.coin_pulse = None;
                    if w < 3 {
                        self.coin_count[w as usize] += 1;
                        self.coin_pulse = Some((0xFF, CPU_HZ * 800 / 1000));
                    }
                } else {
                    self.coin_pulse = Some((w, left - c));
                }
            }
            None => {
                if let Some(w) = self.coin_queue.pop_front() {
                    self.coin_pulse = Some((w, CPU_HZ * 40 / 1000));
                }
            }
        }
    }

    fn cvsd_byte(&mut self, v: u8) {
        self.voice_bytes += 1;
        let mut tmp = Vec::with_capacity(8);
        self.cvsd.byte(v, &mut tmp);
        if self.voice_pcm.len() < VOICE_BIT_RATE as usize * 4 {
            self.voice_pcm.extend(tmp);
        }
    }
}

impl Board for PayBoard {
    fn mem_read(&mut self, p: u32) -> u8 {
        if let Some(a) = self.mem_age.as_mut() {
            if p >= MEM_AGE_BASE && p < MEM_AGE_BASE + MEM_AGE_LEN as u32 {
                a.rd[(p - MEM_AGE_BASE) as usize] = (self.cycles >> 14) as u32 + 1;
            }
        }
        if let Some(m) = self.mem_seen.as_mut() {
            if let Some(x) = m.get_mut(p as usize) {
                *x |= 1;
            }
        }
        match p {
            0x00000..=0x0FFFF => self.rom[p as usize],
            0x10000..=0x1FFFF => {
                let half = (self.outputs[0xA3] & 1) as usize;
                self.voice[(half << 16) | (p as usize & 0xFFFF)]
            }
            0x20000..=0x27FFF => self.ram_lo[p as usize & 0x7FFF],
            0x28000..=0x2FFFF => self.eeprom[p as usize & 0x1FFF],
            0x30000..=0x3FFFF => self.ram[p as usize & 0xFFFF],
            _ => 0xFF,
        }
    }

    fn mem_write(&mut self, p: u32, v: u8) {
        if let Some(a) = self.mem_age.as_mut() {
            if p >= MEM_AGE_BASE && p < MEM_AGE_BASE + MEM_AGE_LEN as u32 {
                a.wr[(p - MEM_AGE_BASE) as usize] = (self.cycles >> 14) as u32 + 1;
            }
        }
        if let Some(m) = self.mem_seen.as_mut() {
            if let Some(x) = m.get_mut(p as usize) {
                *x |= 2;
            }
        }
        match p {
            0x00000..=0x0FFFF => self.rom_latch = v,
            0x20000..=0x27FFF => self.ram_lo[p as usize & 0x7FFF] = v,
            0x28000..=0x2FFFF => {
                if self.outputs[0xA4] & 1 != 0 {
                    let a = p as usize & 0x1FFF;
                    if self.eeprom[a] != v {
                        self.eeprom[a] = v;
                        self.eeprom_dirty = true;
                    }
                }
            }
            0x30000..=0x3FFFF => self.ram[p as usize & 0xFFFF] = v,
            _ => {}
        }
    }

    fn io_read(&mut self, port: u16) -> u8 {
        let p = (port & 0xFF) as u8;
        let mut v = match p {
            0x80..=0x8F => self.rtc.read(p),
            0x40 => self.read_40(),
            0x60 => self.read_60(),
            0xA6 => {
                if self.program_button {
                    self.inputs[0xA6] & 0x7F
                } else {
                    self.inputs[0xA6] | 0x80
                }
            }
            0xB8 => {
                self.power_off = true;
                self.inputs[0xB8]
            }
            0xC2 => self.modem_read(),
            _ => self.inputs[p as usize],
        };
        if p == self.ring_bit.0 {
            let on = self.ring_now();
            let (_, mask, low) = self.ring_bit;
            if on != low {
                v |= mask;
            } else {
                v &= !mask;
            }
        }
        let s = self.stat(p as u16);
        s.reads += 1;
        s.last_read = v;
        v
    }

    fn io_write(&mut self, port: u16, v: u8) {
        let p = (port & 0xFF) as u8;
        let old = self.outputs[p as usize];
        self.outputs[p as usize] = v;
        match p {
            0x80..=0x8F => self.rtc.write(p, v),
            0x40 => {
                self.cvsd_byte(v);
                self.dreq = false;
            }
            0xC1 => self.modem.addr = v,
            0xC2 => self.modem_write(v),
            0xA1 => {
                if old & 0x08 == 0 && v & 0x08 != 0 {
                    self.events.push(Event::CoinRelay(v & 0x04 != 0));
                }
            }
            0xA8 if (old ^ v) & 1 != 0 => {
                self.events.push(Event::LineSeized(v & 1 != 0));
            }
            _ => {}
        }
        let s = self.stat(p as u16);
        s.writes += 1;
        s.last_write = v;
        if s.write_values.len() < 16 && !s.write_values.contains(&v) {
            s.write_values.push(v);
        }
    }

    fn tick(&mut self, cycles: u32, tout_edges: u32) {
        self.cycles += cycles as u64;
        self.rtc.tick(cycles);
        self.advance_person(cycles, true);
        // The codec asks for a byte every 8 bit clocks; TOUT toggles at
        // 2 x 19.2 kHz, so 4 edges per bit, 32 per byte.
        self.tout_count += tout_edges;
        if self.tout_count >= 32 {
            self.tout_count -= 32;
            self.dreq = true;
        }
    }

    fn int_lines(&mut self) -> (bool, bool, bool) {
        (false, self.rtc.irq(), false)
    }

    fn dreq1(&mut self) -> bool {
        self.dreq
    }

    fn asci_rx(&mut self, ch: usize) -> Option<u8> {
        self.serial_in[ch].pop_front()
    }

    fn asci_tx(&mut self, ch: usize, v: u8) {
        if self.serial_out[ch].len() >= 65536 && ch == 1 {
            // PRINT output: keep the newest
            self.serial_out[1].drain(..16384);
        }
        if self.serial_out[ch].len() < 65536 {
            self.serial_out[ch].push(v);
        }
    }

    fn note_write(&mut self, logical: u16, v: u8) {
        if !self.watch_writes.is_empty() && self.watch_writes.contains(&logical) {
            println!("{:8.3}s  write {:04X} <- {:02X} (pc {:04X})", self.cycles as f64 / CPU_HZ as f64, logical, v, self.cur_pc);
        }
    }
}
