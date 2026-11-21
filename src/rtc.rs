//! OKI MSM6242B-style real-time clock on ports 0x80..0x8F.
//! Registers 0..12 hold BCD time digits, 13..15 are control (CD, CE, CF).
//! Its STD.P output, set to 1/64 s in interrupt mode, drives the Z180 INT1
//! line and is the firmware's main 64 Hz tick. Before switching the board off
//! the firmware sets it to fire once an hour (CE = 0x0E), or once a minute
//! when a job is pending (0x0A); that pulse wakes the board, which then runs
//! its hourly housekeeping at minute 00 (routine D039).
//!
//! Time runs with the emulated CPU clock from the computer's local time at
//! start-up, so pausing or speeding up the emulator moves it the same way the
//! firmware sees it. The firmware (or PNM) can set it by writing the digits.

#[derive(Clone, Debug)]
pub struct Rtc {
    pub cd: u8,
    pub ce: u8,
    pub cf: u8,
    pub irq_flag: bool,
    acc: u64,
    /// CPU clocks per second.
    pub clock_hz: u64,
    /// Seconds added to the start time (the clock was set).
    pub offset_secs: i64,
    pub written: [u8; 16],
    /// STD.P pulses so far (any period).
    pub ticks: u64,
    /// Local time (seconds since 1970) when the emulator started.
    base_secs: i64,
    /// CPU clocks counted since start.
    elapsed: u64,
    /// Last whole second / minute / hour that fired (slow periods).
    last_period: i64,
}

impl Rtc {
    pub fn new(clock_hz: u64) -> Self {
        let host = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        Rtc {
            cd: 0,
            ce: 0,
            cf: 0x04,
            irq_flag: false,
            acc: 0,
            clock_hz,
            offset_secs: 0,
            written: [0; 16],
            ticks: 0,
            base_secs: host + local_offset_secs(),
            elapsed: 0,
            last_period: i64::MIN,
        }
    }

    /// The pulse period in seconds for the slow settings (1 s, 1 min, 1 h),
    /// or None for 1/64 s.
    pub fn slow_period(&self) -> Option<i64> {
        match (self.ce >> 2) & 3 {
            0 => None,
            1 => Some(1),
            2 => Some(60),
            _ => Some(3600),
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        self.elapsed += cycles as u64;
        self.acc += cycles as u64;
        let step = self.clock_hz / 64;
        while self.acc >= step {
            self.acc -= step;
            match self.slow_period() {
                None => self.fire(),
                // slow pulses come on the clock's own second/minute/hour boundary
                Some(p) => {
                    let idx = self.secs().div_euclid(p);
                    if self.last_period == i64::MIN {
                        self.last_period = idx;
                    } else if idx != self.last_period {
                        self.last_period = idx;
                        self.fire();
                    }
                }
            }
        }
    }

    fn fire(&mut self) {
        self.irq_flag = true;
        self.ticks += 1;
    }

    /// INT1 request (active while flag set and not masked).
    pub fn irq(&self) -> bool {
        self.irq_flag && self.ce & 1 == 0
    }

    /// Local time on the clock, seconds since 1970.
    pub fn secs(&self) -> i64 {
        self.base_secs + self.offset_secs + (self.elapsed / self.clock_hz.max(1)) as i64
    }

    fn fields(&self) -> (i64, u32, u32, u32, u32, u32, u32) {
        let secs = self.secs();
        let days = secs.div_euclid(86400);
        let sod = secs.rem_euclid(86400);
        let (y, m, d) = civil_from_days(days);
        let wd = ((days + 4).rem_euclid(7)) as u32; // 1970-01-01 was Thursday
        (y, m, d, (sod / 3600) as u32, ((sod / 60) % 60) as u32, (sod % 60) as u32, wd)
    }

    /// The clock as text (for the user interface).
    pub fn describe(&self) -> String {
        let (y, mo, d, h, mi, s, _) = self.fields();
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, mi, s)
    }

    pub fn read(&mut self, r: u8) -> u8 {
        let (y, mo, d, h, mi, s, wd) = self.fields();
        let y = y.rem_euclid(100) as u32;
        let v = match r & 15 {
            0 => s % 10,
            1 => s / 10,
            2 => mi % 10,
            3 => mi / 10,
            4 => h % 10,
            5 => h / 10,
            6 => d % 10,
            7 => d / 10,
            8 => mo % 10,
            9 => mo / 10,
            10 => y % 10,
            11 => y / 10,
            12 => wd,
            13 => (self.cd & 0x09) as u32 | if self.irq_flag { 4 } else { 0 },
            14 => self.ce as u32,
            _ => self.cf as u32,
        };
        (v as u8) & 0x0F
    }

