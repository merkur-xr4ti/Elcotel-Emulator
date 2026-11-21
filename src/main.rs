#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod board;
mod coverage;
mod cpu;
mod cvsd;
mod disasm;
mod dpsk;
mod files;
mod machine;
mod modem;
mod phone;
mod pnm;
mod ratefile;
mod rtc;
mod sip;
mod z180;

use board::{PayBoard, CPU_HZ};
use machine::Machine;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

/// The stock program ROM's usual file name: its saved memory keeps the
/// original name nvram.bin (see nvram_for).
const ROM_NAME: &str = "Elcotel_PGM_5.4.4_1999_27C512.BIN";

/// Folders holding the dumps. Each base folder may have `rom/` (program ROM
/// dumps, 27C512), `voice/` (voice ROM dump, 27C010) and `rates/` (PNM .R94
/// rate files). Base folders: `--firmware-dir <folder>`, the program's own
/// folder, the folder above it (the repository, for `bin/elcotel-emu.exe`),
/// then the working directory (handy with `cargo run`).
fn base_dirs(args: &[String]) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(i) = args.iter().position(|a| a == "--firmware-dir") {
        if let Some(d) = args.get(i + 1) {
            v.push(PathBuf::from(d));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            v.push(d.to_path_buf());
            if let Some(up) = d.parent() {
                v.push(up.to_path_buf());
            }
        }
    }
    if let Ok(d) = std::env::current_dir() {
        if !v.contains(&d) {
            v.push(d);
        }
    }
    v
}

fn sub_dirs(bases: &[PathBuf], name: &str) -> Vec<PathBuf> {
    bases.iter().map(|b| b.join(name)).collect()
}

/// Files in some folders, sorted by name within each folder.
fn files_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
        v.sort();
        out.extend(v);
    }
    out
}

/// The first .R94 rate file in the rates folders.
fn find_rate_file(rate_dirs: &[PathBuf]) -> Option<PathBuf> {
    files_in(rate_dirs).into_iter().find(|p| p.extension().map(|x| x.to_string_lossy().eq_ignore_ascii_case("r94")).unwrap_or(false))
}

/// The first voice ROM dump in the voice folders.
fn find_voice(voice_dirs: &[PathBuf]) -> Option<PathBuf> {
    files_in(voice_dirs).into_iter().find(|p| std::fs::read(p).ok().and_then(|d| files::fit_voice(d).ok()).is_some())
}

/// The voice ROM in the voice/ folder (for the app's "use the folder's" button).
pub fn folder_voice() -> Option<PathBuf> {
    find_voice(&sub_dirs(&base_dirs(&[]), "voice"))
}

/// The rate file in the rates/ folder.
pub fn folder_rate_file() -> Option<PathBuf> {
    find_rate_file(&sub_dirs(&base_dirs(&[]), "rates"))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bases = base_dirs(&args);
    let rom_dirs = sub_dirs(&bases, "rom");
    let voice_dirs = sub_dirs(&bases, "voice");
    let rate_dirs = sub_dirs(&bases, "rates");
    let gui = !args.iter().skip(1).any(|a| a.starts_with("--") && a != "--firmware-dir");
    // Program ROMs: every dump in rom/ plus files opened in the app.
    let mut roms = find_roms(&rom_dirs);
    for p in files::external_roms() {
        if roms.iter().any(|(q, _)| q == &p) {
            continue;
        }
        if let Some((img, _)) = std::fs::read(&p).ok().and_then(|d| files::fit_program(d).ok()) {
            roms.push((p, phone::rom_version(&img)));
        }
    }
    // The one to run: ELCOTEL_ROM (a path, or a file name in rom/; tests),
    // else the one picked in the app (firmware.txt), else the first in rom/.
    let wanted = std::env::var("ELCOTEL_ROM").ok().or_else(|| if gui { files::choice("firmware") } else { None });
    let rom_path = wanted
        .and_then(|w| {
            let p = PathBuf::from(&w);
            if p.is_file() {
                Some(p)
            } else {
                roms.iter().map(|(q, _)| q.clone()).find(|q| q.file_name().map(|n| n.to_string_lossy().eq_ignore_ascii_case(&w)).unwrap_or(false))
            }
        })
        .or_else(|| roms.first().map(|(p, _)| p.clone()));
    // The voice ROM: the one picked in the app, else the first in voice/.
    let voice_path = files::choice("voice").filter(|_| gui).map(PathBuf::from).filter(|p| p.is_file()).or_else(|| find_voice(&voice_dirs));
    let rom = rom_path.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|d| files::fit_program(d).ok().map(|x| x.0));
    let voice = voice_path.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|d| files::fit_voice(d).ok());
    let (Some(rom), Some(voice)) = (rom, voice) else {
        let here = bases.first().map(|b| b.display().to_string()).unwrap_or_default();
        let msg = format!(
            "Couldn't find the ROM dumps.\n\nPut the program ROM dump (64 KB, 27C512) in a folder named \"rom\" and the voice ROM dump (128 KB, 27C010) in a folder named \"voice\" next to the program:\n\n  {}\\rom\\\n  {}\\voice\\\n\nA PNM rate file (.R94) can go in a folder named \"rates\" there too.",
            here, here
        );
        eprintln!("{}", msg);
        let _ = eframe::run_simple_native("Elcotel Series 5 emulator", eframe::NativeOptions::default(), move |ctx, _| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("Firmware not found");
                ui.label(&msg);
            });
        });
        return;
    };
    if args.len() > 3 && args[1] == "--dis" {
        let start = hex(&args[2]) as u16;
        let count: usize = args[3].parse().unwrap();
        let rd = |a: u16| rom[a as usize];
        let mut pc = start;
        for _ in 0..count {
            let (t, l) = disasm::disasm(&rd, pc);
            let bytes: Vec<String> = (0..l).map(|i| format!("{:02X}", rom[pc.wrapping_add(i) as usize])).collect();
            println!("{:04X}  {:<12} {}", pc, bytes.join(""), t);
            pc = pc.wrapping_add(l);
        }
        return;
    }
    if args.len() > 2 && args[1] == "--trace" {
        trace(&args, rom, voice);
        return;
    }
    if args.len() > 1 && args[1] == "--presettest" {
        presettest(rom, voice);
        return;
    }
    if args.len() > 1 && args[1] == "--incomingtest" {
        incomingtest(rom, voice);
        return;
    }
    if args.len() > 1 && args[1] == "--modemtest" {
        modemtest(rom, voice);
        return;
    }
    if args.len() > 1 && args[1] == "--ratetest" {
        ratetest(rom, voice, find_rate_file(&rate_dirs));
        return;
    }
    if args.len() > 1 && args[1] == "--coverage" {
        coveragetest(rom, voice, find_rate_file(&rate_dirs), args.get(2).cloned());
        return;
    }
    if args.len() > 1 && args[1] == "--telemtest" {
        telemtest(rom, voice);
        return;
    }
    if args.len() > 1 && args[1] == "--selftest" {
        selftest(rom, voice);
        return;
    }

    // a rate file the person opened in the app, else the usual search
    let rates = files::choice("rates")
        .and_then(|p| ratefile::RateFile::load(std::path::Path::new(&p)))
        .or_else(|| find_rate_file(&rate_dirs).and_then(|p| ratefile::RateFile::load(&p)));

    // shared pieces
    let audio_shared = Arc::new(Mutex::new(audio::AudioShared::default()));
    let call_audio = Arc::new(Mutex::new(sip::CallAudio::default()));
    let (sip_tx, sip_cmd_rx) = std::sync::mpsc::channel();
    let (sip_ev_tx, sip_rx) = std::sync::mpsc::channel();
    sip::SipAgent::spawn(sip_cmd_rx, sip_ev_tx, call_audio.clone());
    let nvram = rom_path.as_ref().and_then(|p| nvram_for(p.file_name()?.to_str()?));
    let mut board = PayBoard::new(rom, voice.clone());
    board.mem_seen = Some(vec![0; 0x40000]);
    board.mem_age = Some(board::MemAge::new());
    let phone = Arc::new(Mutex::new(phone::Phone::new(board, sip_tx, sip_rx, audio_shared.clone(), nvram)));
    phone.lock().unwrap().call_audio = Some(call_audio.clone());
    phone.lock().unwrap().m.pc_hist = Some(vec![0; 65536]);
    phone.lock().unwrap().log_path = app_dir().map(|d| d.join("log.txt"));
    phone::spawn_runner(phone.clone());
    let _audio = audio::Audio::start(audio_shared.clone(), call_audio.clone());

    let viewport =
        egui::ViewportBuilder::default().with_title("Elcotel Series 5 payphone emulator").with_inner_size([1280.0, 820.0]).with_min_inner_size([980.0, 700.0]);
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let _ = eframe::run_native("elcotel-emu", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, phone, audio_shared, rates, voice, roms)))));
}

