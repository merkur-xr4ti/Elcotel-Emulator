//! Software Bell 103 modem (300 bit/s FSK, 8N1) working on 8 kHz line audio.
//! Stands in for the analog side of the payphone's TDK 73K212 when a PNM
//! modem dials in over the internet line: the answering side sends on
//! 2225/2025 Hz (mark/space) and listens on 1270/1070 Hz.

use std::collections::VecDeque;
use std::f32::consts::TAU;

const FS: f32 = 8000.0;
const BAUD: f32 = 300.0;

struct ToneDet {
    w: f32,
    n: usize,
    buf_i: VecDeque<f32>,
    buf_q: VecDeque<f32>,
    si: f32,
    sq: f32,
}

impl ToneDet {
    fn new(freq: f32, n: usize) -> Self {
        ToneDet { w: TAU * freq / FS, n, buf_i: VecDeque::new(), buf_q: VecDeque::new(), si: 0.0, sq: 0.0 }
    }
    /// Magnitude of this tone over the last n samples.
    fn push(&mut self, t: u64, x: f32) -> f32 {
        let ph = self.w * (t % 80_000) as f32;
        let i = x * ph.cos();
        let q = x * ph.sin();
        self.si += i;
        self.sq += q;
        self.buf_i.push_back(i);
        self.buf_q.push_back(q);
        if self.buf_i.len() > self.n {
            self.si -= self.buf_i.pop_front().unwrap();
            self.sq -= self.buf_q.pop_front().unwrap();
        }
        (self.si * self.si + self.sq * self.sq).sqrt() / self.n as f32
    }
}

pub struct FskModem {
    pub answer: bool,
    t: u64,
    // transmit
    tx_phase: f32,
    tx_bits: VecDeque<bool>,
    tx_bit: bool,
    tx_acc: f32,
    pub tx_enabled: bool,
    // receive
    mark: ToneDet,
    space: ToneDet,
    level: f32,
    pub carrier: bool,
    rx_state: i32,
    rx_count: f32,
    rx_byte: u8,
    rx_bits: u32,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

impl FskModem {
    pub fn new(answer: bool) -> Self {
        let (m, s) = if answer { (1270.0, 1070.0) } else { (2225.0, 2025.0) };
        FskModem {
            answer,
            t: 0,
            tx_phase: 0.0,
            tx_bits: VecDeque::new(),
            tx_bit: true,
            tx_acc: 0.0,
            tx_enabled: true,
            mark: ToneDet::new(m, 20),
            space: ToneDet::new(s, 20),
            level: 0.0,
            carrier: false,
            rx_state: -1,
            rx_count: 0.0,
            rx_byte: 0,
            rx_bits: 0,
            rx_bytes: 0,
            tx_bytes: 0,
        }
    }

    pub fn send_byte(&mut self, b: u8) {
        self.tx_bits.push_back(false);
        for i in 0..8 {
            self.tx_bits.push_back(b >> i & 1 != 0);
        }
        self.tx_bits.push_back(true);
        self.tx_bytes += 1;
    }

    /// Produce `n` samples of line audio.
    pub fn modulate(&mut self, n: usize) -> Vec<f32> {
        let (mark, space) = if self.answer { (2225.0, 2025.0) } else { (1270.0, 1070.0) };
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            self.tx_acc += BAUD / FS;
            if self.tx_acc >= 1.0 {
                self.tx_acc -= 1.0;
                self.tx_bit = self.tx_bits.pop_front().unwrap_or(true);
            }
            if !self.tx_enabled {
                out.push(0.0);
                continue;
            }
            let f = if self.tx_bit { mark } else { space };
            self.tx_phase = (self.tx_phase + TAU * f / FS) % TAU;
            out.push(0.35 * self.tx_phase.sin());
        }
        out
    }

