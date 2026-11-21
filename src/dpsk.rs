//! 1200 bit/s DPSK modem (Bell 212A / ITU V.22 data format) on 8 kHz audio.
//! 600 baud, two bits per symbol, differential phase (00 = +90, 01 = 0,
//! 11 = +270, 10 = +180 degrees), self-synchronising scrambler 1 + x^-14 + x^-17,
//! async 8N1 characters carried in the bit stream. The originating modem
//! uses a 1200 Hz carrier, the answering one 2400 Hz.

use std::collections::VecDeque;
use std::f32::consts::{PI, TAU};

const FS: f32 = 8000.0;
const SYMBOL_RATE: f32 = 600.0;

fn rc_pulse(t: f32) -> f32 {
    // raised cosine, beta 0.75, t in symbol periods
    let beta = 0.75;
    if t.abs() < 1e-6 {
        return 1.0;
    }
    let sinc = (PI * t).sin() / (PI * t);
    let d = 1.0 - (2.0 * beta * t).powi(2);
    if d.abs() < 1e-4 {
        return (PI / 4.0) * sinc;
    }
    sinc * (PI * beta * t).cos() / d
}

pub struct DpskTx {
    pub fc: f32,
    t: u64,
    bits: VecDeque<bool>,
    sr: u32,
    phase: f32,
    syms: VecDeque<(f32, f32, f32)>, // (time in samples, re, im)
    next_sym: f32,
    pub enabled: bool,
}

impl DpskTx {
    pub fn new(fc: f32) -> Self {
        DpskTx { fc, t: 0, bits: VecDeque::new(), sr: 0, phase: 0.0, syms: VecDeque::new(), next_sym: 0.0, enabled: true }
    }
    pub fn send_byte(&mut self, b: u8) {
        self.bits.push_back(false);
        for i in 0..8 {
            self.bits.push_back(b >> i & 1 != 0);
        }
        self.bits.push_back(true);
    }
    fn scramble(&mut self, b: bool) -> bool {
        let s = b ^ ((self.sr >> 13) & 1 != 0) ^ ((self.sr >> 16) & 1 != 0);
        self.sr = (self.sr << 1) | s as u32;
        s
    }
    fn next_dibit(&mut self) -> (bool, bool) {
        let b1 = self.bits.pop_front().unwrap_or(true);
        let b2 = self.bits.pop_front().unwrap_or(true);
        (self.scramble(b1), self.scramble(b2))
    }
    pub fn modulate(&mut self, n: usize) -> Vec<f32> {
        let sps = FS / SYMBOL_RATE;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let t = self.t as f32;
            while self.next_sym <= t + 2.0 * sps {
                let (q1, q2) = self.next_dibit();
                let change = match (q1, q2) {
                    (false, false) => 0.5 * PI,
                    (false, true) => 0.0,
                    (true, true) => 1.5 * PI,
                    (true, false) => PI,
                };
                self.phase = (self.phase + change) % TAU;
                self.syms.push_back((self.next_sym, self.phase.cos(), self.phase.sin()));
                self.next_sym += sps;
            }
            while let Some(&(ts, _, _)) = self.syms.front() {
                if ts < t - 2.0 * sps {
                    self.syms.pop_front();
                } else {
                    break;
                }
            }
            let (mut re, mut im) = (0.0, 0.0);
            for &(ts, a, b) in &self.syms {
                let h = rc_pulse((t - ts) / sps);
                re += a * h;
                im += b * h;
            }
            let w = TAU * self.fc / FS * (self.t % 80_000) as f32;
            let x = re * w.cos() - im * w.sin();
            out.push(if self.enabled { 0.3 * x } else { 0.0 });
            self.t += 1;
        }
        out
    }
}

struct Ma {
    n: usize,
    buf: VecDeque<(f32, f32)>,
    s: (f32, f32),
}
impl Ma {
    fn new(n: usize) -> Self {
        Ma { n, buf: VecDeque::new(), s: (0.0, 0.0) }
    }
    fn push(&mut self, v: (f32, f32)) -> (f32, f32) {
        self.s.0 += v.0;
        self.s.1 += v.1;
        self.buf.push_back(v);
        if self.buf.len() > self.n {
            let o = self.buf.pop_front().unwrap();
            self.s.0 -= o.0;
            self.s.1 -= o.1;
        }
        (self.s.0 / self.n as f32, self.s.1 / self.n as f32)
    }
}

pub struct DpskRx {
    pub fc: f32,
    t: u64,
    f1: Ma,
    f2: Ma,
    hist: VecDeque<(f32, f32)>,
    // timing
    next: f32,
    sps: f32,
    prev: (f32, f32),
    have_prev: bool,
    pub power: f32,
    sr: u32,
    // uart
    rx_state: i32,
    rx_byte: u8,
    rx_n: u32,
    ones: u32,
    pub locked: bool,
}

impl DpskRx {
    pub fn new(fc: f32) -> Self {
        DpskRx {
            fc,
            t: 0,
            f1: Ma::new(13),
            f2: Ma::new(7),
            hist: VecDeque::new(),
            next: 20.0,
            sps: FS / SYMBOL_RATE,
            prev: (0.0, 0.0),
            have_prev: false,
            power: 0.0,
            sr: 0,
            rx_state: -1,
            rx_byte: 0,
            rx_n: 0,
            ones: 0,
            locked: false,
        }
    }

    fn interp(&self, at: f32) -> (f32, f32) {
        // hist holds the last samples; index 0 is time (t - len)
        let base = self.t as f32 - self.hist.len() as f32;
        let x = at - base;
        let i = x.floor().max(0.0) as usize;
        let fr = x - x.floor();
        let a = self.hist.get(i).copied().unwrap_or((0.0, 0.0));
        let b = self.hist.get(i + 1).copied().unwrap_or(a);
        (a.0 + (b.0 - a.0) * fr, a.1 + (b.1 - a.1) * fr)
    }

