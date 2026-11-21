//! Runs the firmware in real time and stands in for the telephone line:
//! the handset, keypad and coins go to the board; the number the firmware
//! dials out goes to the SIP line; incoming SIP calls ring the phone.

use crate::audio::{AudioShared, Tone};
use crate::board::{Event, PayBoard, CPU_HZ};
use crate::machine::Machine;
use crate::sip::{SipCmd, SipEvent};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineState {
    Idle,
    DialTone,
    Collecting,
    Calling,
    Talking,
    Failed,
    IncomingRinging,
    /// The firmware's modem answered an incoming call (PNM programming).
    ModemCall,
}

impl LineState {
    pub fn label(&self) -> &'static str {
        match self {
            LineState::Idle => "Idle",
            LineState::DialTone => "Dial tone",
            LineState::Collecting => "Dialling",
            LineState::Calling => "Calling",
            LineState::Talking => "Connected",
            LineState::Failed => "Call ended",
            LineState::IncomingRinging => "Incoming call",
            LineState::ModemCall => "Data call (phone's modem)",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub t: f64,
    pub text: String,
}

pub struct Phone {
    pub m: Machine,
    pub sip_tx: Sender<SipCmd>,
    sip_rx: Receiver<SipEvent>,
    pub audio: Arc<Mutex<AudioShared>>,
    pub line: LineState,
    pub dialed: String,
    /// Keys pressed since the handset was lifted.
    pub typed: String,
    /// Digits of the current fast burst from the firmware.
    burst: String,
    pub call_number: String,
    last_digit_t: f64,
    pub sip_status: String,
    pub sip_registered: bool,
    pub sip_enabled: bool,
    pub sip_log: VecDeque<String>,
    pub log: VecDeque<LogLine>,
    pub prompts: VecDeque<(f64, u8)>,
    pub current_prompt: Option<u8>,
    pub incoming_from: Option<String>,
    pub ring_firmware: bool,
    pub paused: bool,
    pub speed: f32,
    pub dial_timeout: f64,
    pub nvram_path: Option<PathBuf>,
    last_save: Instant,
    pub defaulting: Option<(f64, u8)>,
    pub coins_total_cents: u32,
    pub escrow_cents: u32,
    pub cashbox_cents: u32,
    pub returned_cents: u32,
    pub cpu_load: f32,
    /// Virtual PNM session downloading a rate file over the phone's modem.
    pub pnm: Option<crate::pnm::PnmSession>,
    pub pnm_log: Vec<String>,
    pub pnm_status: String,
    /// Some(true) after a download the phone acknowledged completely.
    pub pnm_result: Option<bool>,
    pending_import: Option<Vec<u8>>,
    /// Line audio of the SIP call (set by main), for data calls.
    pub call_audio: Option<Arc<Mutex<crate::sip::CallAudio>>>,
    pub modem: Option<crate::modem::LineModem>,
    /// Answer incoming internet calls at once and play ringback to the caller.
    /// Text log of the line (notes and SIP), for troubleshooting.
    pub log_path: Option<PathBuf>,
    modem_mode_noted: bool,
    modem_samples: f64,
    modem_rx_log: Vec<u8>,
    /// Bytes from the caller whose top bit looks like even / odd parity, and all bytes.
    parity_stats: (u32, u32, u32, u32),
    parity_warned: bool,
    modem_tx_log: Vec<u8>,
    modem_log_t: f64,
    /// Scripted telemetry 998 (custom firmware presets).
    pub presets: Option<(f64, u8)>,
    /// The service code the presets script dials after entering telemetry
    /// (998 = built-in rates; 969 = store RAM settings in the EEPROM).
    pub service_code: String,
    /// A "store settings in the EEPROM" call is waiting / running.
    pub pending_store: bool,
    pub pnm_store_only: bool,
    /// The last probed command: (command, reply bytes, session ended cleanly).
    pub last_probe: Option<(u8, Vec<u8>, bool)>,
    /// Load presets automatically after a factory default.
    pub auto_presets: bool,
}

/// How long to wait after the last digit the firmware sent before placing
/// the call. The firmware trickles digits out every 2.5 s while it waits for
/// coins and holds the last ones until it is paid, so incomplete North
/// American numbers wait indefinitely; anything else uses the timeout.
fn dial_wait(n: &str, timeout: f64) -> Option<f64> {
    if n.is_empty() {
        return None;
    }
    if !n.chars().all(|c| c.is_ascii_digit()) {
        return Some(timeout);
    }
    let len = n.len();
    match n.as_bytes()[0] {
        b'1' => {
            if len >= 11 {
                Some(1.5)
            } else {
                None
            }
        }
        b'0' => {
            if len == 1 || n.starts_with("011") {
                Some(timeout)
            } else if len >= 11 {
                Some(1.5)
            } else {
                None
            }
        }
        _ => {
            if len == 3 && &n[1..] == "11" {
                Some(2.0)
            } else if len < 7 {
                None
            } else if len == 7 {
                // could still become a 10-digit number
                Some(3.5)
            } else if len < 10 {
                None
            } else {
                Some(1.5)
            }
        }
    }
}

impl Phone {
    pub fn new(board: PayBoard, sip_tx: Sender<SipCmd>, sip_rx: Receiver<SipEvent>, audio: Arc<Mutex<AudioShared>>, nvram_path: Option<PathBuf>) -> Phone {
        let mut p = Phone {
            m: Machine::new(board),
            sip_tx,
            sip_rx,
            audio,
            line: LineState::Idle,
            dialed: String::new(),
            typed: String::new(),
            burst: String::new(),
            call_number: String::new(),
            last_digit_t: 0.0,
            sip_status: "Internet calling is off".into(),
            sip_registered: false,
            sip_enabled: false,
            sip_log: VecDeque::new(),
            log: VecDeque::new(),
            prompts: VecDeque::new(),
            current_prompt: None,
            incoming_from: None,
            ring_firmware: false,
            paused: false,
            speed: 1.0,
            dial_timeout: 5.0,
            nvram_path,
            last_save: Instant::now(),
            defaulting: None,
            coins_total_cents: 0,
            escrow_cents: 0,
            cashbox_cents: 0,
            returned_cents: 0,
            cpu_load: 0.0,
            pnm: None,
            pnm_log: Vec::new(),
            pnm_status: String::new(),
            pnm_result: None,
            pending_import: None,
            call_audio: None,
            modem: None,
            modem_mode_noted: false,
            log_path: None,
            modem_samples: 0.0,
            modem_rx_log: Vec::new(),
            parity_stats: (0, 0, 0, 0),
            parity_warned: false,
            modem_tx_log: Vec::new(),
            modem_log_t: 0.0,
            presets: None,
            service_code: "998".into(),
            pending_store: false,
            pnm_store_only: false,
            last_probe: None,
            auto_presets: true,
        };
        let first_run = !p.load_nvram();
        if first_run && is_compiled(&p.m.b.rom) {
            p.note(format!("Running the BASIC program {}", p.firmware_version()));
        } else if first_run {
            p.note("First start: resetting the phone to factory defaults (program button + handset), as on a new board");
            p.start_factory_default();
        }
        p
    }

