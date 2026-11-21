//! HD64180 / Z180 on-chip peripherals: MMU, two programmable reload timers
//! (PRT), two async serial channels (ASCI), the clocked serial port (CSIO,
//! minimal), the two DMA channels, the free-running counter and the interrupt
//! controller. Everything outside the chip is the `Board`.

use crate::cpu::{Cpu, CpuBus};

/// What the CPU sees outside the chip.
pub trait Board {
    fn mem_read(&mut self, phys: u32) -> u8;
    fn mem_write(&mut self, phys: u32, v: u8);
    fn io_read(&mut self, port: u16) -> u8;
    fn io_write(&mut self, port: u16, v: u8);
    /// Advance board time by `cycles` CPU clocks. `tout` is the PRT1 TOUT pin.
    fn tick(&mut self, cycles: u32, tout_edges: u32);
    /// External interrupt request lines (INT0, INT1, INT2), level active.
    fn int_lines(&mut self) -> (bool, bool, bool);
    /// DMA request line for channel 1 (memory -> I/O, voice output).
    fn dreq1(&mut self) -> bool;
    /// Byte arriving on ASCI channel `ch`, if any.
    fn asci_rx(&mut self, ch: usize) -> Option<u8>;
    /// Byte sent on ASCI channel `ch`.
    fn asci_tx(&mut self, ch: usize, v: u8);
    /// Debug hook: a CPU write to a logical address.
    fn note_write(&mut self, _logical: u16, _v: u8) {}
}

#[derive(Clone, Debug, Default)]
pub struct Asci {
    pub cntla: u8,
    pub cntlb: u8,
    pub stat: u8,
    pub tdr: u8,
    pub tdr_full: bool,
    pub rdr: u8,
    pub tsr_busy: u32,
    pub rx_wait: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Internal {
    pub regs: [u8; 32],
    pub icr_base: u8,
    // MMU
    pub cbr: u8,
    pub bbr: u8,
    pub cbar: u8,
    // PRT
    pub tmdr: [u16; 2],
    pub rldr: [u16; 2],
    pub tcr: u8,
    pub tcr_read: [bool; 2],
    pub prt_div: u32,
    pub tout: bool,
    pub tmdr_latch_hi: [u8; 2],
    // ASCI
    pub asci: [Asci; 2],
    // DMA
    pub sar0: u32,
    pub dar0: u32,
    pub bcr0: u16,
    pub mar1: u32,
    pub iar1: u16,
    pub bcr1: u16,
    pub dstat: u8,
    pub dmode: u8,
    pub dcntl: u8,
    pub dreq1_last: bool,
    // interrupts
    pub il: u8,
    pub itc: u8,
    pub frc: u8,
    pub frc_div: u32,
    pub rcr: u8,
    pub omcr: u8,
    pub csio_cntr: u8,
    pub csio_trdr: u8,
}

impl Internal {
    pub fn new() -> Self {
        let mut s = Internal::default();
        s.reset();
        s
    }
    pub fn reset(&mut self) {
        self.cbar = 0xF0;
        self.cbr = 0;
        self.bbr = 0;
        self.icr_base = 0;
        self.tmdr = [0xFFFF; 2];
        self.rldr = [0xFFFF; 2];
        self.tcr = 0;
        self.itc = 0x01;
        self.il = 0;
        self.dstat = 0x32;
        self.dmode = 0;
        self.dcntl = 0xF0;
        self.rcr = 0xFC;
        self.omcr = 0xFF;
        self.frc = 0xFF;
        for a in self.asci.iter_mut() {
            *a = Asci::default();
            a.stat = 0x02; // TDRE
        }
    }

    /// Logical -> physical address through the MMU.
    #[inline]
    pub fn translate(&self, addr: u16) -> u32 {
        let page = (addr >> 12) as u8;
        let ca = self.cbar >> 4;
        let ba = self.cbar & 0x0F;
        let a = addr as u32;
        if page >= ca {
            (a + ((self.cbr as u32) << 12)) & 0xFFFFF
        } else if page >= ba {
            (a + ((self.bbr as u32) << 12)) & 0xFFFFF
        } else {
            a
        }
    }

    pub fn is_internal(&self, port: u16) -> bool {
        port & 0xFFC0 == self.icr_base as u16
    }

