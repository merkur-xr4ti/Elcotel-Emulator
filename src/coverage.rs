//! Code and I/O coverage: which parts of the program ROM the emulator has
//! run, and which I/O ports the firmware's code uses.
//!
//! The static side follows the code from its entry points (reset, restarts,
//! the interrupt vectors, the firmware's timer and sequencer jump tables and
//! every address seen running) and records each IN/OUT instruction with its
//! port. Comparing it with the instructions actually executed shows the code
//! the emulator never reached and the ports nobody has driven yet.

use crate::disasm;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct IoSite {
    pub pc: u16,
    pub port: Option<u8>,
    pub write: bool,
    /// IN0/OUT0: Z180 internal register page.
    pub internal: bool,
}

pub struct StaticCode {
    /// Instruction start -> length.
    pub insns: BTreeMap<u16, u16>,
    /// Calls and jumps: target -> sites.
    pub xrefs: BTreeMap<u16, Vec<u16>>,
    pub io: Vec<IoSite>,
}

fn target(text: &str) -> Option<u16> {
    let i = text.rfind("0x")?;
    u16::from_str_radix(text[i + 2..].trim_end_matches(')'), 16).ok()
}

/// The entry points the CPU and the firmware's own tables give.
pub fn roots(rom: &[u8], vector_base: Option<u16>) -> Vec<u16> {
    let w = |a: usize| rom.get(a).copied().unwrap_or(0) as u16 | (rom.get(a + 1).copied().unwrap_or(0) as u16) << 8;
    let mut r = vec![0x0000, 0x0066];
    r.extend((1..8).map(|k| k * 8));
    // software timer handlers (28 words at 0x4078) and the line sequencer
    // steps (0xD331, indexed by F20A < 0x21)
    r.extend((0..28).map(|k| w(0x4078 + k * 2)));
    r.extend((0..0x21).map(|k| w(0xD331 + k * 2)));
    if let Some(v) = vector_base {
        r.extend((0..9).map(|k| w(v as usize + k * 2)));
    }
    r.retain(|a| (*a as usize) < rom.len());
    r
}

pub fn analyse(rom: &[u8], roots: &[u16]) -> StaticCode {
    let rd = |a: u16| rom.get(a as usize).copied().unwrap_or(0xFF);
    let mut insns = BTreeMap::new();
    let mut xrefs: BTreeMap<u16, Vec<u16>> = BTreeMap::new();
    let mut io = Vec::new();
    let mut todo: Vec<u16> = roots.to_vec();
    let mut seen = BTreeSet::new();
    while let Some(start) = todo.pop() {
        let mut pc = start;
        loop {
            if !seen.insert(pc) || pc as usize >= rom.len() {
                break;
            }
            let (t, len) = disasm::disasm(&rd, pc);
            insns.insert(pc, len);
            let m = t.split_whitespace().next().unwrap_or("");
            // I/O
            let b0 = rd(pc);
            let b1 = rd(pc.wrapping_add(1));
            let b2 = rd(pc.wrapping_add(2));
            match (b0, b1) {
                (0xDB, p) => io.push(IoSite { pc, port: Some(p), write: false, internal: false }),
                (0xD3, p) => io.push(IoSite { pc, port: Some(p), write: true, internal: false }),
                (0xED, x) if x & 0xC7 == 0x00 && x != 0x30 => io.push(IoSite { pc, port: Some(b2), write: false, internal: true }),
                (0xED, x) if x & 0xC7 == 0x01 && x != 0x31 => io.push(IoSite { pc, port: Some(b2), write: true, internal: true }),
                (0xED, x) if x & 0xC7 == 0x40 => io.push(IoSite { pc, port: None, write: false, internal: false }),
                (0xED, x) if x & 0xC7 == 0x41 => io.push(IoSite { pc, port: None, write: true, internal: false }),
                _ => {}
            }
            let next = pc.wrapping_add(len);
            let cond = t.contains(',') || m == "DJNZ";
            match m {
                "JP" | "JR" | "CALL" | "DJNZ" | "RST" => {
                    if let Some(to) = target(&t).filter(|_| !t.contains('(')) {
                        xrefs.entry(to).or_default().push(pc);
                        todo.push(to);
                    }
                    let stops = (m == "JP" || m == "JR") && !cond;
                    if stops {
                        break;
                    }
                }
                "RET" if !t.contains(' ') => break,
                "RETI" | "RETN" | "DB" => break,
                _ => {}
            }
            if m == "DB" {
                break;
            }
            pc = next;
        }
    }
    StaticCode { insns, xrefs, io }
}

/// Code the emulator never ran: runs of static instructions none of which
/// executed, with their size, who jumps to them and the ports they use.
pub struct Region {
    pub start: u16,
    pub end: u16,
    pub callers: Vec<u16>,
    pub ports: Vec<(u8, bool)>,
}