    pub fn note(&mut self, s: impl Into<String>) {
        let t = self.m.seconds();
        let text: String = s.into();
        self.write_log(&format!("{:9.2}s  {}", t, text));
        self.log.push_back(LogLine { t, text });
        while self.log.len() > 400 {
            self.log.pop_front();
        }
    }

    // ---------- persistence (battery RAM + EEPROM) ----------
    fn load_nvram(&mut self) -> bool {
        let Some(p) = &self.nvram_path else { return false };
        let Ok(d) = std::fs::read(p) else { return false };
        if d.len() != 0x10000 + 0x2000 + 0x8000 {
            return false;
        }
        self.m.b.ram.copy_from_slice(&d[..0x10000]);
        self.m.b.eeprom.copy_from_slice(&d[0x10000..0x12000]);
        self.m.b.ram_lo.copy_from_slice(&d[0x12000..]);
        true
    }

    pub fn save_nvram(&mut self) {
        let Some(p) = &self.nvram_path else { return };
        let mut d = Vec::with_capacity(0x1A000);
        d.extend_from_slice(&self.m.b.ram);
        d.extend_from_slice(&self.m.b.eeprom);
        d.extend_from_slice(&self.m.b.ram_lo);
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, d);
        self.last_save = Instant::now();
    }

    pub fn wipe_memory(&mut self) {
        self.m.b.ram.iter_mut().for_each(|b| *b = 0);
        self.m.b.ram_lo.iter_mut().for_each(|b| *b = 0);
        self.m.b.eeprom.iter_mut().for_each(|b| *b = 0xFF);
        self.m.reset();
        self.note("Memory cleared (battery and EEPROM removed); power-cycled");
    }

    /// The documented procedure: on-hook, hold the program button, lift the
    /// handset, wait for the relay, let go, hang up.
    pub fn start_factory_default(&mut self) {
        self.defaulting = Some((self.m.seconds(), 0));
    }

    fn run_defaulting(&mut self) {
        let Some((t0, step)) = self.defaulting else { return };
        let t = self.m.seconds() - t0;
        match step {
            0 => {
                self.set_hook(true);
                self.m.b.program_button = true;
                self.defaulting = Some((t0, 1));
            }
            1 if t > 1.0 => {
                self.set_hook(false);
                self.defaulting = Some((t0, 2));
            }
            2 if t > 11.0 => {
                self.m.b.program_button = false;
                self.defaulting = Some((t0, 3));
            }
            3 if t > 13.0 => {
                self.set_hook(true);
                self.defaulting = None;
                self.note("Factory default finished");
                if self.auto_presets && self.has_presets() {
                    self.start_presets();
                }
            }
            _ => {}
        }
    }

    // ---------- person actions ----------
    pub fn set_hook(&mut self, on_hook: bool) {
        if self.m.b.on_hook() == on_hook {
            return;
        }
        self.m.b.set_hook(on_hook);
        // line voltage seen by the board: ~48 V idle, ~8 V with loop current
        self.m.b.inputs[0x41] = if on_hook { 0x60 } else { 0x14 };
        self.audio.lock().unwrap().handset_up = !on_hook;
        if on_hook {
            self.note("Handset hung up");
            self.typed.clear();
            if matches!(self.line, LineState::Calling | LineState::Talking | LineState::IncomingRinging) && self.line != LineState::IncomingRinging {
                let _ = self.sip_tx.send(SipCmd::Hangup);
            }
            if self.line != LineState::IncomingRinging {
                self.line = LineState::Idle;
            }
            self.dialed.clear();
            self.set_tone(Tone::None);
        } else {
            self.note("Handset lifted");
            if self.line == LineState::IncomingRinging {
                let _ = self.sip_tx.send(SipCmd::Answer);
                self.stop_ringing();
                self.line = LineState::Talking;
                self.note("Answered the incoming call");
            } else {
                self.line = LineState::DialTone;
                self.dialed.clear();
                self.typed.clear();
                self.set_tone(Tone::Dial);
            }
        }
    }