    /// Consume line audio; returns the bytes received.
    pub fn demodulate(&mut self, samples: &[f32]) -> Vec<u8> {
        let mut got = Vec::new();
        let spb = FS / BAUD; // samples per bit
        for &x in samples {
            self.t += 1;
            let m = self.mark.push(self.t, x);
            let s = self.space.push(self.t, x);
            let e = m + s;
            self.level += (e - self.level) * 0.01;
            let was = self.carrier;
            self.carrier = if was { self.level > 0.02 } else { self.level > 0.05 };
            if !self.carrier {
                self.rx_state = -1;
                continue;
            }
            let bit = m >= s;
            match self.rx_state {
                -1 => {
                    if !bit {
                        // start bit edge: sample its middle
                        self.rx_state = 0;
                        self.rx_count = spb * 0.5;
                    }
                }
                _ => {
                    self.rx_count -= 1.0;
                    if self.rx_count <= 0.0 {
                        self.rx_count += spb;
                        match self.rx_state {
                            0 => {
                                if bit {
                                    self.rx_state = -1; // false start
                                } else {
                                    self.rx_state = 1;
                                    self.rx_byte = 0;
                                    self.rx_bits = 0;
                                }
                            }
                            1..=8 => {
                                if bit {
                                    self.rx_byte |= 1 << self.rx_bits;
                                }
                                self.rx_bits += 1;
                                self.rx_state += 1;
                            }
                            _ => {
                                if bit {
                                    got.push(self.rx_byte);
                                    self.rx_bytes += 1;
                                }
                                self.rx_state = -1;
                            }
                        }
                    }
                }
            }
        }
        got
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loopback() {
        let mut a = FskModem::new(true);
        let mut o = FskModem::new(false);
        let idle = o.modulate(800);
        a.demodulate(&idle);
        for b in [0x02u8, 0x09, 0x55, 0xA3, 0x03] {
            o.send_byte(b);
        }
        let mut got = Vec::new();
        for _ in 0..200 {
            let s = o.modulate(80);
            got.extend(a.demodulate(&s));
        }
        assert_eq!(got, vec![0x02, 0x09, 0x55, 0xA3, 0x03]);
    }
}

/// The line side of the payphone's 73K212 for a dial-in: answer tone, then
/// whichever the caller speaks - Bell 103 (300 bit/s FSK) or Bell 212A /
/// V.22 (1200 bit/s DPSK).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineMode {
    AnswerTone,
    Fsk300,
    Dpsk1200,
}

pub struct LineModem {
    pub mode: LineMode,
    answer: bool,
    t: u64,
    tone_phase: f32,
    fsk: FskModem,
    dtx: crate::dpsk::DpskTx,
    drx: crate::dpsk::DpskRx,
    n1270: ToneDet,
    n1070: ToneDet,
    n2225: ToneDet,
    sig_t: u64,
    ratio: f32,
    pending: Vec<u8>,
    dpsk_since: u64,
    heard_tone: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    tx_enabled: bool,
    originate_dpsk: bool,
}

