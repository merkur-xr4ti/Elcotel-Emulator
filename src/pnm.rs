//! A virtual PNM Plus: dials into the emulated phone's modem and downloads
//! a rate file with the same protocol the Windows program uses
//! (CommCtlMfunbLogon2Fon / CommCtlMfunnSendAFile in the decompiled PNM,
//! with the framing bytes read from the firmware's own parser):
//!
//!   logon     PC: 02 <station id digits> 03            phone: 07
//!   password  PC: 02 90 03                              phone: 06
//!             PC: <len> 00 <ANI(7) 01 time(6) pwd(10)> <sum>   phone: 06
//!   download  PC: 02 A3 03                              phone: 06
//!             PC: 34 00 <first 52 bytes of the file> <sum>    phone: 06
//!             then for each block of the packed body (<=1024 bytes):
//!             PC: 02 A3 03                              phone: 06
//!             PC: <len lo> <len hi> <data> <sum>        phone: 06 (15 = resend)
//!             the last block ends with 00 00.
//!   hang up   PC: 02 01 03

use crate::board::PayBoard;

pub const ACK: u8 = 0x06;
pub const NAK: u8 = 0x15;
pub const LOGON_OK: u8 = 0x07;
const CMD_PASSWORD: u8 = 0x90;
const CMD_DOWNLOAD: u8 = 0xA3;
const CMD_HANGUP: u8 = 0x01;
/// Burn the downloaded RAM copy into the rates EEPROM (firmware routine 0x8000).
const CMD_BURN: u8 = 0x29;
/// Reload rates and options from the EEPROM into RAM (clears default mode).
const CMD_RELOAD: u8 = 0x08;

/// Byte where the packed body of a .R94 starts (PNM: rates header is 268 bytes).
pub const RATE_BODY_START: usize = 268;

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Ring,
    WaitAnswer,
    Carrier,
    Logon,
    PwdCmd,
    PwdData,
    HeaderCmd,
    HeaderData,
    BlockCmd,
    BlockData,
    Burn,
    Reload,
    Probe,
    Hangup,
    Done,
    Failed,
}

pub struct PnmSession {
    step: Step,
    t0: f64,
    deadline: f64,
    tries: u32,
    file: Vec<u8>,
    blocks: Vec<Vec<u8>>,
    block: usize,
    station_id: String,
    rx_seen: usize,
    saved_ring_bit: (u8, u8, bool),
    pub log: Vec<String>,
    pub finished: Option<bool>,
    burn_rx: Vec<u8>,
    password: Vec<u8>,
    /// Last 7 digits of the payphone number sent at logon (PNM sends the
    /// number it has on file; the firmware stores it as its own).
    pub ani: [u8; 7],
    /// The rates ended up in the EEPROM and are in use.
    pub stored: bool,
    /// Test probe: send this one command after logging on, keep the reply.
    pub probe_cmd: Option<u8>,
    pub probe_reply: Vec<u8>,
    probe_t: f64,
    /// Only store the RAM settings in the EEPROM (no file).
    store_only: bool,
}

fn frame(data: &[u8]) -> Vec<u8> {
    let n = data.len();
    let mut v = vec![(n % 256) as u8, (n / 256) as u8];
    v.extend_from_slice(data);
    let sum: u32 = data.iter().map(|b| *b as u32).sum();
    v.push((sum % 256) as u8);
    v
}

/// PNM's password byte coding (fn21DD): invert, rotate right by one.
fn enc(x: u8) -> u8 {
    let v = 255 - x;
    (v >> 1) | ((v & 1) << 7)
}

fn bcd(x: u32) -> u8 {
    (((x / 10) % 10) * 16 + x % 10) as u8
}

/// Split the packed body like CommCtlMfunnSendAFile: blocks of up to 1024
/// bytes, never ending a block on the first half of a 00 nn pair, and the
/// last block terminated with 00 00.
pub fn rate_blocks(file: &[u8]) -> Vec<Vec<u8>> {
    let body = &file[RATE_BODY_START.min(file.len())..];
    let mut blocks = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    for &b in body {
        if cur.len() == 1024 || (cur.len() == 1023 && b == 0) {
            blocks.push(std::mem::take(&mut cur));
        }
        cur.push(b);
    }
    cur.push(0);
    cur.push(0);
    blocks.push(cur);
    blocks
}