/// Headless run with a script of actions, printing what the firmware does.
/// Actions: [@secs:]inNN=VV | hook=0/1 | keys=123 | coin=n/d/q | prog=0/1 | ring=0/1
/// Watches: pc=XXXX, mw=XXXX (logical write)
fn trace(args: &[String], rom: Vec<u8>, voice: Vec<u8>) {
    let secs: f64 = args[2].parse().unwrap();
    let mut board = PayBoard::new(rom, voice);
    let mut sched: Vec<(u64, String)> = Vec::new();
    let mut watch_pcs: Vec<u16> = Vec::new();
    let mut hist: Option<(f64, f64)> = None;
    let mut hist_map = std::collections::HashMap::<u16, u64>::new();
    for kv in args.iter().skip(3) {
        if let Some(rest) = kv.strip_prefix("pc=") {
            watch_pcs.push(hex(rest) as u16);
            continue;
        }
        if let Some(rest) = kv.strip_prefix("hist=") {
            let (a, b) = rest.split_once('-').unwrap();
            hist = Some((a.parse().unwrap(), b.parse().unwrap()));
            continue;
        }
        if let Some(rest) = kv.strip_prefix("mw=") {
            board.watch_writes.push(hex(rest) as u16);
            continue;
        }
        if let Some(r) = kv.strip_prefix('@') {
            let mut it = r.splitn(2, ':');
            let t: f64 = it.next().unwrap().parse().unwrap();
            sched.push(((t * CPU_HZ as f64) as u64, it.next().unwrap().to_string()));
        } else {
            sched.push((0, kv.clone()));
        }
    }
    let mut m = Machine::new(board);
    let total = (secs * CPU_HZ as f64) as u64;
    let mut last_out = m.b.outputs;
    let mut ser_seen = 0usize;
    let mut pnm_session: Option<pnm::PnmSession> = None;
    let mut pnm_logged = 0usize;
    let mut snap: Option<Vec<u8>> = None;
    let mut next = 0usize;
    sched.sort_by_key(|s| s.0);
    let quiet_ports = [0x40usize, 0xA4, 0xC1, 0xC2];
    while m.total_cycles < total {
        while next < sched.len() && sched[next].0 <= m.total_cycles {
            let a = sched[next].1.clone();
            next += 1;
            println!("{:8.3}s  ACTION {}", m.seconds(), a);
            let (k, v) = a.split_once('=').unwrap();
            match k {
                "hook" => m.b.set_hook(v == "1"),
                "keys" => v.chars().for_each(|c| m.b.press_key(c)),
                "coin" => m.b.drop_coin(match v {
                    "n" => 0,
                    "d" => 1,
                    _ => 2,
                }),
                "prog" => m.b.program_button = v == "1",
                "ring" => m.b.ring_signal = v == "1",
                "snap" => snap = Some(m.b.ram.clone()),
                "pnm" => pnm_session = Some(pnm::PnmSession::new(std::fs::read(v).unwrap(), "9999", m.seconds())),
                "ser" => {
                    let bytes: Vec<u8> = (0..v.len() / 2).map(|i| u8::from_str_radix(&v[i * 2..i * 2 + 2], 16).unwrap()).collect();
                    m.b.serial_in[0].extend(bytes);
                }
                "ringbit" => {
                    let parts: Vec<&str> = v.split(':').collect();
                    m.b.ring_bit = (hex(parts[0]) as u8, hex(parts[1]) as u8, parts.get(2) == Some(&"low"));
                    m.b.ring_pulsed = parts.contains(&"pulse");
                }
                _ if k.starts_with("in") => {
                    let p = hex(&k[2..]) as usize;
                    m.b.inputs[p] = hex(v) as u8;
                }
                _ => println!("unknown action {}", k),
            }
        }
        let in_hist = hist.map(|(a, b)| m.seconds() >= a && m.seconds() < b).unwrap_or(false);
        if in_hist {
            if !m.asleep {
                *hist_map.entry(m.z.cpu.pc).or_default() += 1;
            }
            m.run(1);
        } else if watch_pcs.is_empty() {
            m.run(2000);
        } else {
            let pc = m.z.cpu.pc;
            if watch_pcs.contains(&pc) && !m.asleep {
                let c = &m.z.cpu;
                println!("{:8.3}s  at {:04X} A={:02X} BC={:04X} DE={:04X} HL={:04X} SP={:04X}", m.seconds(), pc, c.a, c.bc(), c.de(), c.hl(), c.sp);
            }
            m.run(1);
        }
        for ev in m.b.events.drain(..) {
            println!("{:8.3}s  EVENT {:?}", m.total_cycles as f64 / CPU_HZ as f64, ev);
        }
        for p in 0..256usize {
            if m.b.outputs[p] != last_out[p] && !(0x80..=0x8F).contains(&p) && !quiet_ports.contains(&p) {
                println!("{:8.3}s  out {:02X} {:02X}->{:02X}", m.seconds(), p, last_out[p], m.b.outputs[p]);
            }
        }
        last_out = m.b.outputs;
        if let Some(s) = pnm_session.as_mut() {
            let now = m.seconds();
            s.tick(&mut m.b, now);
            for l in &s.log[pnm_logged..] {
                println!("   PNM {}", l);
            }
            pnm_logged = s.log.len();
        }
        if m.b.serial_out[0].len() > ser_seen {
            let s: Vec<String> = m.b.serial_out[0][ser_seen..].iter().map(|b| format!("{:02X}", b)).collect();
            println!("{:8.3}s  SERIAL OUT {}", m.seconds(), s.join(" "));
            ser_seen = m.b.serial_out[0].len();
        }
        // serial port 1: PRINT output of ELCOMPILER programs, line by line
        while let Some(nl) = m.b.serial_out[1].iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = m.b.serial_out[1].drain(..=nl).collect();
            println!("{:8.3}s  PRINT {}", m.seconds(), String::from_utf8_lossy(&line).trim_end());
        }
    }
    if !hist_map.is_empty() {
        let mut h: Vec<_> = hist_map.into_iter().collect();
        h.sort_by_key(|x| std::cmp::Reverse(x.1));
        let mut pcs: Vec<u16> = h.iter().take(400).map(|x| x.0).collect();
        pcs.sort();
        let s: Vec<String> = pcs.iter().map(|p| format!("{:04X}", p)).collect();
        println!("hist pcs: {}", s.join(" "));
    }
    for (name, mem) in [("ram", &m.b.ram), ("eeprom", &m.b.eeprom), ("ram_lo", &m.b.ram_lo)] {
        for pat in [&b"517299"[..], &b"RATES FOR PCM"[..], &[0x04u8, 0x04, 0x04, 0x05, 0x04, 0x04, 0x04, 0x04][..]] {
            if let Some(p) = mem.windows(pat.len()).position(|w| w == pat) {
                println!("found {:?} in {} at {:04X}", String::from_utf8_lossy(pat), name, p);
            }
        }
    }
    println!("eeprom non-FF bytes: {}  first: {:02X?}", m.b.eeprom.iter().filter(|b| **b != 0xFF).count(), &m.b.eeprom[..16]);
    if let Ok(spec) = std::env::var("DUMPRAM") {
        let (a, n) = spec.split_once(',').unwrap();
        let a = hex(a) as usize - 0x30000;
        let n: usize = n.parse().unwrap();
        println!("ram {:05X}: {:02X?}", a + 0x30000, &m.b.ram[a..a + n]);
    }
    if std::env::var("DUMPPAT").is_ok() {
        for k in 0..0x32usize {
            let o = 0x0400 + k * 0x1F;
            let e = &m.b.ram[o..o + 0x1F];
            if e[0] != 0 {
                println!("pattern {:2}: {:02X?}", k, e);
            }
        }
    }
    if let Some(s) = &snap {
        let mut i = 0;
        while i < 0x10000 {
            if s[i] != m.b.ram[i] {
                let st = i;
                let mut last = i;
                while i < 0x10000 && i - last < 32 {
                    if s[i] != m.b.ram[i] {
                        last = i;
                    }
                    i += 1;
                }
                println!("ram changed {:05X}-{:05X} ({} bytes)", 0x30000 + st, 0x30000 + last, last - st + 1);
            }
            i += 1;
        }
    }
    println!(
        "asci0 cntla={:02X} cntlb={:02X} stat={:02X} itc={:02X} modem regs={:02X?}",
        m.z.io.asci[0].cntla, m.z.io.asci[0].cntlb, m.z.io.asci[0].stat, m.z.io.itc, m.b.modem.regs
    );
    println!("end: PC={:04X} asleep={} sleeps={} wakes={}", m.z.cpu.pc, m.asleep, m.sleep_count, m.wake_count);
    for (i, n) in m.z.irq_count.iter().enumerate() {
        if *n > 0 {
            println!("irq {}: {}", z180::IRQ_NAMES[i], n);
        }
    }
    println!("voice bytes: {}  serial0 out: {:02X?}", m.b.voice_bytes, &m.b.serial_out[0][..m.b.serial_out[0].len().min(32)]);
}