    fn key_started(&mut self, c: char) {
        if !self.m.b.on_hook() {
            self.typed.push(c);
            if self.typed.len() > 32 {
                self.typed.remove(0);
            }
            // a real line drops dial tone at the first digit
            if self.line == LineState::DialTone {
                self.set_tone(Tone::None);
            }
        }
    }

    /// A quick tap (typed digits).
    pub fn press_key(&mut self, c: char) {
        self.key_started(c);
        self.m.b.press_key(c);
    }

    /// Key pushed down (mouse or keyboard); release with key_release.
    pub fn key_hold(&mut self, c: char) -> u32 {
        self.key_started(c);
        self.m.b.key_hold(c)
    }

    pub fn key_release(&mut self, id: u32) {
        self.m.b.key_release(id);
    }

    pub fn drop_coin(&mut self, which: usize) {
        self.m.b.drop_coin(which);
        self.coins_total_cents += [5, 10, 25][which.min(2)];
        self.escrow_cents += [5, 10, 25][which.min(2)];
        self.note(format!("Coin dropped: {}", ["nickel (5¢)", "dime (10¢)", "quarter (25¢)"][which.min(2)]));
    }

    fn set_tone(&mut self, t: Tone) {
        self.audio.lock().unwrap().tone = t;
    }

    fn stop_ringing(&mut self) {
        self.m.b.ring_signal = false;
        self.audio.lock().unwrap().ringer = false;
        self.incoming_from = None;
    }

    // ---------- the clock ----------
    /// Advance by `real` seconds of wall time.
    pub fn advance(&mut self, real: f64) {
        self.run_defaulting();
        self.run_presets();
        self.start_pending_import();
        if !self.paused {
            let cycles = (real * self.speed as f64 * CPU_HZ as f64) as u64;
            self.m.run(cycles);
        }
        // keypad tone in the handset while a key is down
        let kd = self.m.b.key_down();
        {
            let mut a = self.audio.lock().unwrap();
            if a.key_tone != kd {
                a.key_tone = kd;
            }
        }
        // voice to the speaker
        if !self.m.b.voice_pcm.is_empty() {
            let mut a = self.audio.lock().unwrap();
            a.voice.extend(self.m.b.voice_pcm.drain(..));
            let excess = a.voice.len().saturating_sub(crate::board::VOICE_BIT_RATE as usize * 2);
            a.voice.drain(..excess);
        }
        self.pump_modem(real);
        let evs: Vec<Event> = std::mem::take(&mut self.m.b.events);
        for e in evs {
            self.board_event(e);
        }
        while let Ok(e) = self.sip_rx.try_recv() {
            self.sip_event(e);
        }
        self.line_logic();
        self.tick_pnm();
        if self.last_save.elapsed() > Duration::from_secs(20) {
            self.save_nvram();
        }
    }

    fn board_event(&mut self, e: Event) {
        let t = self.m.seconds();
        match e {
            Event::Prompt(p) => {
                self.prompts.push_back((t, p));
                while self.prompts.len() > 60 {
                    self.prompts.pop_front();
                }
                self.current_prompt = Some(p);
            }
            Event::DtmfOut(c) => {
                if self.line != LineState::ModemCall {
                    self.note(format!("Firmware dials digit {}", c));
                }
                match self.line {
                    LineState::DialTone | LineState::Collecting | LineState::Failed if !self.m.b.on_hook() => {
                        if self.line == LineState::Failed {
                            self.dialed.clear();
                            self.burst.clear();
                        }
                        // After payment the firmware may send the whole number again,
                        // reformatted by the rate file (e.g. 299-5551 -> 517-299-5551),
                        // as one fast burst. A burst that is a complete 10/11-digit
                        // number replaces what it trickled out before.
                        if t - self.last_digit_t > 0.45 {
                            self.burst.clear();
                        }
                        self.burst.push(c);
                        self.dialed.push(c);
                        // keep the digits sent before if, with the burst, they make a
                        // complete number (1 + 800...), else use the burst alone
                        if complete_number(&self.burst) && !complete_number(&self.dialed) {
                            self.dialed = self.burst.clone();
                        }
                        self.last_digit_t = t;
                        self.line = LineState::Collecting;
                        self.set_tone(Tone::None);
                    }
                    LineState::Calling | LineState::Talking => {
                        let _ = self.sip_tx.send(SipCmd::Dtmf(c));
                    }
                    _ => {}
                }
            }
            Event::CoinRelay(_) if self.escrow_cents == 0 => {}
            Event::CoinRelay(collect) => {
                if collect {
                    self.cashbox_cents += self.escrow_cents;
                    self.note(format!("Coin relay: {} cents collected into the cash box", self.escrow_cents));
                } else {
                    self.returned_cents += self.escrow_cents;
                    self.note(format!("Coin relay: {} cents returned to the coin return", self.escrow_cents));
                }
                self.escrow_cents = 0;
                // Coins are only settled at the end of a call. Collected while
                // the handset is still up and the call is live means the paid
                // time ran out without another deposit: the firmware has cut
                // the call off (and resets for a new one), so end it on the
                // internet side too and give dial tone, as the line would.
                if collect && !self.m.b.on_hook() && matches!(self.line, LineState::Talking | LineState::Calling) {
                    self.note("Paid time is up and no more coins were added: the phone cut the call off");
                    let _ = self.sip_tx.send(SipCmd::Hangup);
                    self.line = LineState::DialTone;
                    self.dialed.clear();
                    self.burst.clear();
                    self.set_tone(Tone::Dial);
                }
            }
            Event::LineSeized(on) => {
                if on && self.m.b.on_hook() && self.line == LineState::IncomingRinging {
                    self.start_modem_call();
                } else if on && self.m.b.on_hook() && self.m.b.ring_signal && self.pnm.is_none() {
                    self.note("The phone's modem answered the ringing line (remote programming mode)");
                } else if !on && self.line == LineState::ModemCall {
                    self.note("The phone's modem hung up the data call");
                    let _ = self.sip_tx.send(SipCmd::Hangup);
                    self.end_modem_call();
                }
            }
        }
    }

