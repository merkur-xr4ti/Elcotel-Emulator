//! MC34115-style CVSD (continuously variable slope delta) decoder.
//! The voice EPROM and the firmware's DMA stream are one bit per sample,
//! least significant bit first, at roughly 19.2 kbit/s.

#[derive(Clone, Debug)]
pub struct Cvsd {
    integ: f32,
    step: f32,
    hist: u8,
    lp1: f32,
    lp2: f32,
    hp_prev_in: f32,
    hp_prev_out: f32,
}

impl Default for Cvsd {
    fn default() -> Self {
        Self::new()
    }
}

const STEP_MIN: f32 = 0.006;
const STEP_MAX: f32 = 0.22;
const STEP_DECAY: f32 = 0.985;
const STEP_GAIN: f32 = 0.018;
const LEAK: f32 = 0.990;

impl Cvsd {
    pub fn new() -> Self {
        Cvsd { integ: 0.0, step: STEP_MIN, hist: 0, lp1: 0.0, lp2: 0.0, hp_prev_in: 0.0, hp_prev_out: 0.0 }
    }

    /// Decode one bit; returns an audio sample in -1..1 at the bit rate.
    pub fn bit(&mut self, b: bool) -> f32 {
        self.hist = ((self.hist << 1) | b as u8) & 0x07;
        // Three equal bits in a row: the syllabic filter raises the step.
        if self.hist == 0 || self.hist == 7 {
            self.step = (self.step + STEP_GAIN).min(STEP_MAX);
        } else {
            self.step = (self.step * STEP_DECAY).max(STEP_MIN);
        }
        self.integ = self.integ * LEAK + if b { self.step } else { -self.step };
        self.integ = self.integ.clamp(-1.5, 1.5);
        // two-pole low pass (~3.4 kHz at 19.2 k)
        self.lp1 += 0.55 * (self.integ - self.lp1);
        self.lp2 += 0.55 * (self.lp1 - self.lp2);
        // DC block
        let out = self.lp2 - self.hp_prev_in + 0.995 * self.hp_prev_out;
        self.hp_prev_in = self.lp2;
        self.hp_prev_out = out;
        (out * 0.9).clamp(-1.0, 1.0)
    }

    /// Decode a byte (LSB first), appending samples.
    pub fn byte(&mut self, v: u8, out: &mut Vec<f32>) {
        for i in 0..8 {
            out.push(self.bit(v >> i & 1 != 0));
        }
    }
}

/// Decode a stretch of the voice EPROM directly (for the prompt browser).
pub fn decode_bytes(data: &[u8]) -> Vec<f32> {
    let mut c = Cvsd::new();
    let mut out = Vec::with_capacity(data.len() * 8);
    for &b in data {
        c.byte(b, &mut out);
    }
    out
}

/// One entry of a voice EPROM page index: (start offset, length) in bytes.
pub fn voice_index(voice: &[u8], page: usize) -> Vec<(usize, usize)> {
    let base = page * 0x10000;
    let mut v = Vec::new();
    if voice.len() < base + 4 {
        return v;
    }
    let first = u16::from_le_bytes([voice[base], voice[base + 1]]) as usize;
    let n = (first / 4).min(128);
    for i in 0..n {
        let o = base + i * 4;
        let start = u16::from_le_bytes([voice[o], voice[o + 1]]) as usize;
        let len = u16::from_le_bytes([voice[o + 2], voice[o + 3]]) as usize;
        if start == 0 && len == 0 {
            break;
        }
        v.push((base + start, len));
    }
    v
}
