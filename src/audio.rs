//! Sound card: plays what the handset would hear (firmware voice prompts,
//! the line, local call-progress tones, the ringer) and records the
//! microphone for the line.

use crate::board::VOICE_BIT_RATE;
use crate::sip::CallAudio;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    None,
    Dial,
    Ringback,
    Busy,
    Reorder,
}

pub struct AudioShared {
    /// Firmware voice at VOICE_BIT_RATE.
    pub voice: VecDeque<f32>,
    /// Voice ROM browser playback (heard even with the handset down).
    pub preview: VecDeque<f32>,
    pub tone: Tone,
    /// Keypad DTMF heard in the handset while a key is held.
    pub key_tone: Option<char>,
    /// The phone's bell (incoming call, handset on hook).
    pub ringer: bool,
    /// Handset lifted: line and tones are audible.
    pub handset_up: bool,
    pub volume: f32,
    pub mic_on: bool,
    pub out_level: f32,
    pub mic_level: f32,
    pub device_name: String,
    pub error: Option<String>,
}

impl Default for AudioShared {
    fn default() -> Self {
        AudioShared {
            voice: VecDeque::new(),
            preview: VecDeque::new(),
            tone: Tone::None,
            key_tone: None,
            ringer: false,
            handset_up: false,
            volume: 0.8,
            mic_on: true,
            out_level: 0.0,
            mic_level: 0.0,
            device_name: String::new(),
            error: None,
        }
    }
}

pub struct Audio {
    _out: Option<cpal::Stream>,
    _inp: Option<cpal::Stream>,
}

impl Audio {
    pub fn start(shared: Arc<Mutex<AudioShared>>, call: Arc<Mutex<CallAudio>>) -> Audio {
        let host = cpal::default_host();
        let mut out_stream = None;
        let mut in_stream = None;
        if let Some(dev) = host.default_output_device() {
            if let Ok(cfg) = dev.default_output_config() {
                let rate = cfg.sample_rate().0 as f32;
                let ch = cfg.channels() as usize;
                shared.lock().unwrap().device_name = dev.name().unwrap_or_default();
                let sh = shared.clone();
                let cl = call.clone();
                let mut gen = Gen::new(rate);
                let err = |e| eprintln!("audio out: {}", e);
                let stream = match cfg.sample_format() {
                    cpal::SampleFormat::F32 => dev.build_output_stream(&cfg.config(), move |data: &mut [f32], _| gen.fill(data, ch, &sh, &cl), err, None),
                    cpal::SampleFormat::I16 => {
                        let mut tmp = Vec::new();
                        dev.build_output_stream(
                            &cfg.config(),
                            move |data: &mut [i16], _| {
                                tmp.resize(data.len(), 0.0);
                                gen.fill(&mut tmp, ch, &sh, &cl);
                                for (d, s) in data.iter_mut().zip(tmp.iter()) {
                                    *d = (s * 32767.0) as i16;
                                }
                            },
                            err,
                            None,
                        )
                    }
                    _ => {
                        let mut tmp = Vec::new();
                        dev.build_output_stream(
                            &cfg.config(),
                            move |data: &mut [u16], _| {
                                tmp.resize(data.len(), 0.0);
                                gen.fill(&mut tmp, ch, &sh, &cl);
                                for (d, s) in data.iter_mut().zip(tmp.iter()) {
                                    *d = ((s * 0.5 + 0.5) * 65535.0) as u16;
                                }
                            },
                            err,
                            None,
                        )
                    }
                };
                match stream {
                    Ok(s) => {
                        let _ = s.play();
                        out_stream = Some(s);
                    }
                    Err(e) => shared.lock().unwrap().error = Some(format!("Speaker: {}", e)),
                }
            }
        } else {
            shared.lock().unwrap().error = Some("No speaker found".into());
        }
        if let Some(dev) = host.default_input_device() {
            if let Ok(cfg) = dev.default_input_config() {
                let rate = cfg.sample_rate().0 as f32;
                let ch = cfg.channels() as usize;
                let sh = shared.clone();
                let cl = call.clone();
                let mut dec = Decim::new(rate);
                let err = |e| eprintln!("audio in: {}", e);
                let stream = match cfg.sample_format() {
                    cpal::SampleFormat::F32 => dev.build_input_stream(&cfg.config(), move |data: &[f32], _| dec.push(data, ch, &sh, &cl), err, None),
                    cpal::SampleFormat::I16 => dev.build_input_stream(
                        &cfg.config(),
                        move |data: &[i16], _| {
                            let v: Vec<f32> = data.iter().map(|x| *x as f32 / 32768.0).collect();
                            dec.push(&v, ch, &sh, &cl)
                        },
                        err,
                        None,
                    ),
                    _ => dev.build_input_stream(
                        &cfg.config(),
                        move |data: &[u16], _| {
                            let v: Vec<f32> = data.iter().map(|x| *x as f32 / 32768.0 - 1.0).collect();
                            dec.push(&v, ch, &sh, &cl)
                        },
                        err,
                        None,
                    ),
                };
                if let Ok(s) = stream {
                    let _ = s.play();
                    in_stream = Some(s);
                }
            }
        }
        Audio { _out: out_stream, _inp: in_stream }
    }
}