    fn line_logic(&mut self) {
        let t = self.m.seconds();
        if self.line == LineState::Collecting && !self.dialed.is_empty() {
            let quiet = t - self.last_digit_t;
            let wait = dial_wait(&self.dialed, self.dial_timeout);
            if wait.map(|w| quiet >= w).unwrap_or(false) {
                self.call_number = self.dialed.clone();
                if self.sip_enabled {
                    self.note(format!("Placing internet call to {}", self.call_number));
                    let _ = self.sip_tx.send(SipCmd::Dial(self.call_number.clone()));
                    self.line = LineState::Calling;
                    self.set_tone(Tone::Ringback);
                } else {
                    self.note(format!("Number {} dialled, but internet calling is off", self.call_number));
                    self.line = LineState::Failed;
                    self.set_tone(Tone::Reorder);
                }
            }
        }
        // the voice prompt label fades once the codec idles
        if self.m.z.io.dstat & 0x80 == 0 && self.audio.lock().unwrap().voice.is_empty() {
            self.current_prompt = None;
        }
    }

    fn sip_event(&mut self, e: SipEvent) {
        match e {
            SipEvent::Status(s) => {
                self.sip_status = s.clone();
                self.note(format!("Internet line: {}", s));
            }
            SipEvent::Registered(r) => self.sip_registered = r,
            SipEvent::Log(l) => {
                self.write_log(&format!("           SIP {}", l));
                self.sip_log.push_back(l);
                while self.sip_log.len() > 300 {
                    self.sip_log.pop_front();
                }
            }
            SipEvent::Ringing => {
                if self.line == LineState::Calling {
                    self.set_tone(Tone::Ringback);
                }
            }
            SipEvent::EarlyMedia => {
                if self.line == LineState::Calling {
                    self.set_tone(Tone::None);
                }
            }
            SipEvent::Answered => {
                if self.line == LineState::ModemCall {
                    return;
                }
                self.set_tone(Tone::None);
                if self.line == LineState::Calling && !self.m.b.on_hook() {
                    // answer supervision: the line reverses polarity
                    self.m.b.line_reversed = true;
                }
                self.line = LineState::Talking;
                self.note("Call connected");
            }
            SipEvent::Ended(why) => {
                if self.line == LineState::ModemCall {
                    self.note(format!("Data call ended: {}", why));
                    self.end_modem_call();
                } else if self.line == LineState::IncomingRinging {
                    self.stop_ringing();
                    self.line = LineState::Idle;
                    self.note(format!("Incoming call ended: {}", why));
                } else if matches!(self.line, LineState::DialTone | LineState::Collecting) {
                    // our own hang-up (the phone cut the call off): the
                    // caller already has dial tone for a new call
                } else if !self.m.b.on_hook() {
                    if self.line == LineState::Talking {
                        // the far end hung up: the line goes dead, and the
                        // firmware ends the call (settles coins, drops the line)
                        self.m.b.loop_open = true;
                    }
                    self.line = LineState::Failed;
                    let busy = why.contains("486") || why.to_lowercase().contains("busy");
                    self.set_tone(if busy { Tone::Busy } else { Tone::Reorder });
                    self.note(format!("Call ended: {}", why));
                } else {
                    self.line = LineState::Idle;
                }
            }
            SipEvent::Incoming(from) => {
                if self.m.b.on_hook() && self.line == LineState::Idle && self.pnm.is_none() {
                    self.line = LineState::IncomingRinging;
                    self.incoming_from = Some(from.clone());
                    self.audio.lock().unwrap().ringer = true;
                    if self.ring_firmware {
                        self.m.b.ring_signal = true;
                    }
                    self.note(format!("Incoming call from {} - lift the handset to answer", from));
                } else {
                    let _ = self.sip_tx.send(SipCmd::Hangup);
                }
            }
        }
    }
}

/// Real-time driver thread.
pub fn spawn_runner(phone: Arc<Mutex<Phone>>) {
    std::thread::spawn(move || {
        let slice = Duration::from_millis(10);
        let mut last = Instant::now();
        loop {
            let start = Instant::now();
            let real = (start - last).as_secs_f64().min(0.05);
            last = start;
            {
                let mut p = phone.lock().unwrap();
                p.advance(real);
                let used = start.elapsed().as_secs_f32() / slice.as_secs_f32();
                p.cpu_load = p.cpu_load * 0.95 + used * 0.05;
            }
            let el = start.elapsed();
            if el < slice {
                std::thread::sleep(slice - el);
            }
        }
    });
}