/// End-to-end check without a window: the payphone (running the real
/// firmware) calls a second, local SIP agent that answers.
/// Telemetry entry: lift the handset, wait, type the bypass code with a
/// given gap between keys, then dial 404 (read back the phone number) and
/// report what the firmware said and dialled. NVRAM=<file> starts every try
/// from a copy of that saved memory; CODE= overrides the keys (default: the
/// bypass code the phone holds).
fn telemtest(rom: Vec<u8>, voice: Vec<u8>) {
    let code_env = std::env::var("CODE").ok();
    let waits: Vec<f64> = std::env::var("WAITS").ok().map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or(vec![0.3, 1.0, 2.0, 4.0, 8.0]);
    let gaps: Vec<f64> = std::env::var("GAPS").ok().map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or(vec![0.15, 0.4, 1.0]);
    for &wait in &waits {
        for &gap in &gaps {
            let (tx, _rx_cmd) = std::sync::mpsc::channel();
            let (_tx_ev, rx) = std::sync::mpsc::channel();
            let nv = std::env::var("NVRAM").ok().map(|f| {
                let tmp = std::env::temp_dir().join("elcotel-telem-copy.bin");
                std::fs::copy(&f, &tmp).unwrap();
                tmp
            });
            let mut p = phone::Phone::new(PayBoard::new(rom.clone(), voice.clone()), tx, rx, Arc::new(Mutex::new(audio::AudioShared::default())), nv);
            let run = |p: &mut phone::Phone, secs: f64| {
                for _ in 0..(secs * 100.0) as usize {
                    p.advance(0.01);
                }
            };
            // first start (factory default, built-in rates) then rest on hook
            let mut w = 0;
            while (p.defaulting.is_some() || p.presets.is_some()) && w < 200 {
                run(&mut p, 0.5);
                w += 1;
            }
            run(&mut p, 5.0);
            p.set_hook(false);
            run(&mut p, wait);
            let code = code_env.clone().unwrap_or_else(|| p.telemetry_code());
            let before = p.prompts.len();
            let log_before = p.log.len();
            for c in code.chars() {
                p.press_key(c);
                run(&mut p, gap);
            }
            run(&mut p, 6.0);
            let after_code: Vec<String> = p.prompts.iter().skip(before).map(|x| format!("{:02X}", x.1)).collect();
            let mid = p.prompts.len();
            for c in "404".chars() {
                p.press_key(c);
                run(&mut p, 0.3);
            }
            run(&mut p, 8.0);
            let after_404: Vec<String> = p.prompts.iter().skip(mid).map(|x| format!("{:02X}", x.1)).collect();
            let dialled: String = p.log.iter().skip(log_before).filter_map(|l| l.text.strip_prefix("Firmware dials digit ").map(|s| s.to_string())).collect();
            println!(
                "wait {:4.1}s gap {:4.2}s | after code: [{}] | after 404: [{}] | dialled out: {}",
                wait,
                gap,
                after_code.join(" "),
                after_404.join(" "),
                if dialled.is_empty() { "-".into() } else { dialled }
            );
        }
    }
}