    fn descramble(&mut self, q: bool) -> bool {
        let b = q ^ ((self.sr >> 13) & 1 != 0) ^ ((self.sr >> 16) & 1 != 0);
        self.sr = (self.sr << 1) | q as u32;
        b
    }

    fn uart_bit(&mut self, b: bool, out: &mut Vec<u8>) {
        if b {
            self.ones += 1;
        } else {
            self.ones = 0;
        }
        if !self.locked {
            // training: wait for a run of (descrambled) ones
            if self.ones >= 24 {
                self.locked = true;
            }
            return;
        }
        match self.rx_state {
            -1 => {
                if !b {
                    self.rx_state = 0;
                    self.rx_byte = 0;
                    self.rx_n = 0;
                }
            }
            0 => {
                if b {
                    self.rx_byte |= 1 << self.rx_n;
                }
                self.rx_n += 1;
                if self.rx_n == 8 {
                    self.rx_state = 1;
                }
            }
            _ => {
                if b {
                    out.push(self.rx_byte);
                }
                self.rx_state = -1;
            }
        }
    }

    pub fn demodulate(&mut self, samples: &[f32]) -> Vec<u8> {
        let mut out = Vec::new();
        for &x in samples {
            let w = TAU * self.fc / FS * (self.t % 80_000) as f32;
            let z = (x * w.cos() * 2.0, -x * w.sin() * 2.0);
            let y = self.f2.push(self.f1.push(z));
            self.power += ((y.0 * y.0 + y.1 * y.1) - self.power) * 0.005;
            self.hist.push_back(y);
            self.t += 1;
            if self.hist.len() > 64 {
                self.hist.pop_front();
            }
            // symbol strobe (with Gardner timing correction)
            let now = self.t as f32 - 1.0;
            if now >= self.next {
                let ys = self.interp(self.next);
                let ymid = self.interp(self.next - self.sps / 2.0);
                if self.have_prev {
                    // Gardner: Re{(y_k - y_{k-1}) * conj(y_mid)}
                    let e = (ys.0 - self.prev.0) * ymid.0 + (ys.1 - self.prev.1) * ymid.1;
                    let norm = self.power.max(1e-6);
                    let adj = (-0.15 * e / norm).clamp(-0.6, 0.6);
                    // differential phase
                    let d = (ys.0 * self.prev.0 + ys.1 * self.prev.1, ys.1 * self.prev.0 - ys.0 * self.prev.1);
                    let ang = d.1.atan2(d.0);
                    let q = ((ang / (0.5 * PI)).round() as i32).rem_euclid(4);
                    let (q1, q2) = match q {
                        1 => (false, false),
                        0 => (false, true),
                        3 => (true, true),
                        _ => (true, false),
                    };
                    let b1 = self.descramble(q1);
                    let b2 = self.descramble(q2);
                    if self.power > 0.002 {
                        self.uart_bit(b1, &mut out);
                        self.uart_bit(b2, &mut out);
                    } else {
                        self.locked = false;
                        self.ones = 0;
                    }
                    self.next += self.sps + adj;
                } else {
                    self.next += self.sps;
                }
                self.prev = ys;
                self.have_prev = true;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::{ulaw_decode, ulaw_encode};

    fn link(data: &[u8], fc: f32) -> Vec<u8> {
        let mut tx = DpskTx::new(fc);
        let mut rx = DpskRx::new(fc);
        // scrambled ones first (training), then the data
        let mut got = Vec::new();
        let s = tx.modulate(8000);
        let s: Vec<f32> = s.iter().map(|x| ulaw_decode(ulaw_encode(*x))).collect();
        got.extend(rx.demodulate(&s));
        for b in data {
            tx.send_byte(*b);
        }
        for _ in 0..40 {
            let s = tx.modulate(400);
            let s: Vec<f32> = s.iter().map(|x| ulaw_decode(ulaw_encode(*x))).collect();
            got.extend(rx.demodulate(&s));
        }
        got
    }

    #[test]
    fn offset_and_noise() {
        use rand::Rng;
        let mut r = rand::thread_rng();
        let mut tx = DpskTx::new(2406.0);
        let mut rx = DpskRx::new(2400.0);
        let data = [0x06u8, 0x07, 0x02, 0xA3, 0x03, 0x34, 0x00, 0xC5];
        let mut got = Vec::new();
        let mut run = |tx: &mut DpskTx, rx: &mut DpskRx, n: usize, got: &mut Vec<u8>| {
            let s: Vec<f32> = tx.modulate(n).iter().map(|x| ulaw_decode(ulaw_encode(x * 0.4 + r.gen_range(-0.01..0.01)))).collect();
            got.extend(rx.demodulate(&s));
        };
        run(&mut tx, &mut rx, 8000, &mut got);
        for b in data {
            tx.send_byte(b);
        }
        for _ in 0..40 {
            run(&mut tx, &mut rx, 400, &mut got);
        }
        assert_eq!(got, data.to_vec());
    }

    #[test]
    fn loop_answer_band() {
        let data = [0x02u8, 0x05, 0x02, 0x07, 0x00, 0x03, 0x90, 0xA3, 0x55, 0xFF, 0x00];
        assert_eq!(link(&data, 2400.0), data.to_vec());
    }

    #[test]
    fn loop_originate_band() {
        let data = [0x02u8, 0x09, 0x09, 0x09, 0x09, 0x03, 0x06, 0x15, 0x0B];
        assert_eq!(link(&data, 1200.0), data.to_vec());
    }
}
