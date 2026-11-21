//! CPU + board + power management.
//!
//! On-hook the real phone runs from its battery: the firmware saves its state
//! and switches the board off by reading port 0xB8 (with the Z180 IOSTP bit
//! set). Lifting the handset, ringing current or the program button power it
//! up again, which is a CPU reset; the firmware then finds its warm-start
//! signature in RAM and carries on.

use crate::board::{PayBoard, CPU_HZ};
use crate::z180::Z180;

pub struct Machine {
    pub z: Z180,
    pub b: PayBoard,
    pub asleep: bool,
    sleep_inputs: (u8, bool, bool),
    pub sleep_count: u64,
    pub wake_count: u64,
    pub total_cycles: u64,
    /// Breakpoint-style hooks: PCs whose hits are counted (debug panel).
    pub pc_hits: Vec<(u16, u64)>,
    /// Per-PC instruction counts while profiling (tests only).
    pub pc_hist: Option<Vec<u32>>,
    last_ring: bool,
    /// Clock pulses when the board was switched off, and how many times the
    /// clock has woken it since (hourly / per-minute housekeeping).
    sleep_rtc_ticks: u64,
    pub rtc_wakes: u64,
}

impl Machine {
    pub fn new(b: PayBoard) -> Self {
        Machine {
            z: Z180::new(),
            b,
            asleep: false,
            sleep_inputs: (0, false, false),
            sleep_count: 0,
            wake_count: 0,
            total_cycles: 0,
            pc_hits: Vec::new(),
            pc_hist: None,
            last_ring: false,
            sleep_rtc_ticks: 0,
            rtc_wakes: 0,
        }
    }

    pub fn reset(&mut self) {
        self.z.reset();
        self.asleep = false;
        self.b.keys_blocked_until = self.b.cycles + CPU_HZ * 12 / 10;
    }

    fn wake_inputs(&self) -> (u8, bool, bool) {
        (self.b.inputs[0x60] & 0x01, self.b.ring_now(), self.b.program_button)
    }

    /// Run for about `cycles` CPU clocks.
    pub fn run(&mut self, cycles: u64) {
        let end = self.total_cycles + cycles;
        while self.total_cycles < end {
            if self.asleep {
                // Board powered down: only the clock chip keeps time.
                let chunk = 2048u32;
                self.b.sleep_tick(chunk);
                self.total_cycles += chunk as u64;
                let now = self.wake_inputs();
                // off-hook powers the board from the line, whatever happened before
                // ring voltage wakes it once per ring burst (it may power down
                // again in the silence before the next one)
                let ring_edge = now.1 && !self.last_ring;
                self.last_ring = now.1;
                // the clock chip set to a slow pulse (1 s, 1 min, 1 h, unmasked)
                // wakes it too: that is how hourly jobs run on a sleeping phone
                let rtc = &self.b.rtc;
                let rtc_wake = rtc.ticks != self.sleep_rtc_ticks && rtc.slow_period().is_some() && rtc.ce & 1 == 0;
                if rtc_wake {
                    self.rtc_wakes += 1;
                }
                if now.0 == 0 || now.0 != self.sleep_inputs.0 || ring_edge || (now.2 && !self.sleep_inputs.2) || rtc_wake {
                    self.asleep = false;
                    self.wake_count += 1;
                    self.z.reset();
                    // the firmware needs about a second to boot before it
                    // watches the keypad; hold key presses until then
                    self.b.keys_blocked_until = self.b.cycles + CPU_HZ * 12 / 10;
                }
                continue;
            }
            self.b.cur_pc = self.z.cpu.pc;
            if !self.pc_hits.is_empty() {
                let pc = self.z.cpu.pc;
                for h in self.pc_hits.iter_mut() {
                    if h.0 == pc {
                        h.1 += 1;
                    }
                }
            }
            if let Some(h) = self.pc_hist.as_mut() {
                let c = &mut h[self.z.cpu.pc as usize];
                *c = c.saturating_add(1);
            }
            self.b.on_step(&self.z);
            let c = self.z.step(&mut self.b);
            self.total_cycles += c as u64;
            if self.b.power_off {
                self.b.power_off = false;
                self.asleep = true;
                self.sleep_count += 1;
                self.sleep_inputs = self.wake_inputs();
                self.last_ring = self.b.ring_now();
                self.sleep_rtc_ticks = self.b.rtc.ticks;
                self.b.on_power_off();
            }
        }
    }

    pub fn seconds(&self) -> f64 {
        self.total_cycles as f64 / CPU_HZ as f64
    }
}