fn selftest(rom: Vec<u8>, voice: Vec<u8>) {
    use sip::{SipAgent, SipCmd, SipConfig, SipEvent};
    let audio_shared = Arc::new(Mutex::new(audio::AudioShared::default()));
    // callee on 127.0.0.1:5072
    let callee_audio = Arc::new(Mutex::new(sip::CallAudio::default()));
    let (ctx, crx) = std::sync::mpsc::channel();
    let (cetx, cerx) = std::sync::mpsc::channel();
    SipAgent::spawn(crx, cetx, callee_audio);
    let callee_cfg =
        SipConfig { enabled: true, server: "127.0.0.1:5071".into(), username: "callee".into(), local_port: 5072, register: false, ..Default::default() };
    ctx.send(SipCmd::Configure(callee_cfg)).unwrap();
    // payphone agent sends straight to the callee
    let call_audio = Arc::new(Mutex::new(sip::CallAudio::default()));
    let (ptx, prx) = std::sync::mpsc::channel();
    let (petx, perx) = std::sync::mpsc::channel();
    SipAgent::spawn(prx, petx, call_audio.clone());
    let cfg =
        SipConfig { enabled: true, server: "127.0.0.1:5072".into(), username: "payphone".into(), local_port: 5071, register: false, ..Default::default() };
    ptx.send(SipCmd::Configure(cfg)).unwrap();
    let mut p = phone::Phone::new(PayBoard::new(rom, voice), ptx, perx, audio_shared, None);
    p.sip_enabled = true;
    let step = |p: &mut phone::Phone, secs: f64| {
        let n = (secs * 100.0) as usize;
        for _ in 0..n {
            p.advance(0.01);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    step(&mut p, 16.0);
    println!("after factory default: defaulting={:?} on_hook={}", p.defaulting.is_some(), p.m.b.on_hook());
    p.set_hook(false);
    step(&mut p, 2.0);
    for c in "5551234".chars() {
        p.press_key(c);
    }
    let mut answered_at = None;
    let mut coin_dropped = false;
    for i in 0..4000 {
        step(&mut p, 0.01);
        if !coin_dropped && p.prompts.len() > 2 && i > 300 {
            p.drop_coin(2);
            coin_dropped = true;
        }
        while let Ok(e) = cerx.try_recv() {
            match e {
                SipEvent::Incoming(from) => {
                    println!("callee: incoming call from {}", from);
                    ctx.send(SipCmd::Answer).unwrap();
                }
                SipEvent::Answered => println!("callee: answered"),
                SipEvent::Ended(w) => println!("callee: ended ({})", w),
                _ => {}
            }
        }
        if p.line == phone::LineState::Talking && answered_at.is_none() {
            answered_at = Some(p.m.seconds());
            println!("payphone: CONNECTED to {} at {:.1}s", p.call_number, p.m.seconds());
            call_audio.lock().unwrap().to_line.extend(std::iter::repeat_n(0.3, 800));
        }
        if let Some(t) = answered_at {
            if p.m.seconds() - t > 3.0 {
                break;
            }
        }
    }
    p.set_hook(true);
    step(&mut p, 1.0);
    while let Ok(e) = cerx.try_recv() {
        if let SipEvent::Ended(w) = e {
            println!("callee: ended ({})", w);
        }
    }
    for l in &p.log {
        println!("{:8.2}s {}", l.t, l.text);
    }
    let pr: Vec<String> =
        p.prompts.iter().map(|x| if std::env::var("CALLSECS").is_ok() { format!("{:.0}s:{:02X}", x.0, x.1) } else { format!("{:02X}", x.1) }).collect();
    println!("prompts: {}", pr.join(" "));
    println!("RESULT: {}", if answered_at.is_some() { "PASS - call placed by the firmware and answered over SIP" } else { "FAIL" });
}

/// Headless check of the rate file import through the phone controller.
fn ratetest(rom: Vec<u8>, voice: Vec<u8>, rate_file: Option<PathBuf>) {
    let Some(path) = rate_file else {
        println!("no rate file");
        return;
    };
    let (tx, _rx_cmd) = std::sync::mpsc::channel();
    let (_tx_ev, rx) = std::sync::mpsc::channel();
    let audio_shared = Arc::new(Mutex::new(audio::AudioShared::default()));
    // NVRAM=<file>: start from a copy of saved memory (the original is untouched)
    let nv = std::env::var("NVRAM").ok().map(|f| {
        let tmp = std::env::temp_dir().join("elcotel-nvram-copy.bin");
        std::fs::copy(&f, &tmp).unwrap();
        tmp
    });
    let mut p = phone::Phone::new(PayBoard::new(rom, voice), tx, rx, audio_shared, nv);
    let mut raw = std::fs::read(&path).unwrap();
    // OPEN=1: load the file with the area codes it doesn't list opened
    if std::env::var("OPEN").is_ok() {
        raw = ratefile::RateFile::parse("open", raw.clone()).with_unlisted_open();
    }
    // a custom ROM installs its built-in rates first (as in the app)
    let mut w = 0;
    while (p.defaulting.is_some() || p.presets.is_some() || w < 300) && w < 20000 {
        p.advance(0.01);
        w += 1;
    }
    if std::env::var("NOIMPORT").is_err() {
        p.import_rates(raw.clone());
    }
    let mut shown = 0;
    for _ in 0..20000 {
        p.advance(0.01);
        for l in &p.pnm_log[shown..] {
            println!("PNM {}", l);
        }
        shown = p.pnm_log.len();
        if p.pnm_result.is_some() && !p.importing() {
            break;
        }
    }
    println!("RESULT: {:?} ({})", p.pnm_result, p.pnm_status);
    // What the phone itself now prices calls with (read back from its memory).
    match ratefile::RateFile::from_phone_ram(&p.m.b.ram) {
        Some(r) => {
            for (i, b) in r.bands().iter().enumerate() {
                println!("phone band {}: {}", i + 1, b.describe());
            }
            let c = r.npa_counts();
            println!("phone area codes: {:?}", c.iter().map(|(k, v)| (*k, v.len())).collect::<Vec<_>>());
        }
        None => println!("phone has no rate tables in memory"),
    }
    // 299 is local in this file (25c, dialled as 517-299-xxxx); 313-222 is
    // band 3 (50c for 3 minutes)
    let ld_coins: usize = std::env::var("COINS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    // Settings to try before the calls:
    //   SETNUM=<10 digits>  set the payphone number (stored in the EEPROM)
    //   OPTSET=LLLL:MM,...  OR bits into option bytes (logical C000-CFFF)
    //   ALLOW_OOS=1         clear the out-of-state block (C242 bit 2), stored
    let settle = |p: &mut phone::Phone| {
        let mut w = 0;
        while p.importing() && w < 30000 {
            p.advance(0.01);
            w += 1;
        }
    };
    if let Ok(n) = std::env::var("SETNUM") {
        p.set_phone_number(&n);
        settle(&mut p);
        println!("payphone number: {}", p.phone_number());
    }
    if let Ok(v) = std::env::var("OPTSET") {
        for kv in v.split(',') {
            if let Some((a, m)) = kv.split_once(':') {
                p.m.b.ram[(hex(a) - 0xB000) as usize] |= hex(m) as u8;
            }
        }
        pnm::fix_option_checksums(&mut p.m.b);
    }
    if std::env::var("ALLOW_OOS").is_ok() {
        p.set_out_of_state_blocked(false);
        settle(&mut p);
        println!("out-of-state block now {} ({})", p.out_of_state_blocked(), p.pnm_status);
    }
    // The calls: NUMS=a,b,c (each paid with COINS quarters 18 s in; default
    // a local and a long-distance number), each lasting CALLSECS seconds.
    //   PAYAT=secs      drop another COINS quarters then (overtime)
    //   REVERSE_AT=secs the far end answers (line polarity reverses) then
    //   CALLLOG=1       print the phone's log after each call
    let nums: Vec<(String, usize)> = match std::env::var("NUMS") {
        Ok(v) => v.split(',').map(|n| (n.to_string(), ld_coins)).collect(),
        Err(_) => vec![("2995551".into(), 1), ("3132221234".into(), ld_coins)],
    };
    let secs: usize = std::env::var("CALLSECS").ok().and_then(|v| v.parse().ok()).unwrap_or(45);
    let at = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<usize>().ok()).map(|s| s * 100);
    let (pay_at, reverse_at) = (at("PAYAT"), at("REVERSE_AT"));
    for (num, coins) in nums {
        let num = num.as_str();
        p.set_hook(false);
        for _ in 0..200 {
            p.advance(0.01);
        }
        for c in num.chars() {
            p.press_key(c);
        }
        let before = p.prompts.len();
        for i in 0..secs * 100 {
            p.advance(0.01);
            if i == 1800 || (Some(i) == pay_at && num.len() > 7) {
                for _ in 0..coins {
                    p.drop_coin(2);
                }
            }
            if Some(i) == reverse_at && num.len() > 7 {
                p.m.b.line_reversed = true;
            }
        }
        let pr: Vec<String> = p.prompts.iter().skip(before).map(|x| format!("{:.0}s:{:02X}", x.0, x.1)).collect();
        println!("{} -> sent {} line {:?} (prompts {})", num, p.dialed, p.line, pr.join(" "));
        p.set_hook(true);
        for _ in 0..300 {
            p.advance(0.01);
        }
        if std::env::var("CALLLOG").is_ok() {
            for l in p.log.iter().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
                println!("LOG {:8.2}s {}", l.t, l.text);
            }
        }
    }
}

pub fn app_dir() -> Option<PathBuf> {
    std::env::var("APPDATA").ok().map(|d| PathBuf::from(d).join("ElcotelEmu"))
}

/// Each program ROM keeps its own battery RAM and EEPROM; the stock dump
/// keeps the original file name.
pub fn nvram_for(rom_file: &str) -> Option<PathBuf> {
    let d = app_dir()?;
    if rom_file.eq_ignore_ascii_case(ROM_NAME) {
        return Some(d.join("nvram.bin"));
    }
    let stem: String = rom_file.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    Some(d.join(format!("nvram-{}.bin", stem)))
}

/// Every 64 KB Elcotel program ROM ("PCM 05" signature) in the firmware folders.
pub fn find_roms(dirs: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut v: Vec<(PathBuf, String)> = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if e.metadata().map(|m| m.len() != 0x10000).unwrap_or(true) {
                continue;
            }
            let Ok(data) = std::fs::read(&p) else { continue };
            let ver = phone::rom_version(&data);
            if !ver.starts_with("PCM 05") {
                continue;
            }
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if !v.iter().any(|(q, _)| q.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default() == name) {
                v.push((p, ver));
            }
        }
    }
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