struct Gen {
    rate: f32,
    t: f64,
    voice_pos: f64,
    voice_last: f32,
    line_pos: f64,
    line_last: f32,
    voice_pos2: f64,
    preview_last: f32,
}

impl Gen {
    fn new(rate: f32) -> Self {
        Gen { rate, t: 0.0, voice_pos: 0.0, voice_last: 0.0, line_pos: 0.0, line_last: 0.0, voice_pos2: 0.0, preview_last: 0.0 }
    }

    fn fill(&mut self, data: &mut [f32], ch: usize, sh: &Arc<Mutex<AudioShared>>, cl: &Arc<Mutex<CallAudio>>) {
        let mut s = sh.lock().unwrap();
        let mut c = cl.lock().unwrap();
        let vstep = VOICE_BIT_RATE as f64 / self.rate as f64;
        let lstep = 8000.0 / self.rate as f64;
        let dt = 1.0 / self.rate as f64;
        let mut peak = 0f32;
        // keep latency bounded
        while s.voice.len() > VOICE_BIT_RATE as usize / 2 {
            s.voice.pop_front();
        }
        // In a data call the phone's modem owns the line audio: the speaker
        // must not take any of it (that cut holes in the caller's signal).
        let modem_call = c.modem;
        while !modem_call && c.from_line.len() > 4000 {
            c.from_line.pop_front();
        }
        for frame in data.chunks_mut(ch) {
            // voice prompts (resampled from 19.2 kHz, simple averaging)
            self.voice_pos += vstep;
            let mut acc = 0.0;
            let mut n = 0;
            while self.voice_pos >= 1.0 {
                self.voice_pos -= 1.0;
                if let Some(v) = s.voice.pop_front() {
                    acc += v;
                    n += 1;
                }
            }
            if n > 0 {
                self.voice_last = acc / n as f32;
            } else if s.voice.is_empty() {
                self.voice_last *= 0.95;
            }
            let mut x = if s.handset_up { self.voice_last * 0.9 } else { 0.0 };
            if self.voice_pos2 >= 0.0 {
                self.voice_pos2 += vstep;
                while self.voice_pos2 >= 1.0 {
                    self.voice_pos2 -= 1.0;
                    self.preview_last = s.preview.pop_front().unwrap_or(0.0);
                }
                x += self.preview_last * 0.9;
            }
            // line audio (8 kHz)
            self.line_pos += lstep;
            while self.line_pos >= 1.0 {
                self.line_pos -= 1.0;
                if let Some(v) = speaker_take(&mut c) {
                    self.line_last = v;
                } else if modem_call {
                    self.line_last = 0.0;
                }
            }
            if c.active && s.handset_up && !c.modem {
                x += self.line_last;
            }
            // call progress tones (North American precise tones)
            let t = self.t;
            let tw = |f: f64| (t * f * std::f64::consts::TAU).sin() as f32;
            if s.handset_up {
                x += match s.tone {
                    Tone::None => 0.0,
                    Tone::Dial => 0.12 * (tw(350.0) + tw(440.0)),
                    Tone::Ringback => {
                        if t % 6.0 < 2.0 {
                            0.12 * (tw(440.0) + tw(480.0))
                        } else {
                            0.0
                        }
                    }
                    Tone::Busy => {
                        if t % 1.0 < 0.5 {
                            0.12 * (tw(480.0) + tw(620.0))
                        } else {
                            0.0
                        }
                    }
                    Tone::Reorder => {
                        if t % 0.5 < 0.25 {
                            0.12 * (tw(480.0) + tw(620.0))
                        } else {
                            0.0
                        }
                    }
                };
            }
            if s.handset_up {
                if let Some(k) = s.key_tone {
                    let (lo, hi) = dtmf_freqs(k);
                    x += 0.10 * (tw(lo) + tw(hi));
                }
            }
            if s.ringer && t % 6.0 < 2.0 {
                // piezo warble
                let warble = if (t * 20.0) as i64 % 2 == 0 { 1100.0 } else { 1400.0 };
                x += 0.10 * tw(warble).signum();
            }
            x *= s.volume;
            peak = peak.max(x.abs());
            let x = x.clamp(-1.0, 1.0);
            for o in frame.iter_mut() {
                *o = x;
            }
            self.t += dt;
            if self.t > 600.0 {
                self.t -= 600.0;
            }
        }
        s.out_level = s.out_level * 0.8 + peak * 0.2;
    }
}