impl Phone {
    /// Queue a rate file download, done the way PNM Plus does it: call the
    /// phone's modem, log on and send the file with command 0xA3. It starts
    /// once the phone is idle with the handset on hook.
    pub fn import_rates(&mut self, file: Vec<u8>) {
        if self.pnm.is_some() {
            return;
        }
        self.pnm_result = None;
        self.pnm_status = "waiting for the phone to be idle and on hook".into();
        self.pending_import = Some(file);
    }

    pub fn importing(&self) -> bool {
        self.pnm.is_some() || self.pending_import.is_some() || self.pending_store
    }

    /// Test: call the phone as PNM would and send it one command byte
    /// (coverage runs). The reply ends up in `last_probe`.
    pub fn start_pnm_probe(&mut self, cmd: u8) -> bool {
        if self.pnm.is_some() || !self.m.b.on_hook() || self.line != LineState::Idle || self.defaulting.is_some() {
            return false;
        }
        let (mut id, pwd) = (self.station_id(), self.pnm_password());
        if id.contains('?') {
            id = "9999".into();
        }
        self.last_probe = None;
        self.pnm_result = None;
        self.pnm = Some(self.with_ani(crate::pnm::PnmSession::probe(cmd, &id, &pwd, self.m.seconds())));
        true
    }

    /// Call the phone as PNM would and have it store its battery-RAM settings
    /// in the EEPROM and reload them (after changing an option here).
    pub fn store_settings(&mut self) {
        if self.pnm.is_none() {
            self.pending_store = true;
            self.pnm_result = None;
            self.pnm_status = "waiting for the phone to be idle and on hook".into();
        }
    }

    fn start_pending_import(&mut self) {
        if (self.pending_import.is_none() && !self.pending_store) || self.pnm.is_some() || self.defaulting.is_some() {
            return;
        }
        if !self.m.b.on_hook() || self.line != LineState::Idle {
            return;
        }
        if self.pending_import.is_none() {
            self.pending_store = false;
            self.pnm_store_only = true;
            self.pnm_log.clear();
            let (mut id, pwd) = (self.station_id(), self.pnm_password());
            if id.contains('?') {
                id = "9999".into();
            }
            self.note(format!("Calling the phone's modem as PNM would (station id {}) to store its settings in the EEPROM", id));
            self.pnm = Some(self.with_ani(crate::pnm::PnmSession::store_settings(&id, &pwd, self.m.seconds())));
            return;
        }
        self.pnm_store_only = false;
        let file = self.pending_import.take().unwrap();
        let blocks = crate::pnm::rate_blocks(&file).len();
        self.note(format!("Rate file download started: calling the phone's modem as PNM Plus would ({} data blocks)", blocks));
        self.pnm_log.clear();
        let (mut id, pwd) = (self.station_id(), self.pnm_password());
        if id.contains('?') {
            id = "9999".into();
        }
        self.note(format!("Logging on as PNM with station id {}", id));
        self.pnm = Some(self.with_ani(crate::pnm::PnmSession::with_password(file, &id, &pwd, self.m.seconds())));
    }

    fn tick_pnm(&mut self) {
        let now = self.m.seconds();
        let Some(mut s) = self.pnm.take() else { return };
        let before = s.log.len();
        s.tick(&mut self.m.b, now);
        let new: Vec<String> = s.log[before..].to_vec();
        self.pnm_log.extend(new);
        self.pnm_status = s.status();
        match s.finished {
            Some(ok) if s.probe_cmd.is_some() => {
                self.pnm_result = Some(ok);
                self.last_probe = Some((s.probe_cmd.unwrap_or(0), s.probe_reply.clone(), ok));
            }
            Some(ok) => {
                self.pnm_result = Some(ok);
                if ok && self.pnm_store_only {
                    self.pnm_status = "settings stored in the EEPROM".into();
                    self.note(if s.stored {
                        "Settings stored: the phone wrote them to its EEPROM and reloaded them"
                    } else {
                        "The phone took the call but did not confirm storing its settings"
                    });
                    self.save_nvram();
                } else if ok {
                    self.pnm_status = "rate file loaded into the phone".into();
                    if s.stored {
                        self.note("Rate file installed: the phone stored it in its EEPROM and now prices calls from it");
                    } else {
                        self.note("Rate file download finished: the phone acknowledged every block (kept in battery RAM only)");
                    }
                    self.save_nvram();
                } else {
                    self.pnm_status = format!("download failed ({})", s.log.last().cloned().unwrap_or_default());
                    self.note("Rate file download failed - see the Rate file tab");
                }
            }
            None => self.pnm = Some(s),
        }
    }
}

/// "PCM 05.09.00" style version text from a program ROM (at 0x98).
pub fn rom_version(rom: &[u8]) -> String {
    if rom.len() < 0xB0 {
        return String::new();
    }
    let s: String = rom[0x98..0xA4].iter().map(|&b| if (32..127).contains(&b) { b as char } else { ' ' }).collect();
    if is_compiled(rom) {
        // an ELCOMPILER program: its name follows at 00A4
        let name: String = rom[0xA4..0x100].iter().take_while(|&&b| b != 0).map(|&b| if (32..127).contains(&b) { b as char } else { ' ' }).collect();
        return format!("ELCOMPILER: {}", name.trim());
    }
    s.trim().to_string()
}

