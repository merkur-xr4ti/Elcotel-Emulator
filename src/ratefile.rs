//! PNM rate file (.R94): what can be read from it without the download protocol.

pub struct RateFile {
    pub path: String,
    pub raw: Vec<u8>,
    pub expanded: Vec<u8>,
    pub id: String,
    pub description: String,
}

impl RateFile {
    pub fn load(path: &std::path::Path) -> Option<RateFile> {
        let raw = std::fs::read(path).ok()?;
        Some(RateFile::parse(&path.display().to_string(), raw))
    }

    /// Read a rate file already in memory.
    pub fn parse(path: &str, raw: Vec<u8>) -> RateFile {
        // rate file id: printable digits after the 0x12 header bytes
        let id: String = raw.iter().skip(0x12).take(8).take_while(|b| b.is_ascii_alphanumeric()).map(|&b| b as char).collect();
        // description: a length-prefixed (Pascal) string right after the
        // NUL-terminated copyright text.
        let mut description = String::new();
        if let Some(pos) = find(&raw, b"Elcotel Inc") {
            if let Some(nul) = raw[pos..].iter().position(|&b| b == 0) {
                let at = pos + nul + 1;
                if at < raw.len() {
                    let n = raw[at] as usize;
                    let end = (at + 1 + n).min(raw.len());
                    description = raw[at + 1..end].iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }).collect();
                }
            }
        }
        // The body uses the PNM zero-run coding (00 nn = nn zero bytes);
        // the header is stored plain. Expand from the end of the description.
        let body_start = crate::pnm::RATE_BODY_START.min(raw.len());
        let mut expanded = raw[..body_start].to_vec();
        let mut i = body_start;
        while i < raw.len() {
            let b = raw[i];
            if b == 0 && i + 1 < raw.len() {
                let n = raw[i + 1] as usize;
                expanded.extend(std::iter::repeat_n(0, n));
                i += 2;
            } else {
                expanded.push(b);
                i += 1;
            }
        }
        RateFile { path: path.to_string(), raw, expanded, id, description }
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

// ---------------------------------------------------------------------------
// Editing a rate file the way PNM's rate editor does (Module4 of the
// decompiled PNM Plus: RateMsubDissectR94File / RateMsubSaveR94File).
//
// Unpacked body (0-based offsets):
//   0..800     price band for each area code 200..999 (0 = not allowed,
//              0xFE = decided per exchange by the NXX tables)
//   864..872   how many price bands each of the 8 call categories has
//   877..879   offset of the price band table (4 bytes per band:
//              first-period charge in 5c units, first-period minutes
//              (0xFE = unlimited), additional charge, additional minutes)
//   887        number of area-code entries, 888 number of bands,
//   889        number of NXX tables
// The header (268 bytes) is kept as is; there is no checksum, only the
// unpacked length (bytes 1..4) and a save time stamp (bytes 30..34).

pub const BODY: usize = crate::pnm::RATE_BODY_START;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub first_units: u8,
    pub first_minutes: u8,
    pub add_units: u8,
    pub add_minutes: u8,
}

fn dollars(units: u8) -> String {
    let c = units as u32 * 5;
    format!("${}.{:02}", c / 100, c % 100)
}

impl Band {
    pub fn describe(&self) -> String {
        if self.first_units == 0 && self.add_units == 0 {
            return "free".into();
        }
        let first = if self.first_minutes == 0xFE {
            format!("{} for unlimited time", dollars(self.first_units))
        } else {
            format!("{} for the first {} min", dollars(self.first_units), self.first_minutes)
        };
        if self.first_minutes == 0xFE || self.add_units == 0 {
            first
        } else {
            format!("{}, then {} per {} min", first, dollars(self.add_units), self.add_minutes)
        }
    }
}

impl RateFile {
    fn body(&self) -> &[u8] {
        &self.expanded[BODY.min(self.expanded.len())..]
    }

    fn band_table(&self) -> Option<(usize, usize)> {
        let b = self.body();
        if b.len() < 900 {
            return None;
        }
        let off = b[877] as usize | (b[878] as usize) << 8;
        let n = b[888] as usize;
        if off + n * 4 > b.len() {
            return None;
        }
        Some((off, n))
    }

    /// The price bands, 1-based as the area-code table uses them.
    pub fn bands(&self) -> Vec<Band> {
        let Some((off, n)) = self.band_table() else { return Vec::new() };
        let b = self.body();
        (0..n)
            .map(|k| {
                let p = off + k * 4;
                Band { first_units: b[p], first_minutes: b[p + 1], add_units: b[p + 2], add_minutes: b[p + 3] }
            })
            .collect()
    }

    /// The area codes using each band value (0 = refused, 0xFE = per exchange).
    pub fn npa_counts(&self) -> std::collections::BTreeMap<u8, Vec<u16>> {
        let mut m: std::collections::BTreeMap<u8, Vec<u16>> = Default::default();
        let b = self.body();
        for (i, &band) in b.iter().take(800).enumerate() {
            m.entry(band).or_default().push(200 + i as u16);
        }
        m
    }

    /// Rebuild the .R94 from an edited unpacked image (PNM's zero-run packing).
    pub fn pack(expanded: &[u8]) -> Vec<u8> {
        let mut out = expanded[..BODY.min(expanded.len())].to_vec();
        let body = &expanded[BODY.min(expanded.len())..];
        let mut i = 0;
        while i < body.len() {
            if body[i] == 0 {
                let mut n = 0;
                while i < body.len() && body[i] == 0 && n < 255 {
                    n += 1;
                    i += 1;
                }
                out.push(0);
                out.push(n as u8);
            } else {
                out.push(body[i]);
                i += 1;
            }
        }
        out
    }
}