/// Headless check of a ROM with built-in rates (run with ELCOTEL_ROM=<file>).
fn presettest(rom: Vec<u8>, voice: Vec<u8>) {
    let (tx, _rx_cmd) = std::sync::mpsc::channel();
    let (_tx_ev, rx) = std::sync::mpsc::channel();
    let audio_shared = Arc::new(Mutex::new(audio::AudioShared::default()));
    let mut p = phone::Phone::new(PayBoard::new(rom, voice), tx, rx, audio_shared, None);
    println!("firmware: {}  built-in rates: {}", p.firmware_version(), p.has_presets());
    let mut i = 0;
    while (p.defaulting.is_some() || p.presets.is_some()) && i < 10000 {
        p.advance(0.01);
        i += 1;
    }
    for _ in 0..400 {
        p.advance(0.01);
    }
    for l in p.log.iter() {
        println!("{:8.2}s {}", l.t, l.text);
    }
    match ratefile::RateFile::from_phone_ram(&p.m.b.ram) {
        Some(r) => r.bands().iter().enumerate().for_each(|(i, b)| println!("phone band {}: {}", i + 1, b.describe())),
        None => println!("phone has no rate tables in memory"),
    }
    for (num, coins) in [("3132221234", 2), ("18002221234", 0)] {
        p.set_hook(false);
        for _ in 0..200 {
            p.advance(0.01);
        }
        for c in num.chars() {
            p.press_key(c);
        }
        let before = p.prompts.len();
        for i in 0..4500 {
            p.advance(0.01);
            if i == 1800 {
                for _ in 0..coins {
                    p.drop_coin(2);
                }
            }
        }
        let pr: Vec<String> = p
            .prompts
            .iter()
            .skip(before)
            .map(|x| if std::env::var("CALLSECS").is_ok() { format!("{:.0}s:{:02X}", x.0, x.1) } else { format!("{:02X}", x.1) })
            .collect();
        println!("{} -> sent {} line {:?} (prompts {})", num, p.dialed, p.line, pr.join(" "));
        p.set_hook(true);
        for _ in 0..300 {
            p.advance(0.01);
        }
    }
}

