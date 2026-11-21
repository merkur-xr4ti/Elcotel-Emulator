//! Z80/Z180 disassembler used by the debugger panel and the trace tools.

const R: [&str; 8] = ["B", "C", "D", "E", "H", "L", "(HL)", "A"];
const RP: [&str; 4] = ["BC", "DE", "HL", "SP"];
const RP2: [&str; 4] = ["BC", "DE", "HL", "AF"];
const CC: [&str; 8] = ["NZ", "Z", "NC", "C", "PO", "PE", "P", "M"];
const ALU: [&str; 8] = ["ADD A,", "ADC A,", "SUB ", "SBC A,", "AND ", "XOR ", "OR ", "CP "];
const ROT: [&str; 8] = ["RLC", "RRC", "RL", "RR", "SLA", "SRA", "SLL", "SRL"];

/// Disassemble one instruction at `pc`. `rd` reads a byte at a logical address.
/// Returns (text, length).
pub fn disasm(rd: &dyn Fn(u16) -> u8, pc: u16) -> (String, u16) {
    let mut len: u16 = 0;
    let mut next = || {
        let v = rd(pc.wrapping_add(len));
        len += 1;
        v
    };
    let mut op = next();
    let mut idx: Option<&str> = None;
    while op == 0xDD || op == 0xFD {
        idx = Some(if op == 0xDD { "IX" } else { "IY" });
        op = next();
    }
    let text = if op == 0xCB {
        let (d, o) = if idx.is_some() {
            let d = next() as i8;
            (Some(d), next())
        } else {
            (None, next())
        };
        let y = (o >> 3) & 7;
        let z = o & 7;
        let reg = match (idx, d) {
            (Some(i), Some(d)) => format!("({}{:+})", i, d),
            _ => R[z as usize].to_string(),
        };
        match o >> 6 {
            0 => format!("{} {}", ROT[y as usize], reg),
            1 => format!("BIT {},{}", y, reg),
            2 => format!("RES {},{}", y, reg),
            _ => format!("SET {},{}", y, reg),
        }
    } else if op == 0xED {
        let o = next();
        let y = ((o >> 3) & 7) as usize;
        let z = o & 7;
        let p = y >> 1;
        let q = y & 1;
        match o >> 6 {
            0 => match z {
                0 => {
                    let n = next();
                    if y == 6 {
                        format!("IN0 (0x{:02X})", n)
                    } else {
                        format!("IN0 {},(0x{:02X})", R[y], n)
                    }
                }
                1 => {
                    let n = next();
                    format!("OUT0 (0x{:02X}),{}", n, R[y])
                }
                4 => format!("TST {}", R[y]),
                _ => format!("DB 0xED,0x{:02X}", o),
            },
            1 => match z {
                0 => {
                    if y == 6 {
                        "IN (C)".into()
                    } else {
                        format!("IN {},(C)", R[y])
                    }
                }
                1 => format!("OUT (C),{}", if y == 6 { "0" } else { R[y] }),
                2 => format!("{} HL,{}", if q == 0 { "SBC" } else { "ADC" }, RP[p]),
                3 => {
                    let lo = next() as u16;
                    let hi = next() as u16;
                    let a = hi << 8 | lo;
                    if q == 0 {
                        format!("LD (0x{:04X}),{}", a, RP[p])
                    } else {
                        format!("LD {},(0x{:04X})", RP[p], a)
                    }
                }
                4 => match o {
                    0x4C | 0x5C | 0x6C | 0x7C => format!("MLT {}", RP[p]),
                    0x64 => format!("TST 0x{:02X}", next()),
                    0x74 => format!("TSTIO 0x{:02X}", next()),
                    _ => "NEG".into(),
                },
                5 => {
                    if y == 1 {
                        "RETI".into()
                    } else {
                        "RETN".into()
                    }
                }
                6 if o == 0x76 => "SLP".into(),
                6 => format!("IM {}", [0, 0, 1, 2, 0, 0, 1, 2][y]),
                _ => match y {
                    0 => "LD I,A".into(),
                    1 => "LD R,A".into(),
                    2 => "LD A,I".into(),
                    3 => "LD A,R".into(),
                    4 => "RRD".into(),
                    5 => "RLD".into(),
                    6 => "SLP".into(),
                    _ => "NOP*".into(),
                },
            },
            2 => match o {
                0xA0 => "LDI".into(),
                0xA1 => "CPI".into(),
                0xA2 => "INI".into(),
                0xA3 => "OUTI".into(),
                0xA8 => "LDD".into(),
                0xA9 => "CPD".into(),
                0xAA => "IND".into(),
                0xAB => "OUTD".into(),
                0xB0 => "LDIR".into(),
                0xB1 => "CPIR".into(),
                0xB2 => "INIR".into(),
                0xB3 => "OTIR".into(),
                0xB8 => "LDDR".into(),
                0xB9 => "CPDR".into(),
                0xBA => "INDR".into(),
                0xBB => "OTDR".into(),
                0x83 => "OTIM".into(),
                0x8B => "OTDM".into(),
                0x93 => "OTIMR".into(),
                0x9B => "OTDMR".into(),
                _ => format!("DB 0xED,0x{:02X}", o),
            },
            _ => format!("DB 0xED,0x{:02X}", o),
        }
    } else {
        let x = op >> 6;
        let y = ((op >> 3) & 7) as usize;
        let z = op & 7;
        let p = y >> 1;
        let q = y & 1;
        let hl = idx.unwrap_or("HL");
        let mem = |len_next: &mut dyn FnMut() -> u8| -> String {
            match idx {
                Some(i) => format!("({}{:+})", i, len_next() as i8),
                None => "(HL)".into(),
            }
        };
        let reg = |r: usize| -> String {
            match (r, idx) {
                (4, Some(i)) => format!("{}H", i),
                (5, Some(i)) => format!("{}L", i),
                _ => R[r].to_string(),
            }
        };
        let rp = |p: usize| if p == 2 { hl.to_string() } else { RP[p].to_string() };
        match x {
            0 => match z {
                0 => match y {
                    0 => "NOP".into(),
                    1 => "EX AF,AF'".into(),
                    2 => {
                        let d = next() as i8;
                        format!("DJNZ 0x{:04X}", pc.wrapping_add(2).wrapping_add(d as i16 as u16))
                    }
                    3 => {
                        let d = next() as i8;
                        format!("JR 0x{:04X}", pc.wrapping_add(2).wrapping_add(d as i16 as u16))
                    }
                    _ => {
                        let d = next() as i8;
                        format!("JR {},0x{:04X}", CC[y - 4], pc.wrapping_add(2).wrapping_add(d as i16 as u16))
                    }
                },
                1 => {
                    if q == 0 {
                        let lo = next() as u16;
                        let hi = next() as u16;
                        format!("LD {},0x{:04X}", rp(p), hi << 8 | lo)
                    } else {
                        format!("ADD {},{}", hl, rp(p))
                    }
                }
                2 => match (q, p) {
                    (0, 0) => "LD (BC),A".into(),
                    (0, 1) => "LD (DE),A".into(),
                    (1, 0) => "LD A,(BC)".into(),
                    (1, 1) => "LD A,(DE)".into(),
                    _ => {
                        let lo = next() as u16;
                        let hi = next() as u16;
                        let a = hi << 8 | lo;
                        match (q, p) {
                            (0, 2) => format!("LD (0x{:04X}),{}", a, hl),
                            (0, _) => format!("LD (0x{:04X}),A", a),
                            (1, 2) => format!("LD {},(0x{:04X})", hl, a),
                            _ => format!("LD A,(0x{:04X})", a),
                        }
                    }
                },
                3 => format!("{} {}", if q == 0 { "INC" } else { "DEC" }, rp(p)),
                4 | 5 => {
                    let n = if z == 4 { "INC" } else { "DEC" };
                    if y == 6 {
                        format!("{} {}", n, mem(&mut next))
                    } else {
                        format!("{} {}", n, reg(y))
                    }
                }
                6 => {
                    if y == 6 {
                        let m = mem(&mut next);
                        format!("LD {},0x{:02X}", m, next())
                    } else {
                        format!("LD {},0x{:02X}", reg(y), next())
                    }
                }
                _ => ["RLCA", "RRCA", "RLA", "RRA", "DAA", "CPL", "SCF", "CCF"][y].into(),
            },
            1 => {
                if y == 6 && z == 6 {
                    "HALT".into()
                } else if y == 6 {
                    format!("LD {},{}", mem(&mut next), R[z as usize])
                } else if z == 6 {
                    format!("LD {},{}", R[y], mem(&mut next))
                } else {
                    format!("LD {},{}", reg(y), reg(z as usize))
                }
            }
            2 => {
                if z == 6 {
                    format!("{}{}", ALU[y], mem(&mut next))
                } else {
                    format!("{}{}", ALU[y], reg(z as usize))
                }
            }
            _ => match z {
                0 => format!("RET {}", CC[y]),
                1 => {
                    if q == 0 {
                        format!("POP {}", if p == 2 { hl } else { RP2[p] })
                    } else {
                        match p {
                            0 => "RET".into(),
                            1 => "EXX".into(),
                            2 => format!("JP ({})", hl),
                            _ => format!("LD SP,{}", hl),
                        }
                    }
                }
                2 => {
                    let lo = next() as u16;
                    let hi = next() as u16;
                    format!("JP {},0x{:04X}", CC[y], hi << 8 | lo)
                }
                3 => match y {
                    0 => {
                        let lo = next() as u16;
                        let hi = next() as u16;
                        format!("JP 0x{:04X}", hi << 8 | lo)
                    }
                    2 => format!("OUT (0x{:02X}),A", next()),
                    3 => format!("IN A,(0x{:02X})", next()),
                    4 => format!("EX (SP),{}", hl),
                    5 => "EX DE,HL".into(),
                    6 => "DI".into(),
                    7 => "EI".into(),
                    _ => "??".into(),
                },
                4 => {
                    let lo = next() as u16;
                    let hi = next() as u16;
                    format!("CALL {},0x{:04X}", CC[y], hi << 8 | lo)
                }
                5 => {
                    if q == 0 {
                        format!("PUSH {}", if p == 2 { hl } else { RP2[p] })
                    } else {
                        let lo = next() as u16;
                        let hi = next() as u16;
                        format!("CALL 0x{:04X}", hi << 8 | lo)
                    }
                }
                6 => format!("{}0x{:02X}", ALU[y], next()),
                _ => format!("RST 0x{:02X}", y * 8),
            },
        }
    };
    (text, len)
}