    pub fn read(&mut self, r: u8) -> u8 {
        match r {
            0x00 | 0x01 => self.asci[r as usize].cntla,
            0x02 | 0x03 => self.asci[(r - 2) as usize].cntlb,
            0x04 | 0x05 => {
                let a = &self.asci[(r - 4) as usize];
                let mut s = a.stat & !0x82;
                if a.rx_wait == 0 && a.stat & 0x80 != 0 {
                    s |= 0x80;
                }
                if !a.tdr_full {
                    s |= 0x02;
                }
                s
            }
            0x06 | 0x07 => self.asci[(r - 6) as usize].tdr,
            0x08 | 0x09 => {
                let a = &mut self.asci[(r - 8) as usize];
                a.stat &= !0x80;
                a.rdr
            }
            0x0A => self.csio_cntr | 0x80,
            0x0B => self.csio_trdr,
            0x0C | 0x0D | 0x14 | 0x15 => {
                let t = if r < 0x14 { 0 } else { 1 };
                if self.tcr_read[t] {
                    self.tcr &= !(if t == 0 { 0x40 } else { 0x80 });
                    self.tcr_read[t] = false;
                }
                if r & 1 == 0 {
                    self.tmdr_latch_hi[t] = (self.tmdr[t] >> 8) as u8;
                    self.tmdr[t] as u8
                } else {
                    self.tmdr_latch_hi[t]
                }
            }
            0x0E => self.rldr[0] as u8,
            0x0F => (self.rldr[0] >> 8) as u8,
            0x16 => self.rldr[1] as u8,
            0x17 => (self.rldr[1] >> 8) as u8,
            0x10 => {
                if self.tcr & 0x40 != 0 {
                    self.tcr_read[0] = true;
                }
                if self.tcr & 0x80 != 0 {
                    self.tcr_read[1] = true;
                }
                self.tcr
            }
            0x18 => self.frc,
            0x20 => self.sar0 as u8,
            0x21 => (self.sar0 >> 8) as u8,
            0x22 => (self.sar0 >> 16) as u8,
            0x23 => self.dar0 as u8,
            0x24 => (self.dar0 >> 8) as u8,
            0x25 => (self.dar0 >> 16) as u8,
            0x26 => self.bcr0 as u8,
            0x27 => (self.bcr0 >> 8) as u8,
            0x28 => self.mar1 as u8,
            0x29 => (self.mar1 >> 8) as u8,
            0x2A => (self.mar1 >> 16) as u8,
            0x2B => self.iar1 as u8,
            0x2C => (self.iar1 >> 8) as u8,
            0x2E => self.bcr1 as u8,
            0x2F => (self.bcr1 >> 8) as u8,
            0x30 => self.dstat | 0x30,
            0x31 => self.dmode | 0xC1,
            0x32 => self.dcntl,
            0x33 => self.il,
            0x34 => self.itc | 0x38,
            0x36 => self.rcr,
            0x38 => self.cbr,
            0x39 => self.bbr,
            0x3A => self.cbar,
            0x3E => self.omcr,
            0x3F => self.icr_base | 0x1F,
            _ => self.regs[r as usize & 31],
        }
    }