struct Decim {
    step: f64,
    pos: f64,
    acc: f32,
    n: u32,
}

impl Decim {
    fn new(rate: f32) -> Self {
        Decim { step: 8000.0 / rate as f64, pos: 0.0, acc: 0.0, n: 0 }
    }
    fn push(&mut self, data: &[f32], ch: usize, sh: &Arc<Mutex<AudioShared>>, cl: &Arc<Mutex<CallAudio>>) {
        let (on, up) = {
            let s = sh.lock().unwrap();
            (s.mic_on, s.handset_up)
        };
        let mut c = cl.lock().unwrap();
        let mut peak = 0f32;
        for frame in data.chunks(ch) {
            let v = frame[0];
            peak = peak.max(v.abs());
            self.acc += v;
            self.n += 1;
            self.pos += self.step;
            if self.pos >= 1.0 {
                self.pos -= 1.0;
                let s = self.acc / self.n.max(1) as f32;
                self.acc = 0.0;
                self.n = 0;
                if c.active && on && up && !c.modem {
                    c.to_line.push_back(s);
                }
            }
        }
        drop(c);
        let mut s = sh.lock().unwrap();
        s.mic_level = s.mic_level * 0.8 + peak * 0.2;
    }
}

/// DTMF row/column frequencies for a keypad key.
fn dtmf_freqs(k: char) -> (f64, f64) {
    let (row, col) = match k {
        '1' => (0, 0),
        '2' => (0, 1),
        '3' => (0, 2),
        '4' => (1, 0),
        '5' => (1, 1),
        '6' => (1, 2),
        '7' => (2, 0),
        '8' => (2, 1),
        '9' => (2, 2),
        '*' => (3, 0),
        '0' => (3, 1),
        _ => (3, 2),
    };
    ([697.0, 770.0, 852.0, 941.0][row], [1209.0, 1336.0, 1477.0][col])
}

/// One 8 kHz sample of line audio for the speaker. During a data call the
/// phone's modem owns the incoming audio, so the speaker takes nothing.
pub fn speaker_take(c: &mut CallAudio) -> Option<f32> {
    if c.modem {
        None
    } else {
        c.from_line.pop_front()
    }
}
