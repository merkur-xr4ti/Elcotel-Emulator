//! Files the person chooses: program ROM, voice ROM and rate file dumps
//! opened in the app (remembered in %APPDATA%/ElcotelEmu), and the checks
//! that a file is a dump of the right size. Nothing is built into the
//! program: the dumps live in rom/, voice/ and rates/ folders beside it.

use std::path::{Path, PathBuf};

/// A remembered choice (%APPDATA%\ElcotelEmu\<key>.txt).
pub fn choice(key: &str) -> Option<String> {
    let s = std::fs::read_to_string(crate::app_dir()?.join(format!("{}.txt", key))).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn set_choice(key: &str, value: &str) {
    if let Some(d) = crate::app_dir() {
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(d.join(format!("{}.txt", key)), value);
    }
}

/// Outside program ROM files the person opened before (shown in the list).
pub fn external_roms() -> Vec<PathBuf> {
    choice("firmware-files").map(|s| s.lines().map(|l| PathBuf::from(l.trim())).filter(|p| p.is_file()).collect()).unwrap_or_default()
}

pub fn remember_external_rom(p: &Path) {
    let mut v = external_roms();
    if !v.iter().any(|q| q == p) {
        v.push(p.to_path_buf());
    }
    let s: Vec<String> = v.iter().map(|p| p.display().to_string()).collect();
    set_choice("firmware-files", &s.join("\n"));
}

/// Check a program ROM: 64 KB (smaller power-of-two dumps are mirrored, as
/// the board's spare address lines would). Returns the image and a warning.
pub fn fit_program(data: Vec<u8>) -> Result<(Vec<u8>, Option<String>), String> {
    let n = data.len();
    if n == 0 || n > 0x10000 || !n.is_power_of_two() || n < 0x2000 {
        return Err(format!("That file is {} bytes; a program ROM dump is 65,536 bytes (a 27C512 chip).", n));
    }
    let img: Vec<u8> = data.iter().cycle().take(0x10000).copied().collect();
    let warn = if crate::phone::rom_version(&img).starts_with("PCM 05") || crate::phone::is_compiled(&img) {
        None
    } else {
        Some("This doesn't look like Elcotel Series 5 firmware (no \"PCM 05\" version text). It was loaded anyway; it may not start.".to_string())
    };
    Ok((img, warn))
}

/// Check a voice ROM: up to 128 KB (a 27C010); smaller dumps are mirrored.
pub fn fit_voice(data: Vec<u8>) -> Result<Vec<u8>, String> {
    let n = data.len();
    if n == 0 || n > 0x20000 || !n.is_power_of_two() || n < 0x2000 {
        return Err(format!("That file is {} bytes; a voice ROM dump is up to 131,072 bytes (a 27C010 chip).", n));
    }
    Ok(data.iter().cycle().take(0x20000).copied().collect())
}

/// Pick a file with the system's open dialog.
pub fn pick(title: &str, what: &str, exts: &[&str]) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title(title).add_filter(what, exts).add_filter("All files", &["*"]).pick_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_dump_sizes() {
        let mut rom = vec![0xFFu8; 0x10000];
        rom[0x98..0xA4].copy_from_slice(b"PCM 05.04.04");
        let (img, warn) = fit_program(rom).unwrap();
        assert_eq!(img.len(), 0x10000);
        assert!(warn.is_none());
        assert!(fit_program(vec![0; 1000]).is_err());
        let (half, warn) = fit_program(vec![0x55; 0x8000]).unwrap();
        assert_eq!(half.len(), 0x10000);
        assert!(warn.is_some());
        assert_eq!(fit_voice(vec![1; 0x10000]).unwrap().len(), 0x20000);
        assert!(fit_voice(vec![0; 0x40000]).is_err());
    }
}