    pub fn write(&mut self, r: u8, v: u8) {
        self.regs[r as usize & 31] = v;
        match r {
            0x00 | 0x01 => self.asci[r as usize].cntla = v,
            0x02 | 0x03 => self.asci[(r - 2) as usize].cntlb = v,
            0x04 | 0x05 => {
                let a = &mut self.asci[(r - 4) as usize];
                a.stat = (a.stat & 0xF6) | (v & 0x09);
                if r == 0x05 {
                    a.stat = (a.stat & !0x04) | (v & 0x04);
                }
            }
            0x06 | 0x07 => {
                let a = &mut self.asci[(r - 6) as usize];
                a.tdr = v;
                a.tdr_full = true;
            }
            0x08 | 0x09 => self.asci[(r - 8) as usize].rdr = v,
            0x0A => self.csio_cntr = (self.csio_cntr & 0x80) | (v & 0x7F),
            0x0B => self.csio_trdr = v,
            0x0C => self.tmdr[0] = (self.tmdr[0] & 0xFF00) | v as u16,
            0x0D => self.tmdr[0] = (self.tmdr[0] & 0x00FF) | (v as u16) << 8,
            0x0E => self.rldr[0] = (self.rldr[0] & 0xFF00) | v as u16,
            0x0F => self.rldr[0] = (self.rldr[0] & 0x00FF) | (v as u16) << 8,
            0x14 => self.tmdr[1] = (self.tmdr[1] & 0xFF00) | v as u16,
            0x15 => self.tmdr[1] = (self.tmdr[1] & 0x00FF) | (v as u16) << 8,
            0x16 => self.rldr[1] = (self.rldr[1] & 0xFF00) | v as u16,
            0x17 => self.rldr[1] = (self.rldr[1] & 0x00FF) | (v as u16) << 8,
            0x10 => self.tcr = (self.tcr & 0xC0) | (v & 0x3F),
            0x20 => self.sar0 = (self.sar0 & 0xFFF00) | v as u32,
            0x21 => self.sar0 = (self.sar0 & 0xF00FF) | (v as u32) << 8,
            0x22 => self.sar0 = (self.sar0 & 0x0FFFF) | ((v & 0x0F) as u32) << 16,
            0x23 => self.dar0 = (self.dar0 & 0xFFF00) | v as u32,
            0x24 => self.dar0 = (self.dar0 & 0xF00FF) | (v as u32) << 8,
            0x25 => self.dar0 = (self.dar0 & 0x0FFFF) | ((v & 0x0F) as u32) << 16,
            0x26 => self.bcr0 = (self.bcr0 & 0xFF00) | v as u16,
            0x27 => self.bcr0 = (self.bcr0 & 0x00FF) | (v as u16) << 8,
            0x28 => self.mar1 = (self.mar1 & 0xFFF00) | v as u32,
            0x29 => self.mar1 = (self.mar1 & 0xF00FF) | (v as u32) << 8,
            0x2A => self.mar1 = (self.mar1 & 0x0FFFF) | ((v & 0x0F) as u32) << 16,
            0x2B => self.iar1 = (self.iar1 & 0xFF00) | v as u16,
            0x2C => self.iar1 = (self.iar1 & 0x00FF) | (v as u16) << 8,
            0x2E => self.bcr1 = (self.bcr1 & 0xFF00) | v as u16,
            0x2F => self.bcr1 = (self.bcr1 & 0x00FF) | (v as u16) << 8,
            0x30 => {
                // DE1/DE0 are written only when the matching DWE bit is 0.
                let mut d = self.dstat;
                if v & 0x20 == 0 {
                    d = (d & !0x80) | (v & 0x80);
                }
                if v & 0x10 == 0 {
                    d = (d & !0x40) | (v & 0x40);
                }
                d = (d & !0x0C) | (v & 0x0C);
                if d & 0xC0 != 0 && v & 0x30 != 0x30 {
                    d |= 0x01;
                }
                self.dstat = d;
            }
            0x31 => self.dmode = v,
            0x32 => self.dcntl = v,
            0x33 => self.il = v & 0xE0,
            0x34 => self.itc = (self.itc & 0x80) | (v & 0x87),
            0x36 => self.rcr = v,
            0x38 => self.cbr = v,
            0x39 => self.bbr = v,
            0x3A => self.cbar = v,
            0x3E => self.omcr = v,
            0x3F => self.icr_base = v & 0xC0,
            _ => {}
        }
    }