impl PnmSession {
    pub fn new(file: Vec<u8>, station_id: &str, now: f64) -> PnmSession {
        Self::with_password(file, station_id, b"99999999", now)
    }

    /// Log on with the phone's own station id and PNM password.
    pub fn with_password(file: Vec<u8>, station_id: &str, password: &[u8], now: f64) -> PnmSession {
        let blocks = rate_blocks(&file);
        PnmSession {
            step: Step::Ring,
            t0: now,
            deadline: now + 60.0,
            tries: 0,
            file,
            blocks,
            block: 0,
            station_id: station_id.to_string(),
            rx_seen: 0,
            saved_ring_bit: (0x60, 0x08, false),
            password: password.iter().take(8).copied().collect(),
            ani: [5, 5, 5, 0, 1, 0, 0],
            log: Vec::new(),
            finished: None,
            burn_rx: Vec::new(),
            stored: false,
            store_only: false,
            probe_cmd: None,
            probe_reply: Vec::new(),
            probe_t: 0.0,
        }
    }

    /// A test session: log on, send one command byte (02 cc 03), record what
    /// the phone answers for a few seconds, then hang up.
    pub fn probe(cmd: u8, station_id: &str, password: &[u8], now: f64) -> PnmSession {
        let mut s = PnmSession::with_password(Vec::new(), station_id, password, now);
        s.probe_cmd = Some(cmd);
        s
    }

    /// A session that only logs on and has the phone store its RAM settings
    /// in the EEPROM (0x29) and reload them (0x08): no file is sent.
    pub fn store_settings(station_id: &str, password: &[u8], now: f64) -> PnmSession {
        let mut s = PnmSession::with_password(Vec::new(), station_id, password, now);
        s.store_only = true;
        s
    }

    pub fn status(&self) -> String {
        match self.step {
            Step::Ring | Step::WaitAnswer => "ringing the phone's modem".into(),
            Step::Carrier => "modems connecting".into(),
            Step::Logon => "logging on".into(),
            Step::PwdCmd | Step::PwdData => "sending password".into(),
            Step::HeaderCmd | Step::HeaderData => "sending rate file header".into(),
            Step::BlockCmd | Step::BlockData => format!("sending block {} of {}", self.block + 1, self.blocks.len()),
            Step::Burn => "writing rates into the EEPROM".into(),
            Step::Reload => "reloading rates from the EEPROM".into(),
            Step::Hangup => "hanging up".into(),
            Step::Probe => "waiting for the reply to a probed command".into(),
            Step::Done => "done".into(),
            Step::Failed => "failed".into(),
        }
    }

    fn note(&mut self, now: f64, s: String) {
        self.log.push(format!("{:8.2}s {}", now, s));
    }

    fn send(&mut self, b: &mut PayBoard, now: f64, bytes: &[u8]) {
        let hex: Vec<String> = bytes.iter().take(24).map(|x| format!("{:02X}", x)).collect();
        self.note(now, format!("PC -> phone: {}{}", hex.join(" "), if bytes.len() > 24 { format!(" ... ({} bytes)", bytes.len()) } else { String::new() }));
        b.serial_in[0].extend(bytes.iter().copied());
    }

    fn fail(&mut self, b: &mut PayBoard, now: f64, why: &str) {
        self.note(now, format!("FAILED: {}", why));
        self.step = Step::Failed;
        self.finished = Some(false);
        self.cleanup(b);
    }

    fn cleanup(&mut self, b: &mut PayBoard) {
        b.ring_signal = false;
        b.inputs[0xC2] = 0;
        b.ring_bit = self.saved_ring_bit;
    }

    /// Bytes the phone sent since last time.
    fn reply(&mut self, b: &PayBoard) -> Vec<u8> {
        let out = &b.serial_out[0];
        let v = if out.len() > self.rx_seen { out[self.rx_seen..].to_vec() } else { Vec::new() };
        self.rx_seen = out.len();
        v
    }