/// A program ROM made by ELCOMPILER (BASIC) rather than Elcotel firmware.
pub fn is_compiled(rom: &[u8]) -> bool {
    rom.len() >= 0x100 && &rom[0x98..0xA2] == b"ELCOMPILER"
}

impl Phone {
    pub fn firmware_version(&self) -> String {
        rom_version(&self.m.b.rom)
    }

    /// Custom 5.9.0 firmware carries rates and options in the ROM; telemetry
    /// code 998 installs them.
    pub fn has_presets(&self) -> bool {
        self.firmware_version().starts_with("PCM 05.09")
    }

    /// Swap the program ROM: the old board's memory is saved, the new one
    /// gets its own battery RAM / EEPROM file and is reset to factory
    /// defaults (and its presets loaded) the first time.
    /// Use another voice ROM (128 KB image) from now on.
    pub fn swap_voice(&mut self, voice: Vec<u8>) {
        self.m.b.voice = voice;
        self.note("Voice ROM changed");
    }

    pub fn swap_firmware(&mut self, rom: Vec<u8>, nvram_path: Option<PathBuf>) {
        if self.pnm.is_some() {
            return;
        }
        self.save_nvram();
        let voice = self.m.b.voice.clone();
        let mut b = PayBoard::new(rom, voice);
        b.ring_bit = self.m.b.ring_bit;
        if self.m.b.mem_seen.is_some() {
            b.mem_seen = Some(vec![0; 0x40000]);
        }
        if self.m.b.mem_age.is_some() {
            b.mem_age = Some(crate::board::MemAge::new());
        }
        let profiling = self.m.pc_hist.is_some();
        self.m = Machine::new(b);
        if profiling {
            self.m.pc_hist = Some(vec![0; 65536]);
        }
        self.nvram_path = nvram_path;
        self.line = LineState::Idle;
        self.dialed.clear();
        self.typed.clear();
        self.defaulting = None;
        self.presets = None;
        self.pnm_result = None;
        self.pending_import = None;
        self.audio.lock().unwrap().handset_up = false;
        self.audio.lock().unwrap().tone = Tone::None;
        let v = self.firmware_version();
        self.m.b.serial_out[1].clear();
        if self.load_nvram() {
            self.note(format!("Switched to firmware {} (its saved memory restored)", v));
        } else if is_compiled(&self.m.b.rom) {
            self.note(format!("Switched to the BASIC program {}", v));
        } else {
            self.note(format!("Switched to firmware {}: first start, resetting it to factory defaults", v));
            self.start_factory_default();
        }
    }

    /// Dial the telemetry code and 998, as the firmware's notes say.
    pub fn start_presets(&mut self) {
        if self.presets.is_none() {
            self.service_code = "998".into();
            self.presets = Some((self.m.seconds(), 0));
            self.note(format!("Loading the firmware's built-in rates and options: handset up, {}, then 998", self.telemetry_code()));
        }
    }

    fn run_presets(&mut self) {
        let Some((t0, step)) = self.presets else { return };
        if self.defaulting.is_some() || self.pnm.is_some() {
            return;
        }
        let t = self.m.seconds() - t0;
        match step {
            0 => {
                self.set_hook(true);
                self.presets = Some((t0, 1));
            }
            1 if t > 1.5 => {
                self.set_hook(false);
                self.presets = Some((t0, 2));
            }
            2 if t > 4.0 => {
                let code = self.telemetry_code();
                for c in code.chars() {
                    self.press_key(c);
                }
                self.presets = Some((t0, 3));
            }
            3 if t > 13.0 => {
                let sc = self.service_code.clone();
                for c in sc.chars() {
                    self.press_key(c);
                }
                self.presets = Some((t0, 4));
            }
            4 if t > 30.0 => {
                self.set_hook(true);
                self.presets = None;
                if self.service_code != "998" {
                    self.note(format!("Dialled {} in telemetry: the phone stores its settings in the EEPROM", self.service_code));
                    self.save_nvram();
                    return;
                }
                let ok = self.m.b.eeprom.windows(15).any(|w| w == b"RATES FOR PCM 5");
                if ok {
                    self.note("Built-in rates and options installed (stored in the EEPROM)");
                    self.save_nvram();
                } else {
                    self.note("The firmware did not store its built-in rates - try Load built-in rates again");
                }
            }
            _ => {}
        }
    }
}

/// A whole North American number: NXX-NXX-XXXX or 1-NXX-NXX-XXXX.
fn complete_number(n: &str) -> bool {
    let b = n.as_bytes();
    n.chars().all(|c| c.is_ascii_digit()) && ((b.len() == 10 && b[0] >= b'2') || (b.len() == 11 && b[0] == b'1' && b[1] >= b'2'))
}

impl Phone {
    /// The firmware seized the ringing line: answer the internet call and
    /// connect it to a software modem in place of the 73K212's analog side.
    fn start_modem_call(&mut self) {
        self.note("The phone's modem answered the incoming call: sending the answer tone and connecting it as a data call (300 or 1200 bit/s, whichever the caller uses)");
        let _ = self.sip_tx.send(SipCmd::Answer);
        self.m.b.ring_signal = false;
        self.audio.lock().unwrap().ringer = false;
        self.line = LineState::ModemCall;
        self.m.b.serial_out[0].clear();
        self.modem = Some(crate::modem::LineModem::new_answer());
        self.parity_stats = (0, 0, 0, 0);
        self.parity_warned = false;
        self.modem_mode_noted = false;
        self.modem_samples = 0.0;
        if let Some(ca) = &self.call_audio {
            let mut c = ca.lock().unwrap();
            c.modem = true;
            c.from_line.clear();
            c.to_line.clear();
        }
    }