    /// Advance on-chip timers; returns number of TOUT edges produced.
    pub fn tick(&mut self, cycles: u32) -> u32 {
        // free running counter: phi/10
        self.frc_div += cycles;
        while self.frc_div >= 10 {
            self.frc_div -= 10;
            self.frc = self.frc.wrapping_sub(1);
        }
        let mut edges = 0;
        self.prt_div += cycles;
        while self.prt_div >= 20 {
            self.prt_div -= 20;
            for t in 0..2 {
                let enabled = self.tcr & (1 << t) != 0;
                if !enabled {
                    continue;
                }
                if self.tmdr[t] == 0 {
                    self.tmdr[t] = self.rldr[t];
                } else {
                    self.tmdr[t] -= 1;
                    if self.tmdr[t] == 0 {
                        self.tcr |= if t == 0 { 0x40 } else { 0x80 };
                        if t == 1 {
                            match (self.tcr >> 2) & 3 {
                                1 => {
                                    self.tout = !self.tout;
                                    edges += 1;
                                }
                                2 => self.tout = false,
                                3 => self.tout = true,
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        edges
    }

    /// Serial channels: time in CPU clocks for one character.
    fn char_time(&self, ch: usize) -> u32 {
        let b = self.asci[ch].cntlb;
        let ps = if b & 0x20 != 0 { 30 } else { 10 };
        let dr = if b & 0x08 != 0 { 64 } else { 16 };
        let ss = b & 7;
        let div = if ss == 7 { 1 } else { 1u32 << ss };
        ps * dr * div * 10
    }

    pub fn asci_tick<B: Board>(&mut self, cycles: u32, board: &mut B) {
        for ch in 0..2 {
            let ct = self.char_time(ch);
            let te = self.asci[ch].cntla & 0x20 != 0;
            let re = self.asci[ch].cntla & 0x40 != 0;
            {
                let a = &mut self.asci[ch];
                if a.tsr_busy > 0 {
                    a.tsr_busy = a.tsr_busy.saturating_sub(cycles);
                }
            }
            if te && self.asci[ch].tdr_full && self.asci[ch].tsr_busy == 0 {
                let v = self.asci[ch].tdr;
                self.asci[ch].tdr_full = false;
                self.asci[ch].tsr_busy = ct;
                board.asci_tx(ch, v);
            }
            let a = &mut self.asci[ch];
            if a.rx_wait > 0 {
                a.rx_wait = a.rx_wait.saturating_sub(cycles);
            }
            if re && a.stat & 0x80 == 0 && a.rx_wait == 0 {
                if let Some(v) = board.asci_rx(ch) {
                    a.rdr = v;
                    a.stat |= 0x80;
                    a.rx_wait = ct;
                }
            }
        }
    }
}

/// Memory/I/O path the CPU core uses.
pub struct Bus<'a, B: Board> {
    pub io: &'a mut Internal,
    pub board: &'a mut B,
}

impl<'a, B: Board> CpuBus for Bus<'a, B> {
    #[inline]
    fn mem_read(&mut self, addr: u16) -> u8 {
        let p = self.io.translate(addr);
        self.board.mem_read(p)
    }
    #[inline]
    fn mem_write(&mut self, addr: u16, v: u8) {
        let p = self.io.translate(addr);
        self.board.note_write(addr, v);
        self.board.mem_write(p, v)
    }
    fn io_read(&mut self, port: u16) -> u8 {
        if self.io.is_internal(port) {
            self.io.read((port & 0x3F) as u8)
        } else {
            self.board.io_read(port)
        }
    }
    fn io_write(&mut self, port: u16, v: u8) {
        if self.io.is_internal(port) {
            self.io.write((port & 0x3F) as u8, v)
        } else {
            self.board.io_write(port, v)
        }
    }
}

/// The whole processor: core plus on-chip peripherals.
#[derive(Clone, Debug)]
pub struct Z180 {
    pub cpu: Cpu,
    pub io: Internal,
    pub irq_count: [u64; 12],
}

pub const IRQ_NAMES: [&str; 12] = ["NMI", "INT0", "INT1 (RTC tick)", "INT2", "PRT0", "PRT1", "DMA0", "DMA1 (voice)", "CSIO", "ASCI0", "ASCI1", "TRAP"];

impl Z180 {
    pub fn new() -> Self {
        Z180 { cpu: Cpu::new(), io: Internal::new(), irq_count: [0; 12] }
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
        self.io.reset();
    }

    pub fn read_logical<B: Board>(&self, board: &mut B, addr: u16) -> u8 {
        board.mem_read(self.io.translate(addr))
    }

    fn pending_internal(&self) -> Option<(usize, u8)> {
        let io = &self.io;
        // PRT0, PRT1
        if io.tcr & 0x40 != 0 && io.tcr & 0x10 != 0 {
            return Some((4, 0x04));
        }
        if io.tcr & 0x80 != 0 && io.tcr & 0x20 != 0 {
            return Some((5, 0x06));
        }
        // DMA end of transfer: DE=0 and DIE=1
        if io.dstat & 0x40 == 0 && io.dstat & 0x04 != 0 {
            return Some((6, 0x08));
        }
        if io.dstat & 0x80 == 0 && io.dstat & 0x08 != 0 {
            return Some((7, 0x0A));
        }
        if io.csio_cntr & 0xC0 == 0xC0 {
            return Some((8, 0x0C));
        }
        for (ch, a) in io.asci.iter().enumerate() {
            let rx = a.stat & 0x08 != 0 && a.stat & 0x80 != 0 && a.rx_wait == 0;
            let tx = a.stat & 0x01 != 0 && !a.tdr_full;
            if rx || tx {
                return Some((9 + ch, 0x0E + 2 * ch as u8));
            }
        }
        None
    }

    /// Run one instruction plus peripheral time. Returns cycles.
    pub fn step<B: Board>(&mut self, board: &mut B) -> u32 {
        // interrupts
        if self.cpu.iff1 && !self.cpu.ei_pending {
            let (i0, i1, i2) = board.int_lines();
            let mut taken: Option<(usize, Option<u16>)> = None;
            if i0 && self.io.itc & 1 != 0 {
                taken = Some((1, if self.cpu.im == 2 { Some(((self.cpu.i as u16) << 8) | 0xFF) } else { None }));
            } else if i1 && self.io.itc & 2 != 0 {
                taken = Some((2, Some(((self.cpu.i as u16) << 8) | (self.io.il as u16))));
            } else if i2 && self.io.itc & 4 != 0 {
                taken = Some((3, Some(((self.cpu.i as u16) << 8) | (self.io.il as u16 + 2))));
            } else if let Some((n, off)) = self.pending_internal() {
                taken = Some((n, Some(((self.cpu.i as u16) << 8) | (self.io.il as u16 + off as u16))));
            }
            if let Some((n, vec)) = taken {
                self.irq_count[n] += 1;
                let mut bus = Bus { io: &mut self.io, board };
                let c = self.cpu.interrupt(&mut bus, vec);
                self.advance(board, c);
                return c;
            }
        } else if (self.cpu.halted || self.cpu.sleeping) && !self.cpu.iff1 {
            // stays halted; time still passes
        }
        let c = {
            let mut bus = Bus { io: &mut self.io, board };
            self.cpu.step(&mut bus)
        };
        if self.cpu.trap {
            self.cpu.trap = false;
            self.irq_count[11] += 1;
        }
        self.advance(board, c);
        c
    }

    fn advance<B: Board>(&mut self, board: &mut B, c: u32) {
        let edges = self.io.tick(c);
        self.io.asci_tick(c, board);
        board.tick(c, edges);
        self.dma(board);
    }

    fn dma<B: Board>(&mut self, board: &mut B) {
        let io = &mut self.io;
        if io.dstat & 0x01 == 0 {
            return;
        }
        // Channel 0: memory to memory burst (only mode the firmware needs).
        if io.dstat & 0x40 != 0 {
            let dm = (io.dmode >> 4) & 3;
            let sm = (io.dmode >> 2) & 3;
            let mut n = io.bcr0 as u32;
            if n == 0 {
                n = 0x10000;
            }
            for _ in 0..n {
                let v = board.mem_read(io.sar0 & 0xFFFFF);
                board.mem_write(io.dar0 & 0xFFFFF, v);
                match sm {
                    0 => io.sar0 = io.sar0.wrapping_add(1),
                    1 => io.sar0 = io.sar0.wrapping_sub(1),
                    _ => {}
                }
                match dm {
                    0 => io.dar0 = io.dar0.wrapping_add(1),
                    1 => io.dar0 = io.dar0.wrapping_sub(1),
                    _ => {}
                }
            }
            io.bcr0 = 0;
            io.dstat &= !0x40;
        }
        // Channel 1: memory <-> I/O paced by DREQ1 (edge sensed).
        if io.dstat & 0x80 != 0 {
            let req = board.dreq1();
            let edge_mode = io.dcntl & 0x02 != 0;
            let fire = if edge_mode { req && !io.dreq1_last } else { req };
            io.dreq1_last = req;
            if fire {
                let dim = io.dcntl & 3;
                if dim < 2 {
                    let v = board.mem_read(io.mar1 & 0xFFFFF);
                    board.io_write(io.iar1, v);
                    io.mar1 = if dim == 0 { io.mar1.wrapping_add(1) } else { io.mar1.wrapping_sub(1) };
                } else {
                    let v = board.io_read(io.iar1);
                    board.mem_write(io.mar1 & 0xFFFFF, v);
                    io.mar1 = if dim == 2 { io.mar1.wrapping_add(1) } else { io.mar1.wrapping_sub(1) };
                }
                io.bcr1 = io.bcr1.wrapping_sub(1);
                if io.bcr1 == 0 {
                    io.dstat &= !0x80;
                }
            }
        }
    }
}