    fn header_frame(&self) -> Vec<u8> {
        let mut h = self.file[..52.min(self.file.len())].to_vec();
        h.resize(52, 0);
        frame(&h)
    }

    fn password_frame(&self) -> Vec<u8> {
        // ANI: last 7 digits of the payphone number (any for a defaulted board)
        let mut d: Vec<u8> = self.ani.to_vec();
        d.push(1);
        // local time, BCD: minute hour weekday day month year
        let secs = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|x| x.as_secs() as i64).unwrap_or(0)
            + crate::rtc::local_offset_secs())
        .max(0) as u64;
        let days = secs / 86400;
        let sod = secs % 86400;
        let (y, m, dd) = civil(days as i64);
        let wd = ((days + 4) % 7) as u32 + 1;
        d.extend_from_slice(&[bcd((sod / 60 % 60) as u32), bcd((sod / 3600) as u32), wd as u8, bcd(dd), bcd(m), bcd((y % 100) as u32)]);
        // password, padded with zeros to 8 characters as PNM does
        let pwd: &[u8] = &self.password;
        d.push(enc(pwd.len() as u8));
        let mut s = pwd.len() as u32;
        for i in 0..8 {
            let c = pwd.get(i).copied().unwrap_or(0);
            s += c as u32;
            d.push(enc(c));
        }
        d.push(enc((s % 256) as u8));
        frame(&d)
    }

    /// Advance; call every emulator slice with the current emulated time.
    pub fn tick(&mut self, b: &mut PayBoard, now: f64) {
        if self.finished.is_some() {
            return;
        }
        if now > self.deadline && !matches!(self.step, Step::Done | Step::Failed) {
            let why = format!("no answer while {}", self.status());
            self.fail(b, now, &why);
            return;
        }
        let rx = self.reply(b);
        if !rx.is_empty() {
            let hex: Vec<String> = rx.iter().map(|x| format!("{:02X}", x)).collect();
            self.note(now, format!("phone -> PC: {}", hex.join(" ")));
        }
        let got = |c: u8| rx.contains(&c);
        match self.step {
            Step::Ring => {
                if !b.on_hook() {
                    self.fail(b, now, "hang up the handset first");
                    return;
                }
                self.saved_ring_bit = b.ring_bit;
                b.ring_bit = (0x60, 0x08, false);
                b.ring_signal = true;
                self.note(now, "ringing the payphone".into());
                self.step = Step::WaitAnswer;
                self.deadline = now + 40.0;
            }
            Step::WaitAnswer => {
                if b.line_seized() {
                    self.note(now, "the phone's modem answered".into());
                    b.ring_signal = false;
                    self.step = Step::Carrier;
                    self.t0 = now;
                    self.deadline = now + 30.0;
                }
            }
            Step::Carrier => {
                // answer tone, then the calling modem's carrier
                if now - self.t0 > 2.0 {
                    b.inputs[0xC2] = 0x28;
                }
                if now - self.t0 > 6.5 {
                    self.note(now, "carrier up; sending station id".into());
                    self.step = Step::Logon;
                    self.tries = 0;
                    self.send_logon(b, now);
                }
            }
            Step::Logon => {
                if got(LOGON_OK) {
                    self.note(now, "station id accepted".into());
                    self.step = Step::PwdCmd;
                    self.send(b, now, &[0x02, CMD_PASSWORD, 0x03]);
                    self.deadline = now + 10.0;
                } else if now - self.t0 > 3.0 {
                    self.tries += 1;
                    if self.tries > 3 {
                        self.fail(b, now, "phone did not accept the station id");
                    } else {
                        self.send_logon(b, now);
                    }
                }
            }
            Step::PwdCmd => {
                if got(ACK) {
                    self.step = Step::PwdData;
                    let f = self.password_frame();
                    self.send(b, now, &f);
                    self.deadline = now + 10.0;
                } else if got(NAK) {
                    self.fail(b, now, "password command refused");
                }
            }
            Step::PwdData => {
                if got(ACK) {
                    if let Some(c) = self.probe_cmd {
                        self.note(now, format!("password accepted; probing command {:02X}", c));
                        self.step = Step::Probe;
                        self.probe_t = now;
                        self.send(b, now, &[0x02, c, 0x03]);
                        self.deadline = now + 30.0;
                        return;
                    }
                    if self.store_only {
                        self.note(now, "password accepted; asking the phone to store its settings in its EEPROM (command 0x29)".into());
                        self.step = Step::Burn;
                        self.t0 = now;
                        self.burn_rx.clear();
                        self.send(b, now, &[0x02, CMD_BURN, 0x03]);
                        self.deadline = now + 40.0;
                        return;
                    }
                    self.note(now, "password accepted; downloading rates".into());
                    self.step = Step::HeaderCmd;
                    self.send(b, now, &[0x02, CMD_DOWNLOAD, 0x03]);
                    self.deadline = now + 15.0;
                } else if got(NAK) {
                    self.fail(b, now, "phone did not accept the password");
                }
            }
            Step::HeaderCmd => {
                if got(ACK) {
                    self.step = Step::HeaderData;
                    let f = self.header_frame();
                    self.send(b, now, &f);
                    self.deadline = now + 15.0;
                } else if got(NAK) {
                    self.fail(b, now, "download command refused");
                }
            }
            Step::HeaderData => {
                if got(ACK) {
                    self.step = Step::BlockCmd;
                    self.block = 0;
                    self.tries = 0;
                    self.send(b, now, &[0x02, CMD_DOWNLOAD, 0x03]);
                    self.deadline = now + 15.0;
                } else if got(NAK) {
                    self.fail(b, now, "phone refused the rate file header");
                }
            }
            Step::BlockCmd => {
                if got(ACK) {
                    self.step = Step::BlockData;
                    let f = frame(&self.blocks[self.block].clone());
                    self.send(b, now, &f);
                    self.deadline = now + 35.0;
                } else if got(NAK) {
                    self.fail(b, now, "block command refused");
                }
            }
            Step::BlockData => {
                if got(ACK) {
                    self.block += 1;
                    self.tries = 0;
                    if self.block >= self.blocks.len() {
                        self.note(now, "all blocks sent".into());
                        mark_options_programmed(b);
                        self.note(now, "no options file: keeping the phone's default options, routing long-distance like local calls, and marking them as programmed (signature + checksums) as a PNM options download would".into());
                        self.note(now, "asking the phone to store everything in its EEPROM (command 0x29)".into());
                        self.step = Step::Burn;
                        self.t0 = now;
                        self.burn_rx.clear();
                        self.send(b, now, &[0x02, CMD_BURN, 0x03]);
                        self.deadline = now + 40.0;
                    } else {
                        self.step = Step::BlockCmd;
                        self.send(b, now, &[0x02, CMD_DOWNLOAD, 0x03]);
                        self.deadline = now + 15.0;
                    }
                } else if got(NAK) {
                    self.tries += 1;
                    if self.tries > 3 {
                        self.fail(b, now, "phone kept rejecting a block");
                    } else {
                        self.note(now, "block rejected, sending again".into());
                        self.step = Step::BlockCmd;
                        self.send(b, now, &[0x02, CMD_DOWNLOAD, 0x03]);
                    }
                }
            }
            Step::Burn => {
                // 0x29 answers 06 then, after writing the EEPROM, 0B (stored)
                // or 0C (refused).
                self.burn_rx.extend(rx.iter().copied());
                if self.burn_rx.contains(&0x0B) {
                    self.note(now, "rates and options written into the EEPROM; asking the phone to reload them".into());
                    self.step = Step::Reload;
                    self.burn_rx.clear();
                    self.send(b, now, &[0x02, CMD_RELOAD, 0x03]);
                    self.deadline = now + 40.0;
                } else if self.burn_rx.contains(&0x0C) || self.burn_rx.contains(&NAK) {
                    self.note(now, "phone refused to store the rates in its EEPROM (they stay in battery RAM)".into());
                    self.step = Step::Hangup;
                    self.t0 = now;
                    self.deadline = now + 40.0;
                }
            }
            Step::Reload => {
                if got(ACK) {
                    self.note(now, "phone reloaded its rates from the EEPROM and left factory-default mode".into());
                    self.stored = true;
                    self.step = Step::Hangup;
                    self.t0 = now;
                    self.deadline = now + 40.0;
                } else if !rx.is_empty() {
                    self.note(now, "phone could not reload from the EEPROM".into());
                    self.step = Step::Hangup;
                    self.t0 = now;
                    self.deadline = now + 40.0;
                }
            }
            Step::Probe => {
                self.probe_reply.extend(rx.iter().copied());
                if !b.line_seized() {
                    self.note(now, "the phone hung up after the command".into());
                    self.step = Step::Done;
                    self.finished = Some(true);
                    self.cleanup(b);
                } else if now - self.probe_t > 6.0 {
                    self.step = Step::Hangup;
                    self.t0 = now;
                    self.deadline = now + 40.0;
                }
            }
            Step::Hangup => {
                if now - self.t0 > 1.0 && self.t0 >= 0.0 {
                    self.send(b, now, &[0x02, CMD_HANGUP, 0x03]);
                    self.t0 = -1.0;
                    self.deadline = now + 8.0;
                    self.tries = 0;
                } else if self.t0 < 0.0 && !b.line_seized() {
                    self.note(now, "phone hung up".into());
                    self.step = Step::Done;
                    self.finished = Some(true);
                    self.cleanup(b);
                } else if self.t0 < 0.0 && now + 0.05 > self.deadline {
                    // carrier drop ends the call anyway
                    self.note(now, "dropping carrier".into());
                    self.step = Step::Done;
                    self.finished = Some(true);
                    self.cleanup(b);
                }
            }
            Step::Done | Step::Failed => {}
        }
    }

    fn send_logon(&mut self, b: &mut PayBoard, now: f64) {
        let mut v = vec![0x02];
        for c in self.station_id.chars() {
            v.push(c.to_digit(10).unwrap_or(0) as u8);
        }
        v.push(0x03);
        self.send(b, now, &v);
        self.t0 = now;
        self.deadline = now + 15.0;
    }
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 16-bit byte sum as the firmware computes it (routine 0x2AF2); 0 becomes 1.
fn fw_sum(mem: &[u8]) -> u16 {
    let s = mem.iter().fold(0u16, |a, &b| a.wrapping_add(b as u16));
    if s == 0 {
        1
    } else {
        s
    }
}