/// A second SIP agent calls the payphone; the bell rings, the firmware hears
/// the ring, and lifting the handset answers.
/// Drive the firmware through every situation the emulator can produce and
/// report which code ran, which never did, and how the code uses each port.
fn coveragetest(rom: Vec<u8>, voice: Vec<u8>, rate_file: Option<PathBuf>, out: Option<String>) {
    let (tx, _rx_cmd) = std::sync::mpsc::channel();
    let (_tx_ev, rx) = std::sync::mpsc::channel();
    let audio_shared = Arc::new(Mutex::new(audio::AudioShared::default()));
    let mut p = phone::Phone::new(PayBoard::new(rom.clone(), voice), tx, rx, audio_shared, None);
    p.m.pc_hist = Some(vec![0; 65536]);
    p.m.b.mem_seen = Some(vec![0; 0x40000]);
    let run = |p: &mut phone::Phone, secs: f64| {
        for _ in 0..(secs * 100.0) as usize {
            p.advance(0.01);
        }
    };
    let call = |p: &mut phone::Phone, num: &str, coins: &[usize], secs: f64| {
        p.set_hook(false);
        run(p, 2.0);
        for c in num.chars() {
            p.press_key(c);
        }
        run(p, 8.0);
        for &c in coins {
            p.drop_coin(c);
        }
        run(p, secs);
        p.set_hook(true);
        run(p, 4.0);
    };
    let say = |p: &phone::Phone, what: &str| {
        let hit = p.m.pc_hist.as_ref().unwrap();
        eprintln!("{:7.1}s  {:<40} {} addresses run   clock {}", p.m.seconds(), what, hit.iter().filter(|n| **n > 0).count(), p.m.b.rtc.describe());
    };
    // 1. first start: factory default
    while p.defaulting.is_some() {
        run(&mut p, 0.5);
    }
    run(&mut p, 10.0);
    say(&p, "boot + factory default");
    // 2. rate file over the modem (PNM)
    if let Some(path) = rate_file {
        p.import_rates(std::fs::read(path).unwrap());
        while p.importing() {
            run(&mut p, 0.5);
        }
        run(&mut p, 5.0);
    }
    say(&p, "rate download");
    // 3. calls
    call(&mut p, "2995551", &[1, 1, 0], 45.0);
    say(&p, "local call, dime dime nickel");
    call(&mut p, "3132221234", &[2, 2], 175.0);
    // overtime warning heard: pay it, then let the next period run out
    p.set_hook(false);
    p.set_hook(true);
    say(&p, "long distance (paid period)");
    p.set_hook(false);
    run(&mut p, 2.0);
    for c in "3132221234".chars() {
        p.press_key(c);
    }
    run(&mut p, 8.0);
    p.drop_coin(2);
    p.drop_coin(2);
    run(&mut p, 175.0);
    p.drop_coin(2);
    p.drop_coin(2);
    run(&mut p, 200.0);
    say(&p, "overtime paid, then cut off");
    p.set_hook(true);
    run(&mut p, 4.0);
    for num in [
        "18002221234",
        "911",
        "0",
        "411",
        "611",
        "19005551212",
        "9765551212",
        "011441234567",
        "*",
        "#",
        "5551212",
        "03132221234",
        "01131234567",
        "10102221313",
        "9501234",
        "211",
        "311",
        "511",
        "711",
        "811",
        "18885551212",
        "5172995551",
    ] {
        call(&mut p, num, &[], 20.0);
    }
    say(&p, "free, emergency, operator, refused");
    // coins then hang up before the call (refund)
    p.set_hook(false);
    run(&mut p, 2.0);
    p.drop_coin(2);
    p.drop_coin(0);
    run(&mut p, 3.0);
    p.set_hook(true);
    run(&mut p, 5.0);
    say(&p, "deposit then hang up");
    // far end hangs up mid-call
    p.set_hook(false);
    run(&mut p, 2.0);
    for c in "2995551".chars() {
        p.press_key(c);
    }
    run(&mut p, 8.0);
    p.drop_coin(2);
    run(&mut p, 20.0);
    p.m.b.loop_open = true;
    run(&mut p, 30.0);
    p.set_hook(true);
    run(&mut p, 4.0);
    say(&p, "far end hangs up");
    // hook flash during a call
    p.set_hook(false);
    run(&mut p, 2.0);
    for c in "2995551".chars() {
        p.press_key(c);
    }
    run(&mut p, 8.0);
    p.drop_coin(2);
    run(&mut p, 10.0);
    p.set_hook(true);
    run(&mut p, 0.4);
    p.set_hook(false);
    run(&mut p, 10.0);
    p.set_hook(true);
    run(&mut p, 4.0);
    say(&p, "hook flash");
    // 4. telemetry codes
    p.set_hook(false);
    run(&mut p, 2.0);
    let tc = p.telemetry_code();
    for code in [tc.as_str(), "404", "800", "801", "986", "967", "972", "962", "975", "965", "966", "969", "963"] {
        for c in code.chars() {
            p.press_key(c);
        }
        run(&mut p, 8.0);
    }
    p.set_hook(true);
    run(&mut p, 5.0);
    say(&p, "telemetry codes");
    // 5. ringing: unanswered, then answered
    p.m.b.ring_signal = true;
    run(&mut p, 40.0);
    p.m.b.ring_signal = false;
    run(&mut p, 8.0);
    p.m.b.ring_signal = true;
    run(&mut p, 7.0);
    p.set_hook(false);
    p.m.b.ring_signal = false;
    run(&mut p, 10.0);
    p.set_hook(true);
    run(&mut p, 5.0);
    say(&p, "ringing");
    // 6. housing opened, idle and off hook
    p.m.b.inputs[0x61] &= !0x10;
    run(&mut p, 10.0);
    p.set_hook(false);
    run(&mut p, 5.0);
    p.set_hook(true);
    p.m.b.inputs[0x61] |= 0x10;
    run(&mut p, 10.0);
    say(&p, "housing open");
    // 7. dead line off hook (no line voltage, no loop current)
    p.set_hook(false);
    p.m.b.inputs[0x41] = 0x00;
    p.m.b.loop_open = true;
    run(&mut p, 20.0);
    p.set_hook(true);
    p.m.b.inputs[0x41] = 0x60;
    run(&mut p, 5.0);
    say(&p, "dead line");
    // 8. program button while idle, and a long idle
    p.m.b.program_button = true;
    run(&mut p, 3.0);
    p.m.b.program_button = false;
    run(&mut p, 60.0);
    say(&p, "program button + idle");
    // 9. asleep on hook across midnight: the clock should wake the board
    let s = p.m.b.rtc.secs();
    let target = s.div_euclid(86400) * 86400 + 23 * 3600 + 58 * 60 + 30;
    p.m.b.rtc.offset_secs += target - s;
    let wakes = p.m.rtc_wakes;
    run(&mut p, 240.0);
    eprintln!("clock now {}, clock wake-ups {}, hourly code run {}", p.m.b.rtc.describe(), p.m.rtc_wakes - wakes, p.m.pc_hist.as_ref().unwrap()[0xD043]);
    say(&p, "asleep across midnight");
    // 10. every remote command byte, one PNM call each (logon, password,
    //     command, a few seconds of reply, hang up)
    let mut probes: Vec<(u8, Vec<u8>, usize)> = Vec::new();
    let only: Option<Vec<u8>> = std::env::var("PROBE").ok().map(|v| v.split(',').filter_map(|x| u8::from_str_radix(x, 16).ok()).collect());
    if std::env::var("NOPROBE").is_err() {
        for cmd in 0..=255u8 {
            if only.as_ref().map(|o| !o.contains(&cmd)).unwrap_or(false) {
                continue;
            }
            let before = p.m.pc_hist.as_ref().unwrap().iter().filter(|n| **n > 0).count();
            let mut w = 0;
            while !p.start_pnm_probe(cmd) && w < 30 {
                if !p.m.b.on_hook() {
                    p.set_hook(true);
                }
                run(&mut p, 1.0);
                w += 1;
            }
            let mut w = 0;
            while p.importing() && w < 120 {
                run(&mut p, 0.5);
                w += 1;
            }
            run(&mut p, 1.0);
            let after = p.m.pc_hist.as_ref().unwrap().iter().filter(|n| **n > 0).count();
            let reply = p.last_probe.as_ref().map(|l| l.1.clone()).unwrap_or_default();
            probes.push((cmd, reply, after - before));
        }
        say(&p, "remote commands 00-FF probed");
    }

    let hit = p.m.pc_hist.take().unwrap();
    let vb = ((p.m.z.cpu.i as u16) << 8) | (p.m.z.io.il as u16 & 0xE0);
    let mut roots = coverage::roots(&rom, Some(vb));
    roots.extend((0..65536usize).filter(|a| hit[*a] > 0).map(|a| a as u16));
    let sc = coverage::analyse(&rom, &roots);
    let (ran, total) = coverage::percent(&sc, &hit);
    let mut r = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(r, "Code coverage: {} of {} instructions found in the ROM have run ({:.1}%)", ran, total, ran as f64 * 100.0 / total.max(1) as f64);
    let _ = writeln!(r, "Interrupt vector table at {:04X}", vb);
    if !probes.is_empty() {
        let answered = probes.iter().filter(|p| !p.1.is_empty()).count();
        let acked = probes.iter().filter(|p| p.1.first() == Some(&0x06)).count();
        let _ = writeln!(r, "\nRemote commands probed: {} (answered {}, first byte ACK 06: {})", probes.len(), answered, acked);
        let _ = writeln!(r, "  cmd  new instr  reply (first 24 bytes)");
        for (cmd, reply, new) in &probes {
            if reply.is_empty() && *new == 0 {
                continue;
            }
            let hx: Vec<String> = reply.iter().take(24).map(|b| format!("{:02X}", b)).collect();
            let _ = writeln!(
                r,
                "  {:02X}   {:6}     {}{}",
                cmd,
                new,
                hx.join(" "),
                if reply.len() > 24 { format!(" ... ({} bytes)", reply.len()) } else { String::new() }
            );
        }
        let silent: Vec<String> = probes.iter().filter(|p| p.1.is_empty() && p.2 == 0).map(|p| format!("{:02X}", p.0)).collect();
        let _ = writeln!(r, "  no reply and no new code: {}", silent.join(" "));
    }
    let _ = writeln!(r, "\nPorts used by the code (sites run / sites in code):");
    for row in coverage::port_map(&sc, &hit) {
        let stat = if row.internal { None } else { p.m.b.stats.get(&(row.port as u16)) };
        let _ = writeln!(
            r,
            "  {}{:02X}  read {}/{}  write {}/{}  runtime reads {} writes {}",
            if row.internal { "int " } else { "ext " },
            row.port,
            row.read_sites_run,
            row.read_sites,
            row.write_sites_run,
            row.write_sites,
            stat.map(|s| s.reads).unwrap_or(0),
            stat.map(|s| s.writes).unwrap_or(0)
        );
    }
    let _ = writeln!(
        r,
        "
I/O instructions never run (external ports):"
    );
    for s in sc.io.iter().filter(|s| !s.internal && hit[s.pc as usize] == 0) {
        let _ = writeln!(r, "  {:04X} {} {}", s.pc, if s.write { "out" } else { "in " }, s.port.map(|p| format!("{:02X}", p)).unwrap_or("(C)".into()));
    }
    let regions = coverage::unvisited(&sc, &hit);
    let _ = writeln!(r, "\nCode never run ({} pieces), largest first:", regions.len());
    for g in regions.iter().take(150) {
        let callers: Vec<String> = g.callers.iter().take(6).map(|c| format!("{:04X}", c)).collect();
        let ports: Vec<String> = g.ports.iter().map(|(p, w)| format!("{}{:02X}", if *w { "out " } else { "in " }, p)).collect();
        let _ =
            writeln!(r, "  {:04X}-{:04X} {:5} bytes  from [{}]  ports [{}]", g.start, g.end, g.end.wrapping_sub(g.start), callers.join(" "), ports.join(", "));
    }
    // The frontier: unrun code right next to code that did run - a branch the
    // firmware reached but never took. These are the decisions to explain.
    let rd = |a: u16| rom[a as usize];
    let ran = |a: u16| hit[a as usize] > 0;
    let show = |pc: u16| -> String {
        let mut v: Vec<u16> = sc.insns.range(..pc).rev().take(4).map(|(a, _)| *a).collect();
        v.reverse();
        v.push(pc);
        v.iter().map(|a| format!("{:04X} {}", a, disasm::disasm(&rd, *a).0)).collect::<Vec<_>>().join(" | ")
    };
    let mut frontier: Vec<(u16, u16, u16)> = Vec::new();
    for g in &regions {
        let size = g.end.wrapping_sub(g.start);
        if size < 12 {
            continue;
        }
        for c in g.callers.iter().filter(|c| ran(**c)) {
            frontier.push((size, g.start, *c));
        }
        if let Some((prev, len)) = sc.insns.range(..g.start).next_back() {
            let t = disasm::disasm(&rd, *prev).0;
            if prev.wrapping_add(*len) == g.start
                && ran(*prev)
                && (t.contains(',') && (t.starts_with("J") || t.starts_with("CALL") || t.starts_with("RET")) || t.starts_with("DJNZ") || t.starts_with("RET "))
            {
                frontier.push((size, g.start, *prev));
            }
        }
    }
    frontier.sort_by_key(|f| std::cmp::Reverse(f.0));
    let _ = writeln!(r, "\nFrontier: unrun code reached by a branch that did run ({}), largest first:", frontier.len());
    for (size, start, at) in frontier.iter().take(120) {
        let _ = writeln!(r, "  {:04X} ({} bytes) via {:04X}:  {}", start, size, at, show(*at));
    }
    if let Some(m) = &p.m.b.mem_seen {
        let _ = writeln!(
            r,
            "
Memory by 4 KB block (written / only read / never touched):"
        );
        for base in (0x20000..0x2A000usize).step_by(0x1000).chain((0x30000..0x40000usize).step_by(0x1000)) {
            let s = &m[base..base + 0x1000];
            let w = s.iter().filter(|x| **x & 2 != 0).count();
            let rd = s.iter().filter(|x| **x == 1).count();
            let none = s.iter().filter(|x| **x == 0).count();
            let _ = writeln!(r, "  {:05X}  {:5} {:5} {:5}", base, w, rd, none);
        }
    }
    print!("{}", r);
    if let Some(o) = out {
        let _ = std::fs::write(&o, &r);
        let hits: Vec<u8> = hit.iter().map(|n| (*n > 0) as u8).collect();
        let _ = std::fs::write(format!("{}.hits", o), hits);
    }
}

