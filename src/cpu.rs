//! Zilog Z80 / Hitachi HD64180 (Z180) instruction core.
//!
//! The firmware in the payphone is Z180 code (it uses MLT, IN0/OUT0 and the
//! Z180 internal interrupt vector table at 0x0020). This module executes the
//! full documented Z80 instruction set plus the Z180 extensions. Memory and
//! I/O go through the `CpuBus` trait; the Z180 on-chip peripherals (MMU,
//! timers, serial ports) live in `z180.rs` which implements the bus for the
//! core and forwards external cycles to the board.

pub const FS: u8 = 0x80;
pub const FZ: u8 = 0x40;
pub const FY: u8 = 0x20;
pub const FH: u8 = 0x10;
pub const FX: u8 = 0x08;
pub const FP: u8 = 0x04;
pub const FN: u8 = 0x02;
pub const FC: u8 = 0x01;

pub trait CpuBus {
    fn mem_read(&mut self, addr: u16) -> u8;
    fn mem_write(&mut self, addr: u16, v: u8);
    fn io_read(&mut self, port: u16) -> u8;
    fn io_write(&mut self, port: u16, v: u8);
    /// Opcode fetch (M1). Defaults to a normal read.
    fn fetch(&mut self, addr: u16) -> u8 {
        self.mem_read(addr)
    }
    /// Called for RETI so daisy-chained peripherals could react.
    fn reti(&mut self) {}
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Idx {
    HL,
    IX,
    IY,
}

#[derive(Clone, Debug, Default)]
pub struct Cpu {
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub a_: u8,
    pub f_: u8,
    pub b_: u8,
    pub c_: u8,
    pub d_: u8,
    pub e_: u8,
    pub h_: u8,
    pub l_: u8,
    pub ix: u16,
    pub iy: u16,
    pub sp: u16,
    pub pc: u16,
    pub i: u8,
    pub r: u8,
    pub iff1: bool,
    pub iff2: bool,
    pub im: u8,
    pub halted: bool,
    /// Set by SLP (Z180 sleep); cleared by any interrupt.
    pub sleeping: bool,
    /// EI delays interrupt acceptance by one instruction.
    pub ei_pending: bool,
    /// Last opcode was an undefined one (Z180 TRAP).
    pub trap: bool,
    pub cycles: u64,
}

fn parity(v: u8) -> bool {
    v.count_ones().is_multiple_of(2)
}

fn szp(v: u8) -> u8 {
    let mut f = v & (FS | FY | FX);
    if v == 0 {
        f |= FZ;
    }
    if parity(v) {
        f |= FP;
    }
    f
}

fn sz(v: u8) -> u8 {
    let mut f = v & (FS | FY | FX);
    if v == 0 {
        f |= FZ;
    }
    f
}

impl Cpu {
    pub fn new() -> Self {
        let mut c = Cpu::default();
        c.reset();
        c
    }

    pub fn reset(&mut self) {
        self.pc = 0;
        self.sp = 0xFFFF;
        self.a = 0xFF;
        self.f = 0xFF;
        self.i = 0;
        self.r = 0;
        self.iff1 = false;
        self.iff2 = false;
        self.im = 0;
        self.halted = false;
        self.sleeping = false;
        self.ei_pending = false;
    }

    #[inline]
    pub fn bc(&self) -> u16 {
        (self.b as u16) << 8 | self.c as u16
    }
    #[inline]
    pub fn de(&self) -> u16 {
        (self.d as u16) << 8 | self.e as u16
    }
    #[inline]
    pub fn hl(&self) -> u16 {
        (self.h as u16) << 8 | self.l as u16
    }
    #[inline]
    pub fn af(&self) -> u16 {
        (self.a as u16) << 8 | self.f as u16
    }
    #[inline]
    pub fn set_bc(&mut self, v: u16) {
        self.b = (v >> 8) as u8;
        self.c = v as u8;
    }
    #[inline]
    pub fn set_de(&mut self, v: u16) {
        self.d = (v >> 8) as u8;
        self.e = v as u8;
    }
    #[inline]
    pub fn set_hl(&mut self, v: u16) {
        self.h = (v >> 8) as u8;
        self.l = v as u8;
    }
    #[inline]
    pub fn set_af(&mut self, v: u16) {
        self.a = (v >> 8) as u8;
        self.f = v as u8;
    }