/// What a PNM options (.C94) download leaves in battery RAM, applied to the
/// options the phone already has: the "RATES FOR PCM 5 (C) COPYRIGHT ..."
/// signature at logical 0xC000 (phys 0x31000) and the two checksums the
/// firmware verifies before it will store anything in the EEPROM
/// (routine 0x28BB: 0xCFFE over 0xC000-0xCFFD, 0xBFFE combining that with
/// the dialing-pattern area 0xB400-0xBD5F).
pub fn mark_options_programmed(b: &mut PayBoard) {
    // Call-type routing (0xC250 + type): the factory defaults route only
    // local (type 4 = 2) and toll-free calls. Give the long-distance types
    // the rate bands map to (bands 2-6 -> types 5..8) the same direct route,
    // so every call the rate file prices can be placed.
    let local_route = b.ram[0x1250 + 4];
    for t in 5..=8usize {
        if b.ram[0x1250 + t] == 0 {
            b.ram[0x1250 + t] = if local_route != 0 { local_route } else { 2 };
        }
    }
    let sig = &b.rom[0x2362..0x2362 + 0x20].to_vec();
    b.ram[0x1000..0x1020].copy_from_slice(sig);
    fix_option_checksums(b);
}

/// Recompute the two checksums the firmware keeps over its options
/// (CFFE over C000-CFFD; BFFE combining it with the dialling patterns).
pub fn fix_option_checksums(b: &mut PayBoard) {
    let c = fw_sum(&b.ram[0x1000..0x1FFE]);
    b.ram[0x1FFE] = c as u8;
    b.ram[0x1FFF] = (c >> 8) as u8;
    let de = c.wrapping_add(c & 0xFF).wrapping_add(c >> 8);
    let p = fw_sum(&b.ram[0x0400..0x0400 + 0x960]);
    let v = de.wrapping_add(p);
    b.ram[0x0FFE] = v as u8;
    b.ram[0x0FFF] = (v >> 8) as u8;
}