impl LineModem {
    pub fn new_answer() -> Self {
        Self::new(true, false)
    }
    /// For tests: a calling modem (PNM's side).
    pub fn new_originate(dpsk: bool) -> Self {
        Self::new(false, dpsk)
    }
    fn new(answer: bool, originate_dpsk: bool) -> Self {
        LineModem {
            mode: LineMode::AnswerTone,
            answer,
            t: 0,
            tone_phase: 0.0,
            fsk: FskModem::new(answer),
            dtx: crate::dpsk::DpskTx::new(if answer { 2400.0 } else { 1200.0 }),
            drx: crate::dpsk::DpskRx::new(if answer { 1200.0 } else { 2400.0 }),
            n1270: ToneDet::new(1270.0, 80),
            n1070: ToneDet::new(1070.0, 80),
            n2225: ToneDet::new(2225.0, 80),
            sig_t: 0,
            ratio: 0.5,
            pending: Vec::new(),
            dpsk_since: 0,
            heard_tone: 0,
            rx_bytes: 0,
            tx_bytes: 0,
            tx_enabled: true,
            originate_dpsk,
        }
    }

    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            LineMode::AnswerTone => "answer tone",
            LineMode::Fsk300 => "300 bit/s (Bell 103)",
            LineMode::Dpsk1200 => "1200 bit/s (Bell 212A / V.22)",
        }
    }

    pub fn carrier(&self) -> bool {
        match self.mode {
            LineMode::AnswerTone => false,
            LineMode::Fsk300 => self.fsk.carrier,
            LineMode::Dpsk1200 => self.drx.locked && self.drx.power > 0.002 && self.t - self.dpsk_since > 6400,
        }
    }

    pub fn set_tx_enabled(&mut self, on: bool) {
        self.tx_enabled = on;
    }

    pub fn send_byte(&mut self, b: u8) {
        self.tx_bytes += 1;
        match self.mode {
            LineMode::AnswerTone => self.pending.push(b),
            LineMode::Fsk300 => self.fsk.send_byte(b),
            LineMode::Dpsk1200 => self.dtx.send_byte(b),
        }
    }

    fn set_mode(&mut self, m: LineMode) {
        self.mode = m;
        self.dpsk_since = self.t;
        let p = std::mem::take(&mut self.pending);
        for b in p {
            match m {
                LineMode::Fsk300 => self.fsk.send_byte(b),
                LineMode::Dpsk1200 => self.dtx.send_byte(b),
                LineMode::AnswerTone => {}
            }
        }
    }

    pub fn modulate(&mut self, n: usize) -> Vec<f32> {
        match self.mode {
            LineMode::AnswerTone => {
                if self.answer {
                    // Bell answer tone (the 73K212 in answer mode)
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        self.tone_phase = (self.tone_phase + TAU * 2225.0 / FS) % TAU;
                        v.push(0.3 * self.tone_phase.sin());
                    }
                    self.t += n as u64;
                    v
                } else {
                    self.t += n as u64;
                    vec![0.0; n]
                }
            }
            LineMode::Fsk300 => {
                self.t += n as u64;
                self.fsk.tx_enabled = self.tx_enabled || !self.answer;
                self.fsk.modulate(n)
            }
            LineMode::Dpsk1200 => {
                self.t += n as u64;
                self.dtx.enabled = self.tx_enabled || !self.answer;
                self.dtx.modulate(n)
            }
        }
    }

    pub fn demodulate(&mut self, samples: &[f32]) -> Vec<u8> {
        let mut out = Vec::new();
        if self.mode == LineMode::AnswerTone {
            for &x in samples {
                let ts = self.sig_t + 1;
                if self.answer {
                    // what is the caller sending: pure FSK mark tones, or a
                    // spread DPSK signal around 1200 Hz?
                    let a = self.n1270.push(ts, x);
                    let b = self.n1070.push(ts, x);
                    let _ = self.drx.demodulate(&[x]);
                    let p = self.drx.power;
                    if p > 0.004 {
                        let r = ((2.0 * a).powi(2) + (2.0 * b).powi(2)) / p;
                        self.ratio += (r.min(2.0) - self.ratio) * 0.002;
                        self.heard_tone += 1;
                    } else {
                        self.heard_tone = self.heard_tone.saturating_sub(2);
                    }
                    self.sig_t = ts;
                    if self.heard_tone > 2400 {
                        let m = if self.ratio > 0.6 { LineMode::Fsk300 } else { LineMode::Dpsk1200 };
                        self.set_mode(m);
                        break;
                    }
                } else {
                    // originate: wait for the answer tone, then start
                    let a = self.n2225.push(ts, x);
                    self.sig_t = ts;
                    if a > 0.05 {
                        self.heard_tone += 1;
                    }
                    if self.heard_tone > 1600 {
                        let m = if self.originate_dpsk { LineMode::Dpsk1200 } else { LineMode::Fsk300 };
                        self.set_mode(m);
                        break;
                    }
                }
            }
            return out;
        }
        let got = match self.mode {
            LineMode::Fsk300 => self.fsk.demodulate(samples),
            LineMode::Dpsk1200 => {
                let g = self.drx.demodulate(samples);
                if self.t - self.dpsk_since > 6400 {
                    g
                } else {
                    Vec::new()
                }
            }
            LineMode::AnswerTone => Vec::new(),
        };
        self.rx_bytes += got.len() as u64;
        out.extend(got);
        out
    }
}