    pub fn write(&mut self, r: u8, v: u8) {
        self.written[(r & 15) as usize] = v & 15;
        match r & 15 {
            13 => {
                self.cd = v & 0x0F;
                if v & 0x04 == 0 {
                    self.irq_flag = false;
                }
            }
            14 => {
                if (v >> 2) & 3 != (self.ce >> 2) & 3 {
                    self.last_period = i64::MIN;
                }
                self.ce = v & 0x0F;
            }
            15 => self.cf = v & 0x0F,
            12 => {} // day of week follows the date
            d => self.set_digit(d, (v & 0x0F) as u32),
        }
    }

    /// Setting the time: replace one BCD digit and move the clock to match.
    fn set_digit(&mut self, reg: u8, v: u32) {
        let (y, mo, d, h, mi, s, _) = self.fields();
        let (mut y2, mut mo, mut d, mut h, mut mi, mut s) = (y.rem_euclid(100) as u32, mo, d, h, mi, s);
        let set = |x: u32, units: bool| if units { x / 10 * 10 + v } else { v * 10 + x % 10 };
        match reg {
            0 => s = set(s, true),
            1 => s = set(s, false),
            2 => mi = set(mi, true),
            3 => mi = set(mi, false),
            4 => h = set(h, true),
            5 => h = set(h & 0x3F, false) & 0x3F,
            6 => d = set(d, true),
            7 => d = set(d, false),
            8 => mo = set(mo, true),
            9 => mo = set(mo, false),
            10 => y2 = set(y2, true),
            11 => y2 = set(y2, false),
            _ => return,
        }
        // half-written values (a tens digit before its units) are kept in range
        let (s, mi, h) = (s.min(59), mi.min(59), h.min(23));
        let mo = mo.clamp(1, 12);
        let d = d.clamp(1, 31);
        let year = if y2 < 70 { 2000 + y2 as i64 } else { 1900 + y2 as i64 };
        let new = days_from_civil(year, mo, d) * 86400 + (h * 3600 + mi * 60 + s) as i64;
        self.offset_secs += new - self.secs();
        self.last_period = i64::MIN;
    }
}

/// The computer's offset from UTC (seconds), so the phone shows local time.
#[cfg(windows)]
pub fn local_offset_secs() -> i64 {
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: [u16; 8],
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: [u16; 8],
        daylight_bias: i32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTimeZoneInformation(tz: *mut TimeZoneInformation) -> u32;
    }
    // SAFETY: the structure matches TIME_ZONE_INFORMATION and is filled in by the call.
    unsafe {
        let mut tz: TimeZoneInformation = std::mem::zeroed();
        let r = GetTimeZoneInformation(&mut tz);
        let bias = tz.bias + if r == 2 { tz.daylight_bias } else { tz.standard_bias };
        -(bias as i64) * 60
    }
}

#[cfg(not(windows))]
pub fn local_offset_secs() -> i64 {
    0
}

/// Days since 1970-01-01 -> (year, month, day). (H. Hinnant's algorithm.)
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// (year, month, day) -> days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_set_the_clock() {
        let mut r = Rtc::new(6_144_000);
        // set 23:59:58 on 2026-12-31
        for (reg, v) in [(11u8, 2u8), (10, 6), (9, 1), (8, 2), (7, 3), (6, 1), (5, 2), (4, 3), (3, 5), (2, 9), (1, 5), (0, 8)] {
            r.write(reg, v);
        }
        assert_eq!(r.describe(), "2026-12-31 23:59:58");
        // an hourly pulse comes at the turn of the hour
        r.write(14, 0x0E);
        let before = r.ticks;
        for _ in 0..64 {
            r.tick(6_144_000 / 64);
        }
        assert_eq!(r.ticks, before);
        for _ in 0..64 {
            r.tick(6_144_000 / 64);
        }
        assert_eq!(r.ticks, before + 1);
        assert_eq!(r.describe(), "2027-01-01 00:00:00");
    }
}