/// Area codes never opened up: N11 service codes, premium 900/976, 950
/// carrier access, 958/959 test codes, 555, and the non-geographic codes
/// (500-series personal numbers, 600-series Canadian services, 700, 710,
/// 456, the 88x toll-free reserve and 37x/96x reserved for expansion).
pub fn never_open(npa: u16) -> bool {
    npa % 100 == 11
        || matches!(npa, 900 | 976 | 950 | 958 | 959 | 555 | 700 | 710 | 456)
        || matches!(npa, 500 | 521..=533 | 544 | 566 | 577 | 588)
        || matches!(npa, 600 | 622 | 633 | 644 | 655 | 677 | 688)
        || matches!(npa, 880..=887 | 889)
        || matches!(npa, 370..=379 | 960..=969)
}

/// Toll-free area codes added after 800 (they take the band 800 has).
pub const TOLL_FREE: [u16; 6] = [833, 844, 855, 866, 877, 888];

impl RateFile {
    /// The band most listed area codes use (the file's main long-distance band).
    pub fn main_long_distance_band(&self) -> Option<u8> {
        self.npa_counts().into_iter().filter(|(k, _)| *k != 0 && *k != 0xFE).max_by_key(|(_, v)| v.len()).map(|(k, _)| k)
    }

    /// Long-distance bands: everything charged except the local band
    /// (unlimited time) and free bands.
    pub fn long_distance_bands(&self) -> Vec<u8> {
        self.bands()
            .iter()
            .enumerate()
            .filter(|(_, b)| b.first_minutes != 0xFE && !(b.first_units == 0 && b.add_units == 0))
            .map(|(i, _)| i as u8 + 1)
            .collect()
    }

    /// A copy of the file (still a valid .R94) in which area codes it
    /// doesn't list - mostly ones created after it was made - are priced as
    /// the file's main long-distance band instead of refused, and newer
    /// toll-free codes (833-888) take 800's band. Prices are unchanged.
    pub fn with_unlisted_open(&self) -> Vec<u8> {
        if self.band_table().is_none() {
            return self.raw.clone();
        }
        let mut e = self.expanded.clone();
        let main = self.main_long_distance_band();
        let toll_free = e[BODY + (800 - 200)];
        for i in 0..800 {
            let npa = 200 + i as u16;
            if e[BODY + i] != 0 || never_open(npa) {
                continue;
            }
            if TOLL_FREE.contains(&npa) {
                if toll_free != 0 && toll_free != 0xFE {
                    e[BODY + i] = toll_free;
                }
            } else if let Some(main) = main {
                e[BODY + i] = main;
            }
        }
        // PNM stamps the save time (yy mm dd hh mm) at header bytes 30..34
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let (y, m, d) = civil((now / 86400) as i64);
        let stamp = [(y % 100) as u8, m as u8, d as u8, ((now / 3600) % 24) as u8, ((now / 60) % 60) as u8];
        if e.len() > 35 {
            e[30..35].copy_from_slice(&stamp);
        }
        RateFile::pack(&e)
    }

    /// The rate tables the phone is using now, read from its battery RAM
    /// (board.ram index; phys 0x30000 + index). After a download (and on
    /// every reload from the EEPROM) the firmware keeps the unpacked body
    /// in two pieces: everything before the exchange (NXX) tables at
    /// 0x2000 - area-code bands, category counts, offsets and the price
    /// band table - and the NXX tables themselves at 0x3000 (their file
    /// offset, body bytes 879..881, is where the split falls).
    pub fn from_phone_ram(ram: &[u8]) -> Option<RateFile> {
        const MAIN: usize = 0x2000;
        if ram.len() < MAIN + 0x1000 {
            return None;
        }
        let b = &ram[MAIN..MAIN + 0x1000];
        let n = b[888] as usize;
        let counts: usize = b[864..872].iter().map(|&c| c as usize).sum();
        let off = b[877] as usize | (b[878] as usize) << 8;
        let nxx = b[879] as usize | (b[880] as usize) << 8;
        if n == 0 || n > 64 || counts != n || off < 890 || off + n * 4 > nxx.min(0x1000) {
            return None;
        }
        let mut expanded = vec![0u8; BODY];
        expanded.extend_from_slice(&b[..nxx.min(0x1000)]);
        Some(RateFile { path: "phone memory".into(), raw: Vec::new(), expanded, id: String::new(), description: String::new() })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A rate file from app/rates/ (not part of the repository: the tests
    /// that need one are skipped without it).
    fn load() -> Option<RateFile> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("rates");
        let mut v: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
        v.sort();
        let p = v.into_iter().find(|p| p.extension().map(|x| x.to_string_lossy().eq_ignore_ascii_case("r94")).unwrap_or(false))?;
        RateFile::load(&p)
    }

    #[test]
    fn packing_reproduces_the_file() {
        let Some(r) = load() else { return };
        assert_eq!(RateFile::pack(&r.expanded), r.raw);
    }

    #[test]
    fn unlisted_area_codes_open_without_changing_prices() {
        let Some(r) = load() else { return };
        let band = |r: &RateFile, npa: u16| r.body()[(npa - 200) as usize];
        let r2 = RateFile::parse("x", r.with_unlisted_open());
        assert_eq!(r2.expanded.len(), r.expanded.len());
        assert_eq!(r2.bands(), r.bands());
        for npa in 200..1000u16 {
            if band(&r, npa) != 0 {
                assert_eq!(band(&r2, npa), band(&r, npa), "{} changed", npa);
            } else if never_open(npa) {
                assert_eq!(band(&r2, npa), 0, "{} opened", npa);
            } else if !TOLL_FREE.contains(&npa) {
                assert_eq!(band(&r2, npa), r.main_long_distance_band().unwrap(), "{} not opened", npa);
            }
        }
    }
}