    fn end_modem_call(&mut self) {
        self.flush_modem_log(true);
        if let Some(m) = &self.modem {
            let (rx, tx) = (m.rx_bytes, m.tx_bytes);
            self.note(format!("Data call finished ({} bytes from the caller, {} bytes sent)", rx, tx));
        }
        self.modem = None;
        self.m.b.inputs[0xC2] = 0;
        if let Some(ca) = &self.call_audio {
            ca.lock().unwrap().modem = false;
        }
        self.incoming_from = None;
        self.line = LineState::Idle;
    }

    /// Move audio and bytes between the SIP line and the firmware's serial port.
    fn pump_modem(&mut self, real: f64) {
        let Some(m) = self.modem.as_mut() else { return };
        let Some(ca) = self.call_audio.clone() else { return };
        // caller -> firmware
        let rx: Vec<f32> = ca.lock().unwrap().from_line.drain(..).collect();
        let bytes = m.demodulate(&rx);
        let had_carrier = self.m.b.inputs[0xC2] != 0;
        self.modem_rx_log.extend(bytes.iter().copied());
        let rx_copy = bytes.clone();
        self.m.b.serial_in[0].extend(bytes);
        // 73K212 detect register: carrier present
        let carrier = m.carrier();
        let mode_now = m.mode;
        let mode_name = m.mode_name();
        self.m.b.inputs[0xC2] = if carrier { 0x28 } else { 0 };
        if mode_now != crate::modem::LineMode::AnswerTone && !self.modem_mode_noted {
            self.modem_mode_noted = true;
            self.note(format!("Caller's modem recognised: {}", mode_name));
        }
        if carrier != had_carrier {
            let s = if carrier { "Caller's modem carrier detected" } else { "Caller's modem carrier lost" };
            self.note(s);
        }
        self.check_char_format(&rx_copy);
        // firmware -> caller
        let out: Vec<u8> = std::mem::take(&mut self.m.b.serial_out[0]);
        self.modem_tx_log.extend(out.iter().copied());
        self.flush_modem_log(false);
        let m = self.modem.as_mut().unwrap();
        for b in out {
            m.send_byte(b);
        }
        m.set_tx_enabled(self.m.b.modem.regs[0] & 0x02 != 0);
        // Keep 100 ms of modem audio queued for the network: the RTP sender
        // takes it at exactly 8000 samples a second, so the signal never has
        // gaps or skips whatever the emulator's own pace.
        let _ = real;
        let queued = ca.lock().unwrap().to_line.len();
        let n = 800usize.saturating_sub(queued);
        let tx = m.modulate(n);
        ca.lock().unwrap().to_line.extend(tx);
    }
}

impl Phone {
    /// The station id PNM must log on with (options RAM 0xC0D5: length, digits).
    pub fn station_id(&self) -> String {
        let r = &self.m.b.ram;
        let n = (r[0x10D5] & 0x0F) as usize;
        if n == 0 || n > 10 {
            return "?".into();
        }
        r[0x10D6..0x10D6 + n].iter().map(|d| char::from(b'0' + (d % 10))).collect()
    }
}

impl Phone {
    fn write_log(&self, line: &str) {
        let Some(p) = &self.log_path else { return };
        use std::io::Write;
        if std::fs::metadata(p).map(|m| m.len() > 4_000_000).unwrap_or(false) {
            let _ = std::fs::rename(p, p.with_extension("old.txt"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let _ = writeln!(f, "{} {}", stamp, line);
        }
    }
}

impl Phone {
    /// Write the bytes the modem passed each way into the log (every half second).
    fn flush_modem_log(&mut self, force: bool) {
        let t = self.m.seconds();
        if !force && t - self.modem_log_t < 0.5 {
            return;
        }
        self.modem_log_t = t;
        let hex = |v: &[u8]| v.iter().take(64).map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
        if !self.modem_rx_log.is_empty() {
            let v = std::mem::take(&mut self.modem_rx_log);
            self.note(format!("Modem: caller -> firmware ({} bytes): {}{}", v.len(), hex(&v), if v.len() > 64 { " ..." } else { "" }));
        }
        if !self.modem_tx_log.is_empty() {
            let v = std::mem::take(&mut self.modem_tx_log);
            self.note(format!("Modem: firmware -> caller ({} bytes): {}{}", v.len(), hex(&v), if v.len() > 64 { " ..." } else { "" }));
        }
    }
}

impl Phone {
    /// PNM talks 8N1 (it opens its port "N,8,1" and sends bytes above 0x7F).
    /// A calling modem set to 7E1/7O1 turns its top bit into parity: 02 05 02 07
    /// arrives as 82 05 82 87. Spot that and say so.
    fn check_char_format(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let ones = (b & 0x7F).count_ones();
            let top = b >> 7;
            let even_ok = top == (ones % 2) as u8;
            let (e, o, all, high) = &mut self.parity_stats;
            *all += 1;
            if even_ok {
                *e += 1;
            } else {
                *o += 1;
            }
            if top == 1 {
                *high += 1;
            }
        }
        let (e, o, all, high) = self.parity_stats;
        if self.parity_warned || all < 6 || high < 2 {
            return;
        }
        let fmt = if e == all {
            Some("7E1 (7 data bits, even parity)")
        } else if o == all {
            Some("7O1 (7 data bits, odd parity)")
        } else {
            None
        };
        if let Some(f) = fmt {
            self.parity_warned = true;
            self.note(format!(
                "The calling modem is sending {} characters. The payphone and PNM use 8N1 (PNM opens its port as N,8,1 and its commands use all 8 bits), so the logon looks wrong to the firmware. Set the calling modem's character format to 8N1.",
                f
            ));
        }
    }
}

impl Phone {
    /// The payphone's own number as the firmware keeps it (options RAM
    /// C0E6-C0EF, ten digits, one per byte). PNM writes the last seven at
    /// logon (the number it has on file); register 404 reports it.
    pub fn phone_number(&self) -> String {
        self.m.b.ram[0x10E6..0x10F0].iter().map(|&d| if d < 10 { (b'0' + d) as char } else { '?' }).collect()
    }