    fn fetch8<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let v = bus.mem_read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        v
    }
    fn fetch16<B: CpuBus>(&mut self, bus: &mut B) -> u16 {
        let lo = self.fetch8(bus) as u16;
        let hi = self.fetch8(bus) as u16;
        hi << 8 | lo
    }
    fn fetch_op<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let v = bus.fetch(self.pc);
        self.pc = self.pc.wrapping_add(1);
        self.r = (self.r & 0x80) | (self.r.wrapping_add(1) & 0x7F);
        v
    }
    fn rd16<B: CpuBus>(&mut self, bus: &mut B, a: u16) -> u16 {
        let lo = bus.mem_read(a) as u16;
        let hi = bus.mem_read(a.wrapping_add(1)) as u16;
        hi << 8 | lo
    }
    fn wr16<B: CpuBus>(&mut self, bus: &mut B, a: u16, v: u16) {
        bus.mem_write(a, v as u8);
        bus.mem_write(a.wrapping_add(1), (v >> 8) as u8);
    }
    fn push<B: CpuBus>(&mut self, bus: &mut B, v: u16) {
        self.sp = self.sp.wrapping_sub(1);
        bus.mem_write(self.sp, (v >> 8) as u8);
        self.sp = self.sp.wrapping_sub(1);
        bus.mem_write(self.sp, v as u8);
    }
    fn pop<B: CpuBus>(&mut self, bus: &mut B) -> u16 {
        let lo = bus.mem_read(self.sp) as u16;
        self.sp = self.sp.wrapping_add(1);
        let hi = bus.mem_read(self.sp) as u16;
        self.sp = self.sp.wrapping_add(1);
        hi << 8 | lo
    }

    fn idx_get(&self, x: Idx) -> u16 {
        match x {
            Idx::HL => self.hl(),
            Idx::IX => self.ix,
            Idx::IY => self.iy,
        }
    }
    fn idx_set(&mut self, x: Idx, v: u16) {
        match x {
            Idx::HL => self.set_hl(v),
            Idx::IX => self.ix = v,
            Idx::IY => self.iy = v,
        }
    }

    /// 8-bit register by index (0=B..7=A, 6 is not handled here). With an
    /// index prefix H/L become IXH/IXL (undocumented, but harmless).
    fn r8(&self, r: u8, x: Idx) -> u8 {
        match r {
            0 => self.b,
            1 => self.c,
            2 => self.d,
            3 => self.e,
            4 => match x {
                Idx::HL => self.h,
                Idx::IX => (self.ix >> 8) as u8,
                Idx::IY => (self.iy >> 8) as u8,
            },
            5 => match x {
                Idx::HL => self.l,
                Idx::IX => self.ix as u8,
                Idx::IY => self.iy as u8,
            },
            7 => self.a,
            _ => 0,
        }
    }
    fn set_r8(&mut self, r: u8, x: Idx, v: u8) {
        match r {
            0 => self.b = v,
            1 => self.c = v,
            2 => self.d = v,
            3 => self.e = v,
            4 => match x {
                Idx::HL => self.h = v,
                Idx::IX => self.ix = (self.ix & 0xFF) | (v as u16) << 8,
                Idx::IY => self.iy = (self.iy & 0xFF) | (v as u16) << 8,
            },
            5 => match x {
                Idx::HL => self.l = v,
                Idx::IX => self.ix = (self.ix & 0xFF00) | v as u16,
                Idx::IY => self.iy = (self.iy & 0xFF00) | v as u16,
            },
            7 => self.a = v,
            _ => {}
        }
    }

    fn rp(&self, p: u8, x: Idx) -> u16 {
        match p {
            0 => self.bc(),
            1 => self.de(),
            2 => self.idx_get(x),
            _ => self.sp,
        }
    }
    fn set_rp(&mut self, p: u8, x: Idx, v: u16) {
        match p {
            0 => self.set_bc(v),
            1 => self.set_de(v),
            2 => self.idx_set(x, v),
            _ => self.sp = v,
        }
    }
    fn rp2(&self, p: u8, x: Idx) -> u16 {
        match p {
            0 => self.bc(),
            1 => self.de(),
            2 => self.idx_get(x),
            _ => self.af(),
        }
    }
    fn set_rp2(&mut self, p: u8, x: Idx, v: u16) {
        match p {
            0 => self.set_bc(v),
            1 => self.set_de(v),
            2 => self.idx_set(x, v),
            _ => self.set_af(v),
        }
    }

    fn cond(&self, y: u8) -> bool {
        match y {
            0 => self.f & FZ == 0,
            1 => self.f & FZ != 0,
            2 => self.f & FC == 0,
            3 => self.f & FC != 0,
            4 => self.f & FP == 0,
            5 => self.f & FP != 0,
            6 => self.f & FS == 0,
            _ => self.f & FS != 0,
        }
    }

    // ---- ALU ----
    fn add8(&mut self, v: u8, carry: bool) {
        let c = if carry && self.f & FC != 0 { 1 } else { 0 };
        let a = self.a;
        let r16 = a as u16 + v as u16 + c as u16;
        let r = r16 as u8;
        let mut f = sz(r);
        if (a & 0xF) + (v & 0xF) + c > 0xF {
            f |= FH;
        }
        if r16 > 0xFF {
            f |= FC;
        }
        if (a ^ v) & 0x80 == 0 && (a ^ r) & 0x80 != 0 {
            f |= FP;
        }
        self.a = r;
        self.f = f;
    }
    fn sub8(&mut self, v: u8, carry: bool, store: bool) -> u8 {
        let c = if carry && self.f & FC != 0 { 1 } else { 0 };
        let a = self.a;
        let r16 = (a as u16).wrapping_sub(v as u16).wrapping_sub(c as u16);
        let r = r16 as u8;
        let mut f = sz(r) | FN;
        if (a & 0xF) < (v & 0xF) + c {
            f |= FH;
        }
        if r16 > 0xFF {
            f |= FC;
        }
        if (a ^ v) & 0x80 != 0 && (a ^ r) & 0x80 != 0 {
            f |= FP;
        }
        if store {
            self.a = r;
        } else {
            // CP: undocumented bits come from the operand
            f = (f & !(FY | FX)) | (v & (FY | FX));
        }
        self.f = f;
        r
    }
    fn alu(&mut self, op: u8, v: u8) {
        match op {
            0 => self.add8(v, false),
            1 => self.add8(v, true),
            2 => {
                self.sub8(v, false, true);
            }
            3 => {
                self.sub8(v, true, true);
            }
            4 => {
                self.a &= v;
                self.f = szp(self.a) | FH;
            }
            5 => {
                self.a ^= v;
                self.f = szp(self.a);
            }
            6 => {
                self.a |= v;
                self.f = szp(self.a);
            }
            _ => {
                self.sub8(v, false, false);
            }
        }
    }
    fn inc8(&mut self, v: u8) -> u8 {
        let r = v.wrapping_add(1);
        let mut f = (self.f & FC) | sz(r);
        if v & 0xF == 0xF {
            f |= FH;
        }
        if v == 0x7F {
            f |= FP;
        }
        self.f = f;
        r
    }
    fn dec8(&mut self, v: u8) -> u8 {
        let r = v.wrapping_sub(1);
        let mut f = (self.f & FC) | sz(r) | FN;
        if v & 0xF == 0 {
            f |= FH;
        }
        if v == 0x80 {
            f |= FP;
        }
        self.f = f;
        r
    }
    fn add16(&mut self, a: u16, b: u16) -> u16 {
        let r = a as u32 + b as u32;
        let mut f = self.f & (FS | FZ | FP);
        if (a & 0xFFF) + (b & 0xFFF) > 0xFFF {
            f |= FH;
        }
        if r > 0xFFFF {
            f |= FC;
        }
        f |= ((r >> 8) as u8) & (FY | FX);
        self.f = f;
        r as u16
    }
    fn adc16(&mut self, b: u16) {
        let a = self.hl();
        let c = (self.f & FC) as u32;
        let r = a as u32 + b as u32 + c;
        let r16 = r as u16;
        let mut f = ((r16 >> 8) as u8) & (FS | FY | FX);
        if r16 == 0 {
            f |= FZ;
        }
        if (a & 0xFFF) as u32 + (b & 0xFFF) as u32 + c > 0xFFF {
            f |= FH;
        }
        if r > 0xFFFF {
            f |= FC;
        }
        if (a ^ b) & 0x8000 == 0 && (a ^ r16) & 0x8000 != 0 {
            f |= FP;
        }
        self.f = f;
        self.set_hl(r16);
    }
    fn sbc16(&mut self, b: u16) {
        let a = self.hl();
        let c = (self.f & FC) as u32;
        let r = (a as u32).wrapping_sub(b as u32).wrapping_sub(c);
        let r16 = r as u16;
        let mut f = (((r16 >> 8) as u8) & (FS | FY | FX)) | FN;
        if r16 == 0 {
            f |= FZ;
        }
        if ((a & 0xFFF) as u32) < (b & 0xFFF) as u32 + c {
            f |= FH;
        }
        if r > 0xFFFF {
            f |= FC;
        }
        if (a ^ b) & 0x8000 != 0 && (a ^ r16) & 0x8000 != 0 {
            f |= FP;
        }
        self.f = f;
        self.set_hl(r16);
    }
    fn rot(&mut self, y: u8, v: u8) -> u8 {
        let c = self.f & FC;
        let (r, nc) = match y {
            0 => (v.rotate_left(1), v >> 7),
            1 => (v.rotate_right(1), v & 1),
            2 => ((v << 1) | c, v >> 7),
            3 => ((v >> 1) | (c << 7), v & 1),
            4 => (v << 1, v >> 7),
            5 => ((v >> 1) | (v & 0x80), v & 1),
            6 => ((v << 1) | 1, v >> 7), // SLL (undocumented on Z80, TRAP on Z180)
            _ => (v >> 1, v & 1),
        };
        self.f = szp(r) | nc;
        r
    }
    fn daa(&mut self) {
        let a = self.a;
        let mut corr = 0u8;
        let mut carry = self.f & FC;
        if self.f & FH != 0 || a & 0xF > 9 {
            corr |= 0x06;
        }
        if carry != 0 || a > 0x99 {
            corr |= 0x60;
            carry = FC;
        }

        let (r, h) = if self.f & FN != 0 { (a.wrapping_sub(corr), self.f & FH != 0 && (a & 0xF) < 6) } else { (a.wrapping_add(corr), (a & 0xF) > 9) };
        self.a = r;
        self.f = szp(r) | carry | (self.f & FN) | if h { FH } else { 0 };
    }

    /// Accept a maskable interrupt. `vector_addr`: for IM2 / Z180 internal
    /// vectors, the full address of the vector table entry. For IM1 pass None.
    pub fn interrupt<B: CpuBus>(&mut self, bus: &mut B, vector_addr: Option<u16>) -> u32 {
        self.halted = false;
        self.sleeping = false;
        self.iff1 = false;
        self.iff2 = false;
        let pc = self.pc;
        self.push(bus, pc);
        match vector_addr {
            Some(a) => {
                self.pc = self.rd16(bus, a);
                19
            }
            None => {
                self.pc = 0x38;
                13
            }
        }
    }

    /// Execute one instruction; returns clock cycles used.
    pub fn step<B: CpuBus>(&mut self, bus: &mut B) -> u32 {
        self.ei_pending = false;
        if self.halted || self.sleeping {
            self.r = (self.r & 0x80) | (self.r.wrapping_add(1) & 0x7F);
            return 4;
        }
        let op = self.fetch_op(bus);
        let c = match op {
            0xCB => self.exec_cb(bus, Idx::HL),
            0xED => self.exec_ed(bus),
            0xDD => self.exec_idx(bus, Idx::IX),
            0xFD => self.exec_idx(bus, Idx::IY),
            _ => self.exec_main(bus, op, Idx::HL),
        };
        self.cycles += c as u64;
        c
    }

    fn exec_idx<B: CpuBus>(&mut self, bus: &mut B, x: Idx) -> u32 {
        let op = self.fetch_op(bus);
        match op {
            0xCB => self.exec_cb(bus, x) + 4,
            0xED => self.exec_ed(bus) + 4,
            0xDD => self.exec_idx(bus, Idx::IX),
            0xFD => self.exec_idx(bus, Idx::IY),
            _ => self.exec_main(bus, op, x) + 4,
        }
    }

    /// Address of (HL) or (IX+d)/(IY+d); fetches the displacement.
    fn mem_addr<B: CpuBus>(&mut self, bus: &mut B, x: Idx) -> u16 {
        match x {
            Idx::HL => self.hl(),
            _ => {
                let d = self.fetch8(bus) as i8 as i16 as u16;
                self.idx_get(x).wrapping_add(d)
            }
        }
    }

    fn exec_main<B: CpuBus>(&mut self, bus: &mut B, op: u8, x: Idx) -> u32 {
        let xx = op >> 6;
        let y = (op >> 3) & 7;
        let z = op & 7;
        let p = y >> 1;
        let q = y & 1;
        match xx {
            0 => match z {
                0 => match y {
                    0 => 4,
                    1 => {
                        let t = self.af();
                        self.a = self.a_;
                        self.f = self.f_;
                        self.a_ = (t >> 8) as u8;
                        self.f_ = t as u8;
                        4
                    }
                    2 => {
                        let d = self.fetch8(bus) as i8;
                        self.b = self.b.wrapping_sub(1);
                        if self.b != 0 {
                            self.pc = self.pc.wrapping_add(d as i16 as u16);
                            13
                        } else {
                            8
                        }
                    }
                    3 => {
                        let d = self.fetch8(bus) as i8;
                        self.pc = self.pc.wrapping_add(d as i16 as u16);
                        12
                    }
                    _ => {
                        let d = self.fetch8(bus) as i8;
                        if self.cond(y - 4) {
                            self.pc = self.pc.wrapping_add(d as i16 as u16);
                            12
                        } else {
                            7
                        }
                    }
                },
                1 => {
                    if q == 0 {
                        let v = self.fetch16(bus);
                        self.set_rp(p, x, v);
                        10
                    } else {
                        let a = self.idx_get(x);
                        let b = self.rp(p, x);
                        let r = self.add16(a, b);
                        self.idx_set(x, r);
                        11
                    }
                }
                2 => {
                    match (q, p) {
                        (0, 0) => bus.mem_write(self.bc(), self.a),
                        (0, 1) => bus.mem_write(self.de(), self.a),
                        (0, 2) => {
                            let a = self.fetch16(bus);
                            let v = self.idx_get(x);
                            self.wr16(bus, a, v);
                            return 16;
                        }
                        (0, _) => {
                            let a = self.fetch16(bus);
                            bus.mem_write(a, self.a);
                            return 13;
                        }
                        (1, 0) => self.a = bus.mem_read(self.bc()),
                        (1, 1) => self.a = bus.mem_read(self.de()),
                        (1, 2) => {
                            let a = self.fetch16(bus);
                            let v = self.rd16(bus, a);
                            self.idx_set(x, v);
                            return 16;
                        }
                        _ => {
                            let a = self.fetch16(bus);
                            self.a = bus.mem_read(a);
                            return 13;
                        }
                    }
                    7
                }
                3 => {
                    let v = self.rp(p, x);
                    let v = if q == 0 { v.wrapping_add(1) } else { v.wrapping_sub(1) };
                    self.set_rp(p, x, v);
                    6
                }
                4 | 5 => {
                    if y == 6 {
                        let a = self.mem_addr(bus, x);
                        let v = bus.mem_read(a);
                        let r = if z == 4 { self.inc8(v) } else { self.dec8(v) };
                        bus.mem_write(a, r);
                        11
                    } else {
                        let v = self.r8(y, x);
                        let r = if z == 4 { self.inc8(v) } else { self.dec8(v) };
                        self.set_r8(y, x, r);
                        4
                    }
                }
                6 => {
                    if y == 6 {
                        let a = self.mem_addr(bus, x);
                        let v = self.fetch8(bus);
                        bus.mem_write(a, v);
                        10
                    } else {
                        let v = self.fetch8(bus);
                        self.set_r8(y, x, v);
                        7
                    }
                }
                _ => {
                    match y {
                        0 => {
                            let c = self.a >> 7;
                            self.a = self.a.rotate_left(1);
                            self.f = (self.f & (FS | FZ | FP)) | c | (self.a & (FY | FX));
                        }
                        1 => {
                            let c = self.a & 1;
                            self.a = self.a.rotate_right(1);
                            self.f = (self.f & (FS | FZ | FP)) | c | (self.a & (FY | FX));
                        }
                        2 => {
                            let c = self.a >> 7;
                            self.a = (self.a << 1) | (self.f & FC);
                            self.f = (self.f & (FS | FZ | FP)) | c | (self.a & (FY | FX));
                        }
                        3 => {
                            let c = self.a & 1;
                            self.a = (self.a >> 1) | ((self.f & FC) << 7);
                            self.f = (self.f & (FS | FZ | FP)) | c | (self.a & (FY | FX));
                        }
                        4 => self.daa(),
                        5 => {
                            self.a = !self.a;
                            self.f = (self.f & (FS | FZ | FP | FC)) | FH | FN | (self.a & (FY | FX));
                        }
                        6 => {
                            self.f = (self.f & (FS | FZ | FP)) | FC | (self.a & (FY | FX));
                        }
                        _ => {
                            let c = self.f & FC;
                            self.f = (self.f & (FS | FZ | FP)) | if c != 0 { FH } else { FC } | (self.a & (FY | FX));
                        }
                    }
                    4
                }
            },
            1 => {
                if z == 6 && y == 6 {
                    self.halted = true;
                    return 4;
                }
                if y == 6 {
                    let a = self.mem_addr(bus, x);
                    let v = self.r8(z, Idx::HL);
                    bus.mem_write(a, v);
                    7
                } else if z == 6 {
                    let a = self.mem_addr(bus, x);
                    let v = bus.mem_read(a);
                    self.set_r8(y, Idx::HL, v);
                    7
                } else {
                    let v = self.r8(z, x);
                    self.set_r8(y, x, v);
                    4
                }
            }
            2 => {
                if z == 6 {
                    let a = self.mem_addr(bus, x);
                    let v = bus.mem_read(a);
                    self.alu(y, v);
                    7
                } else {
                    let v = self.r8(z, x);
                    self.alu(y, v);
                    4
                }
            }
            _ => match z {
                0 => {
                    if self.cond(y) {
                        self.pc = self.pop(bus);
                        11
                    } else {
                        5
                    }
                }
                1 => {
                    if q == 0 {
                        let v = self.pop(bus);
                        self.set_rp2(p, x, v);
                        10
                    } else {
                        match p {
                            0 => {
                                self.pc = self.pop(bus);
                                10
                            }
                            1 => {
                                std::mem::swap(&mut self.b, &mut self.b_);
                                std::mem::swap(&mut self.c, &mut self.c_);
                                std::mem::swap(&mut self.d, &mut self.d_);
                                std::mem::swap(&mut self.e, &mut self.e_);
                                std::mem::swap(&mut self.h, &mut self.h_);
                                std::mem::swap(&mut self.l, &mut self.l_);
                                4
                            }
                            2 => {
                                self.pc = self.idx_get(x);
                                4
                            }
                            _ => {
                                self.sp = self.idx_get(x);
                                6
                            }
                        }
                    }
                }
                2 => {
                    let a = self.fetch16(bus);
                    if self.cond(y) {
                        self.pc = a;
                    }
                    10
                }
                3 => match y {
                    0 => {
                        self.pc = self.fetch16(bus);
                        10
                    }
                    1 => self.exec_cb(bus, x),
                    2 => {
                        let n = self.fetch8(bus);
                        let port = (self.a as u16) << 8 | n as u16;
                        bus.io_write(port, self.a);
                        11
                    }
                    3 => {
                        let n = self.fetch8(bus);
                        let port = (self.a as u16) << 8 | n as u16;
                        self.a = bus.io_read(port);
                        11
                    }
                    4 => {
                        let v = self.rd16(bus, self.sp);
                        let r = self.idx_get(x);
                        self.wr16(bus, self.sp, r);
                        self.idx_set(x, v);
                        19
                    }
                    5 => {
                        let t = self.de();
                        self.set_de(self.hl());
                        self.set_hl(t);
                        4
                    }
                    6 => {
                        self.iff1 = false;
                        self.iff2 = false;
                        4
                    }
                    _ => {
                        self.iff1 = true;
                        self.iff2 = true;
                        self.ei_pending = true;
                        4
                    }
                },
                4 => {
                    let a = self.fetch16(bus);
                    if self.cond(y) {
                        let pc = self.pc;
                        self.push(bus, pc);
                        self.pc = a;
                        17
                    } else {
                        10
                    }
                }
                5 => {
                    if q == 0 {
                        let v = self.rp2(p, x);
                        self.push(bus, v);
                        11
                    } else {
                        match p {
                            0 => {
                                let a = self.fetch16(bus);
                                let pc = self.pc;
                                self.push(bus, pc);
                                self.pc = a;
                                17
                            }
                            1 => self.exec_idx(bus, Idx::IX),
                            2 => self.exec_ed(bus),
                            _ => self.exec_idx(bus, Idx::IY),
                        }
                    }
                }
                6 => {
                    let v = self.fetch8(bus);
                    self.alu(y, v);
                    7
                }
                _ => {
                    let pc = self.pc;
                    self.push(bus, pc);
                    self.pc = (y as u16) * 8;
                    11
                }
            },
        }
    }

    fn exec_cb<B: CpuBus>(&mut self, bus: &mut B, x: Idx) -> u32 {
        // For DD CB d op the displacement comes before the opcode.
        let addr = if x != Idx::HL { Some(self.mem_addr(bus, x)) } else { None };
        let op = if x == Idx::HL { self.fetch_op(bus) } else { self.fetch8(bus) };
        let xx = op >> 6;
        let y = (op >> 3) & 7;
        let z = op & 7;
        let (v, a) = if let Some(a) = addr {
            (bus.mem_read(a), Some(a))
        } else if z == 6 {
            let a = self.hl();
            (bus.mem_read(a), Some(a))
        } else {
            (self.r8(z, Idx::HL), None)
        };
        let result = match xx {
            0 => Some(self.rot(y, v)),
            1 => {
                let bit = v & (1 << y);
                let mut f = (self.f & FC) | FH;
                if bit == 0 {
                    f |= FZ | FP;
                }
                if y == 7 && bit != 0 {
                    f |= FS;
                }
                f |= v & (FY | FX);
                self.f = f;
                None
            }
            2 => Some(v & !(1 << y)),
            _ => Some(v | (1 << y)),
        };
        if let Some(r) = result {
            match a {
                Some(addr) => {
                    bus.mem_write(addr, r);
                    if x != Idx::HL && z != 6 {
                        self.set_r8(z, Idx::HL, r);
                    }
                }
                None => self.set_r8(z, Idx::HL, r),
            }
        }
        if a.is_some() {
            if xx == 1 {
                12
            } else {
                15
            }
        } else {
            8
        }
    }

    fn block_ld<B: CpuBus>(&mut self, bus: &mut B, inc: bool) {
        let v = bus.mem_read(self.hl());
        bus.mem_write(self.de(), v);
        let d: u16 = if inc { 1 } else { 0xFFFF };
        self.set_hl(self.hl().wrapping_add(d));
        self.set_de(self.de().wrapping_add(d));
        self.set_bc(self.bc().wrapping_sub(1));
        let n = v.wrapping_add(self.a);
        self.f = (self.f & (FS | FZ | FC)) | if self.bc() != 0 { FP } else { 0 } | (n & FX) | ((n << 4) & FY);
    }
    fn block_cp<B: CpuBus>(&mut self, bus: &mut B, inc: bool) {
        let v = bus.mem_read(self.hl());
        let r = self.a.wrapping_sub(v);
        let h = (self.a & 0xF) < (v & 0xF);
        let d: u16 = if inc { 1 } else { 0xFFFF };
        self.set_hl(self.hl().wrapping_add(d));
        self.set_bc(self.bc().wrapping_sub(1));
        let n = r.wrapping_sub(if h { 1 } else { 0 });
        self.f = (self.f & FC)
            | FN
            | (r & FS)
            | if r == 0 { FZ } else { 0 }
            | if h { FH } else { 0 }
            | if self.bc() != 0 { FP } else { 0 }
            | (n & FX)
            | ((n << 4) & FY);
    }
    fn block_in<B: CpuBus>(&mut self, bus: &mut B, inc: bool) {
        let v = bus.io_read(self.bc());
        bus.mem_write(self.hl(), v);
        self.b = self.b.wrapping_sub(1);
        let d: u16 = if inc { 1 } else { 0xFFFF };
        self.set_hl(self.hl().wrapping_add(d));
        self.f = sz(self.b) | FN | (self.f & FC);
    }
    fn block_out<B: CpuBus>(&mut self, bus: &mut B, inc: bool) {
        let v = bus.mem_read(self.hl());
        self.b = self.b.wrapping_sub(1);
        bus.io_write(self.bc(), v);
        let d: u16 = if inc { 1 } else { 0xFFFF };
        self.set_hl(self.hl().wrapping_add(d));
        self.f = sz(self.b) | FN | (self.f & FC);
    }
    /// Z180 OTIM/OTDM: output (HL) to port (C) with A15..A8 = 0, C += / -= 1.
    fn block_otm<B: CpuBus>(&mut self, bus: &mut B, inc: bool) {
        let v = bus.mem_read(self.hl());
        bus.io_write(self.c as u16, v);
        let d: u16 = if inc { 1 } else { 0xFFFF };
        self.set_hl(self.hl().wrapping_add(d));
        self.c = if inc { self.c.wrapping_add(1) } else { self.c.wrapping_sub(1) };
        self.b = self.b.wrapping_sub(1);
        self.f = sz(self.b) | FN | if v & 0x80 != 0 { FN } else { 0 };
    }

    fn exec_ed<B: CpuBus>(&mut self, bus: &mut B) -> u32 {
        let op = self.fetch_op(bus);
        let xx = op >> 6;
        let y = (op >> 3) & 7;
        let z = op & 7;
        let p = y >> 1;
        let q = y & 1;
        if xx == 0 {
            // Z180: IN0 r,(n) / OUT0 (n),r / TST r
            match z {
                0 => {
                    let n = self.fetch8(bus);
                    let v = bus.io_read(n as u16);
                    self.f = szp(v) | (self.f & FC);
                    if y != 6 {
                        self.set_r8(y, Idx::HL, v);
                    }
                    return 12;
                }
                1 => {
                    let n = self.fetch8(bus);
                    let v = if y == 6 { 0 } else { self.r8(y, Idx::HL) };
                    bus.io_write(n as u16, v);
                    return 13;
                }
                4 => {
                    let v = if y == 6 { bus.mem_read(self.hl()) } else { self.r8(y, Idx::HL) };
                    let r = self.a & v;
                    self.f = szp(r) | FH;
                    return 7;
                }
                _ => {
                    self.trap = true;
                    return 8;
                }
            }
        }
        if xx == 1 {
            match z {
                0 => {
                    let v = bus.io_read(self.bc());
                    self.f = szp(v) | (self.f & FC);
                    if y != 6 {
                        self.set_r8(y, Idx::HL, v);
                    }
                    12
                }
                1 => {
                    let v = if y == 6 { 0 } else { self.r8(y, Idx::HL) };
                    bus.io_write(self.bc(), v);
                    12
                }
                2 => {
                    let v = self.rp(p, Idx::HL);
                    if q == 0 {
                        self.sbc16(v);
                    } else {
                        self.adc16(v);
                    }
                    15
                }
                3 => {
                    let a = self.fetch16(bus);
                    if q == 0 {
                        let v = self.rp(p, Idx::HL);
                        self.wr16(bus, a, v);
                    } else {
                        let v = self.rd16(bus, a);
                        self.set_rp(p, Idx::HL, v);
                    }
                    20
                }
                4 => {
                    match op {
                        0x4C | 0x5C | 0x6C | 0x7C => {
                            // MLT rr
                            let v = self.rp(p, Idx::HL);
                            let r = (v >> 8) * (v & 0xFF);
                            self.set_rp(p, Idx::HL, r);
                            17
                        }
                        0x64 => {
                            // TST n
                            let n = self.fetch8(bus);
                            let r = self.a & n;
                            self.f = szp(r) | FH;
                            9
                        }
                        0x74 => {
                            // TSTIO n
                            let n = self.fetch8(bus);
                            let v = bus.io_read(self.c as u16);
                            self.f = szp(v & n) | FH;
                            12
                        }
                        _ => {
                            // NEG
                            let a = self.a;
                            self.a = 0;
                            self.sub8(a, false, true);
                            8
                        }
                    }
                }
                5 => {
                    // RETN / RETI
                    self.pc = self.pop(bus);
                    if y == 1 {
                        bus.reti();
                    }
                    self.iff1 = self.iff2;
                    14
                }
                6 if op == 0x76 => {
                    // Z180 SLP: sleep until an interrupt
                    self.sleeping = true;
                    8
                }
                6 => {
                    self.im = match y & 3 {
                        0 | 1 => 0,
                        2 => 1,
                        _ => 2,
                    };
                    8
                }
                _ => match y {
                    0 => {
                        self.i = self.a;
                        9
                    }
                    1 => {
                        self.r = self.a;
                        9
                    }
                    2 => {
                        self.a = self.i;
                        self.f = sz(self.a) | (self.f & FC) | if self.iff2 { FP } else { 0 };
                        9
                    }
                    3 => {
                        self.a = self.r;
                        self.f = sz(self.a) | (self.f & FC) | if self.iff2 { FP } else { 0 };
                        9
                    }
                    4 => {
                        // RRD
                        let a = self.hl();
                        let m = bus.mem_read(a);
                        let nm = (self.a << 4) | (m >> 4);
                        self.a = (self.a & 0xF0) | (m & 0xF);
                        bus.mem_write(a, nm);
                        self.f = szp(self.a) | (self.f & FC);
                        18
                    }
                    5 => {
                        // RLD
                        let a = self.hl();
                        let m = bus.mem_read(a);
                        let nm = (m << 4) | (self.a & 0xF);
                        self.a = (self.a & 0xF0) | (m >> 4);
                        bus.mem_write(a, nm);
                        self.f = szp(self.a) | (self.f & FC);
                        18
                    }
                    _ => 8,
                },
            }
        } else if xx == 2 {
            match op {
                0xA0 => {
                    self.block_ld(bus, true);
                    16
                }
                0xA8 => {
                    self.block_ld(bus, false);
                    16
                }
                0xB0 | 0xB8 => {
                    self.block_ld(bus, op == 0xB0);
                    if self.bc() != 0 {
                        self.pc = self.pc.wrapping_sub(2);
                        21
                    } else {
                        16
                    }
                }
                0xA1 | 0xA9 => {
                    self.block_cp(bus, op == 0xA1);
                    16
                }
                0xB1 | 0xB9 => {
                    self.block_cp(bus, op == 0xB1);
                    if self.bc() != 0 && self.f & FZ == 0 {
                        self.pc = self.pc.wrapping_sub(2);
                        21
                    } else {
                        16
                    }
                }
                0xA2 | 0xAA => {
                    self.block_in(bus, op == 0xA2);
                    16
                }
                0xB2 | 0xBA => {
                    self.block_in(bus, op == 0xB2);
                    if self.b != 0 {
                        self.pc = self.pc.wrapping_sub(2);
                        21
                    } else {
                        16
                    }
                }
                0xA3 | 0xAB => {
                    self.block_out(bus, op == 0xA3);
                    16
                }
                0xB3 | 0xBB => {
                    self.block_out(bus, op == 0xB3);
                    if self.b != 0 {
                        self.pc = self.pc.wrapping_sub(2);
                        21
                    } else {
                        16
                    }
                }
                0x83 | 0x8B => {
                    self.block_otm(bus, op == 0x83);
                    14
                }
                0x93 | 0x9B => {
                    self.block_otm(bus, op == 0x93);
                    if self.b != 0 {
                        self.pc = self.pc.wrapping_sub(2);
                        16
                    } else {
                        14
                    }
                }
                _ => {
                    self.trap = true;
                    8
                }
            }
        } else {
            self.trap = true;
            8
        }
    }
}