pub fn unvisited(sc: &StaticCode, hit: &[u32]) -> Vec<Region> {
    let mut out: Vec<Region> = Vec::new();
    let mut cur: Option<(u16, u16)> = None;
    let flush = |cur: &mut Option<(u16, u16)>, out: &mut Vec<Region>| {
        if let Some((s, e)) = cur.take() {
            let inside = |a: u16| a >= s && (a < e || e <= s);
            let callers: Vec<u16> = sc.xrefs.iter().filter(|(k, _)| inside(**k)).flat_map(|(_, v)| v.iter().copied()).filter(|c| !inside(*c)).collect();
            let mut ports: Vec<(u8, bool)> =
                sc.io.iter().filter(|x| x.pc >= s && x.pc < e && !x.internal).filter_map(|x| x.port.map(|p| (p, x.write))).collect();
            ports.sort();
            ports.dedup();
            out.push(Region { start: s, end: e, callers, ports });
        }
    };
    for (&pc, &len) in &sc.insns {
        let ran = hit.get(pc as usize).map(|n| *n > 0).unwrap_or(false);
        if ran {
            flush(&mut cur, &mut out);
            continue;
        }
        cur = match cur {
            Some((s, e)) if e == pc => Some((s, pc.wrapping_add(len))),
            Some(_) => {
                flush(&mut cur, &mut out);
                Some((pc, pc.wrapping_add(len)))
            }
            None => Some((pc, pc.wrapping_add(len))),
        };
    }
    flush(&mut cur, &mut out);
    out.sort_by_key(|r| std::cmp::Reverse(r.end.wrapping_sub(r.start)));
    out
}

/// One row of the I/O map: what the code does with a port and what the
/// emulator saw.
pub struct PortRow {
    pub port: u8,
    pub internal: bool,
    pub read_sites: usize,
    pub write_sites: usize,
    pub read_sites_run: usize,
    pub write_sites_run: usize,
}

pub fn port_map(sc: &StaticCode, hit: &[u32]) -> Vec<PortRow> {
    let mut m: BTreeMap<(bool, u8), PortRow> = BTreeMap::new();
    for s in &sc.io {
        let Some(p) = s.port else { continue };
        let r =
            m.entry((s.internal, p)).or_insert(PortRow { port: p, internal: s.internal, read_sites: 0, write_sites: 0, read_sites_run: 0, write_sites_run: 0 });
        let ran = hit.get(s.pc as usize).map(|n| *n > 0).unwrap_or(false);
        if s.write {
            r.write_sites += 1;
            r.write_sites_run += ran as usize;
        } else {
            r.read_sites += 1;
            r.read_sites_run += ran as usize;
        }
    }
    m.into_values().collect()
}

/// Share of the static instructions that have run.
pub fn percent(sc: &StaticCode, hit: &[u32]) -> (usize, usize) {
    let total = sc.insns.len();
    let ran = sc.insns.keys().filter(|pc| hit.get(**pc as usize).map(|n| *n > 0).unwrap_or(false)).count();
    (ran, total)
}

/// The frontier: unrun code (at least `min` bytes) next to code that did run,
/// as (size, start, deciding instruction). A branch the firmware reached but
/// never took is the decision to explain.
pub fn frontier(sc: &StaticCode, regions: &[Region], rom: &[u8], hit: &[u32], min: u16) -> Vec<(u16, u16, u16)> {
    let rd = |a: u16| rom.get(a as usize).copied().unwrap_or(0xFF);
    let ran = |a: u16| hit.get(a as usize).map(|n| *n > 0).unwrap_or(false);
    let mut out = Vec::new();
    for g in regions {
        let size = g.end.wrapping_sub(g.start);
        if size < min {
            continue;
        }
        for c in g.callers.iter().filter(|c| ran(**c)) {
            out.push((size, g.start, *c));
        }
        if let Some((prev, len)) = sc.insns.range(..g.start).next_back() {
            let t = disasm::disasm(&rd, *prev).0;
            let conditional = (t.contains(',') && (t.starts_with('J') || t.starts_with("CALL"))) || t.starts_with("DJNZ") || t.starts_with("RET ");
            if prev.wrapping_add(*len) == g.start && ran(*prev) && conditional {
                out.push((size, g.start, *prev));
            }
        }
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.0));
    out
}

/// The deciding instruction with the few before it, as one line.
pub fn context(sc: &StaticCode, rom: &[u8], pc: u16) -> String {
    let rd = |a: u16| rom.get(a as usize).copied().unwrap_or(0xFF);
    let mut v: Vec<u16> = sc.insns.range(..pc).rev().take(3).map(|(a, _)| *a).collect();
    v.reverse();
    v.push(pc);
    v.iter().map(|a| format!("{:04X} {}", a, disasm::disasm(&rd, *a).0)).collect::<Vec<_>>().join("  |  ")
}

/// What is known about why a piece of code hasn't run (by its start).
pub fn why_unrun(start: u16) -> Option<&'static str> {
    Some(match start {
        0x2F6C | 0x2F8B => "Only with a newer program image in the upgrade area at 0x20000 - none on this phone.",
        0x80DE => "Dialling patterns from a PNM options file (B400) - none has been sent to this phone.",
        0xB577 | 0xB520 => "Answer supervision by line polarity reversal - an option this phone's settings leave off.",
        0xD043 => "Hourly jobs at minute 00: they run when the clock wakes the board on the hour.",
        0xCD8E | 0xCD29 | 0xCDDC => "Call-progress tone, SIT-tone and voice detectors: the emulated line doesn't drive them.",
        0x63E4 => "Prompts numbered 78h and up, copied from the voice ROM's upper half.",
        0x8671 | 0x6DD0 => "Depends on an option bit (C240-C24F) this phone's settings leave off.",
        0xD331 => "A jump table (data), not code.",
        _ => return None,
    })
}