    /// Set the payphone's number (10 digits) and store it in the EEPROM.
    pub fn set_phone_number(&mut self, number: &str) -> bool {
        let digits: Vec<u8> = number.chars().filter(|c| c.is_ascii_digit()).map(|c| c as u8 - b'0').collect();
        let digits = if digits.len() == 11 && digits[0] == 1 { digits[1..].to_vec() } else { digits };
        if digits.len() != 10 {
            return false;
        }
        self.m.b.ram[0x10E6..0x10F0].copy_from_slice(&digits);
        crate::pnm::fix_option_checksums(&mut self.m.b);
        self.store_settings();
        self.note(format!("Payphone number set to {} (stored in the phone's settings)", self.phone_number()));
        true
    }

    /// A PNM session that logs on with this phone's number (last 7 digits),
    /// as PNM Plus does with the number it has on file.
    fn with_ani(&self, mut s: crate::pnm::PnmSession) -> crate::pnm::PnmSession {
        let n: Vec<u8> = self.m.b.ram[0x10E6..0x10F0].to_vec();
        if n.iter().all(|d| *d < 10) && n.iter().any(|d| *d != 0) {
            s.ani.copy_from_slice(&n[3..10]);
        }
        s
    }

    /// Option C243 bit 1: answer supervision by line polarity reversal
    /// (routine B47F -> B615). With it on, the firmware only charges a call
    /// when the line reverses (the far end answered) and refunds the coins
    /// of a call nobody answered; with it off it keeps the coins of any call
    /// it dialled. The emulated line reverses when the internet call is
    /// answered.
    pub fn answer_supervision(&self) -> bool {
        self.m.b.ram[0x1243] & 0x02 != 0
    }

    pub fn set_answer_supervision(&mut self, on: bool) {
        if on {
            self.m.b.ram[0x1243] |= 0x02;
        } else {
            self.m.b.ram[0x1243] &= !0x02;
        }
        crate::pnm::fix_option_checksums(&mut self.m.b);
        self.store_settings();
        self.note(if on {
            "Option C243 bit 1 set: calls nobody answers are refunded (answer detected by line reversal)"
        } else {
            "Option C243 bit 1 cleared: the phone keeps the coins of every call it dials"
        });
    }

    /// Option C242 bit 2 (battery RAM, set by a PNM options file): the
    /// firmware refuses coin-paid 1+ calls to area codes outside the phone's
    /// own state (those not priced exchange by exchange) - routine 8353
    /// marks them invalid unless F08C bit 0 is set.
    pub fn out_of_state_blocked(&self) -> bool {
        self.m.b.ram[0x1242] & 0x04 != 0
    }

    /// Change that option the way a corrected options file would: the bit,
    /// then the options checksums. The firmware itself is not changed.
    pub fn set_out_of_state_blocked(&mut self, block: bool) {
        if block {
            self.m.b.ram[0x1242] |= 0x04;
        } else {
            self.m.b.ram[0x1242] &= !0x04;
        }
        crate::pnm::fix_option_checksums(&mut self.m.b);
        // keep it in the EEPROM too, so a reload doesn't bring the old value back
        self.store_settings();
        self.note(if block {
            "Option C242 bit 2 set: out-of-state coin calls are refused (\"invalid number\")"
        } else {
            "Option C242 bit 2 cleared: out-of-state 1+ coin calls are allowed again"
        });
    }

    /// The keys that enter telemetry: # and the phone's bypass code.
    /// The bypass code lives in its own field of the options (C0CC: length,
    /// then up to 8 digits at C0CD), separate from the PNM password at C0DB.
    /// Factory defaults set 99999999; an options file - or the custom
    /// firmware's built-in options (998) - can change it (to 88888888 there).
    pub fn telemetry_code(&self) -> String {
        let r = &self.m.b.ram;
        let n = r[0x10CC] as usize;
        let digits: Option<String> =
            if (1..=8).contains(&n) { r[0x10CD..0x10CD + n].iter().map(|&d| if d < 10 { Some((b'0' + d) as char) } else { None }).collect() } else { None };
        format!("#{}", digits.unwrap_or_else(|| "99999999".into()))
    }

    /// The PNM password the phone holds (options RAM 0xC0DB: length, characters).
    pub fn pnm_password(&self) -> Vec<u8> {
        let r = &self.m.b.ram;
        let n = r[0x10DB] as usize;
        if (1..=8).contains(&n) {
            r[0x10DC..0x10DC + n].to_vec()
        } else {
            b"99999999".to_vec()
        }
    }
}