fn incomingtest(rom: Vec<u8>, voice: Vec<u8>) {
    use sip::{SipAgent, SipCmd, SipConfig, SipEvent};
    let audio_shared = Arc::new(Mutex::new(audio::AudioShared::default()));
    let caller_audio = Arc::new(Mutex::new(sip::CallAudio::default()));
    let (ctx, crx) = std::sync::mpsc::channel();
    let (cetx, cerx) = std::sync::mpsc::channel();
    SipAgent::spawn(crx, cetx, caller_audio);
    ctx.send(SipCmd::Configure(SipConfig {
        enabled: true,
        server: "127.0.0.1:5071".into(),
        username: "caller".into(),
        local_port: 5073,
        register: false,
        ..Default::default()
    }))
    .unwrap();
    let call_audio = Arc::new(Mutex::new(sip::CallAudio::default()));
    let (ptx, prx) = std::sync::mpsc::channel();
    let (petx, perx) = std::sync::mpsc::channel();
    SipAgent::spawn(prx, petx, call_audio);
    ptx.send(SipCmd::Configure(SipConfig {
        enabled: true,
        server: "127.0.0.1:5073".into(),
        username: "payphone".into(),
        local_port: 5071,
        register: false,
        ..Default::default()
    }))
    .unwrap();
    let mut p = phone::Phone::new(PayBoard::new(rom, voice), ptx, perx, audio_shared.clone(), None);
    p.sip_enabled = true;
    p.ring_firmware = true;
    let step = |p: &mut phone::Phone, secs: f64| {
        for _ in 0..(secs * 100.0) as usize {
            p.advance(0.01);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    // factory default (+ presets on a custom ROM), then let it go to sleep
    while p.defaulting.is_some() || p.presets.is_some() {
        step(&mut p, 0.5);
    }
    step(&mut p, 6.0);
    println!("before the call: board asleep = {}", p.m.asleep);
    ctx.send(SipCmd::Dial("payphone".into())).unwrap();
    let wakes0 = p.m.wake_count;
    step(&mut p, 9.0);
    println!("ringing: line = {:?}, bell = {}, firmware woke {} times for the ring", p.line, audio_shared.lock().unwrap().ringer, p.m.wake_count - wakes0);
    p.set_hook(false);
    step(&mut p, 3.0);
    let mut caller_connected = false;
    while let Ok(e) = cerx.try_recv() {
        if e == SipEvent::Answered {
            caller_connected = true;
        }
    }
    println!("after lifting the handset: line = {:?}, caller connected = {}", p.line, caller_connected);
    let pr: Vec<String> = p
        .prompts
        .iter()
        .rev()
        .take(6)
        .map(|x| if std::env::var("CALLSECS").is_ok() { format!("{:.0}s:{:02X}", x.0, x.1) } else { format!("{:02X}", x.1) })
        .collect();
    println!("recent prompts: {}", pr.join(" "));
    p.set_hook(true);
    step(&mut p, 1.0);
    println!(
        "RESULT: {}",
        if caller_connected && p.line != phone::LineState::IncomingRinging { "PASS - incoming call rang the phone and was answered" } else { "FAIL" }
    );
}

/// PNM-style dial-in over SIP: a caller with a Bell 103 modem rings the
/// payphone; the firmware's modem answers and replies to the logon frame.
fn modemtest(rom: Vec<u8>, voice: Vec<u8>) {
    use sip::{SipAgent, SipCmd, SipConfig, SipEvent};
    let audio_shared = Arc::new(Mutex::new(audio::AudioShared::default()));
    let caller_audio = Arc::new(Mutex::new(sip::CallAudio::default()));
    let (ctx, crx) = std::sync::mpsc::channel();
    let (cetx, cerx) = std::sync::mpsc::channel();
    SipAgent::spawn(crx, cetx, caller_audio.clone());
    ctx.send(SipCmd::Configure(SipConfig {
        enabled: true,
        server: "127.0.0.1:5071".into(),
        username: "pnm".into(),
        local_port: 5074,
        register: false,
        ..Default::default()
    }))
    .unwrap();
    let call_audio = Arc::new(Mutex::new(sip::CallAudio::default()));
    let (ptx, prx) = std::sync::mpsc::channel();
    let (petx, perx) = std::sync::mpsc::channel();
    SipAgent::spawn(prx, petx, call_audio.clone());
    ptx.send(SipCmd::Configure(SipConfig {
        enabled: true,
        server: "127.0.0.1:5074".into(),
        username: "payphone".into(),
        local_port: 5071,
        register: false,
        ..Default::default()
    }))
    .unwrap();
    let mut p = phone::Phone::new(PayBoard::new(rom, voice), ptx, perx, audio_shared, None);
    p.sip_enabled = true;
    p.ring_firmware = true;
    p.call_audio = Some(call_audio);
    let dpsk = std::env::var("MODEM").map(|v| v == "1200").unwrap_or(false);
    let mut caller = modem::LineModem::new_originate(dpsk);
    println!("caller modem: {}", if dpsk { "1200 bit/s DPSK" } else { "300 bit/s FSK" });
    let mut answered = false;
    let mut sent_logon = false;
    let mut t_answer = 0.0;
    let mut got: Vec<u8> = Vec::new();
    let tick = |p: &mut phone::Phone, caller: &mut modem::LineModem, answered: bool| -> Vec<u8> {
        p.advance(0.01);
        std::thread::sleep(std::time::Duration::from_millis(9));
        if !answered {
            return Vec::new();
        }
        let mut c = caller_audio.lock().unwrap();
        let rx: Vec<f32> = c.from_line.drain(..).collect();
        let bytes = caller.demodulate(&rx);
        let tx = caller.modulate(80);
        c.to_line.extend(tx);
        bytes
    };
    while p.defaulting.is_some() || p.presets.is_some() {
        tick(&mut p, &mut caller, false);
    }
    for _ in 0..500 {
        tick(&mut p, &mut caller, false);
    }
    println!("calling the payphone (firmware {})", p.firmware_version());
    ctx.send(SipCmd::Dial("payphone".into())).unwrap();
    for _ in 0..9000 {
        let rxb = tick(&mut p, &mut caller, answered);
        if sent_logon {
            got.extend(rxb);
        }
        if answered && t_answer > 0.0 && !(caller.carrier() && p.m.b.inputs[0xC2] == 0x28) {
            t_answer = p.m.seconds();
        }
        while let Ok(e) = cerx.try_recv() {
            if e == SipEvent::Answered {
                answered = true;
                t_answer = p.m.seconds();
                println!("{:7.2}s the payphone answered (line state {:?})", p.m.seconds(), p.line);
            }
        }
        if answered && !sent_logon && p.m.seconds() - t_answer > 7.0 {
            println!(
                "{:7.2}s caller carrier seen by the phone: {}; caller sees the phone: {}; sending the logon frame",
                p.m.seconds(),
                p.m.b.inputs[0xC2] == 0x28,
                caller.carrier()
            );
            let id = p.station_id();
            println!("        logging on with station id {}", id);
            caller.send_byte(0x02);
            for c in id.chars() {
                caller.send_byte(c.to_digit(10).unwrap_or(0) as u8);
            }
            caller.send_byte(0x03);
            sent_logon = true;
        }
        if got.contains(&0x07) {
            break;
        }
    }
    for l in p.log.iter().filter(|l| l.text.contains("modem") || l.text.contains("Data call") || l.text.contains("carrier") || l.text.contains("recognised")) {
        println!("{:8.2}s {}", l.t, l.text);
    }
    println!("bytes the caller received: {:02X?}", got);
    println!("RESULT: {}", if got.contains(&0x07) { "PASS - the firmware answered the data call and accepted the logon over the modem" } else { "FAIL" });
    let _ = ctx.send(SipCmd::Hangup);
}
