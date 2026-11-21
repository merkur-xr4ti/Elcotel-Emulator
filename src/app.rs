//! The window: a drawn payphone on the left, panels on the right.

use crate::audio::AudioShared;
use crate::board::CPU_HZ;
use crate::cvsd;
use crate::phone::Phone;
use crate::ratefile::RateFile;
use crate::sip::{SipCmd, SipConfig};
use crate::z180;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub sip: SipConfig,
    pub ring_firmware: bool,
    pub dial_timeout: f64,
    pub volume: f32,
    pub mic_on: bool,
    /// Which rate file (id and size) was last downloaded into the phone.
    pub rates_loaded: String,
    /// Open the area codes the rate file doesn't list when loading it.
    pub open_unlisted: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            sip: SipConfig::default(),
            ring_firmware: false,
            dial_timeout: 5.0,
            volume: 0.8,
            mic_on: true,
            rates_loaded: String::new(),
            open_unlisted: false,
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Call,
    Line,
    Service,
    Firmware,
    Hardware,
    Voice,
    Rates,
    Cpu,
    Coverage,
    Memory,
}

pub struct App {
    pub phone: Arc<Mutex<Phone>>,
    pub audio: Arc<Mutex<AudioShared>>,
    pub settings: Settings,
    sip_draft: SipConfig,
    tab: Tab,
    rates: Option<RateFile>,
    /// Program ROMs found next to the firmware: (file, version text).
    roms: Vec<(std::path::PathBuf, String)>,
    current_rom_file: String,
    voice_rom: Vec<u8>,
    voice_index: Vec<Vec<(usize, usize)>>,
    /// Where the voice ROM came from (a path, or the built-in one).
    voice_name: String,
    /// Result of the last file the person opened (ok, message).
    file_msg: Option<(bool, String)>,
    /// Last coverage analysis (Coverage tab).
    cov: Option<CovView>,
    cov_time: Option<std::time::Instant>,
    /// The payphone number being typed in the Service tab.
    num_draft: String,
    mem_tex: Option<egui::TextureHandle>,
    mem_region: usize,
    mem_window: f32,
    mem_sel: Option<u32>,
    frames: u64,
    key_flash: Option<(char, f64)>,
    /// Keyboard keys currently held: (phone key, press id).
    kb_held: Vec<(egui::Key, u32)>,
    /// Keypad button held with the mouse.
    mouse_key: Option<(char, u32)>,
    last_text_key: Option<(char, f64)>,
    dis_follow: bool,
    auto_import_done: bool,
    /// What the rate download in progress is sending (marks it loaded when done).
    sending_key: Option<String>,
}

const STEEL_DARK: Color32 = Color32::from_rgb(58, 62, 68);
const STEEL: Color32 = Color32::from_rgb(150, 156, 164);
const STEEL_LIGHT: Color32 = Color32::from_rgb(205, 210, 216);
const LCD_BG: Color32 = Color32::from_rgb(28, 44, 34);
const LCD_FG: Color32 = Color32::from_rgb(140, 235, 150);

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        phone: Arc<Mutex<Phone>>,
        audio: Arc<Mutex<AudioShared>>,
        rates: Option<RateFile>,
        voice_rom: Vec<u8>,
        roms: Vec<(std::path::PathBuf, String)>,
    ) -> App {
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let mut style = (*cc.egui_ctx.style()).clone();
        cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
        style.visuals = egui::Visuals::dark();
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        cc.egui_ctx.set_style(style);
        {
            let mut p = phone.lock().unwrap();
            p.ring_firmware = settings.ring_firmware;
            p.dial_timeout = settings.dial_timeout;
            p.sip_enabled = settings.sip.enabled;
            let _ = p.sip_tx.send(SipCmd::Configure(settings.sip.clone()));
        }
        {
            let mut a = audio.lock().unwrap();
            a.volume = settings.volume;
            a.mic_on = settings.mic_on;
        }
        let current_rom_file = {
            let p = phone.lock().unwrap();
            roms.iter()
                .find(|(path, _)| std::fs::read(path).map(|d| d == p.m.b.rom).unwrap_or(false))
                .and_then(|(path, _)| path.file_name().map(|s| s.to_string_lossy().to_string()))
                .unwrap_or_default()
        };
        let voice_index = vec![cvsd::voice_index(&voice_rom, 0), cvsd::voice_index(&voice_rom, 1)];
        App {
            phone,
            audio,
            sip_draft: settings.sip.clone(),
            settings,
            tab: Tab::Call,
            rates,
            roms,
            current_rom_file,
            voice_rom,
            voice_index,
            voice_name: crate::files::choice("voice")
                .filter(|p| std::path::Path::new(p).is_file())
                .or_else(|| crate::folder_voice().map(|p| p.display().to_string()))
                .unwrap_or_else(|| "voice folder".to_string()),
            file_msg: None,
            cov: None,
            cov_time: None,
            num_draft: String::new(),
            mem_tex: None,
            mem_region: 0,
            mem_window: 3.0,
            mem_sel: None,
            frames: 0,
            key_flash: None,
            kb_held: Vec::new(),
            mouse_key: None,
            last_text_key: None,
            dis_follow: true,
            auto_import_done: false,
            sending_key: None,
        }
    }

    fn apply_settings(&mut self) {
        let mut p = self.phone.lock().unwrap();
        p.ring_firmware = self.settings.ring_firmware;
        p.dial_timeout = self.settings.dial_timeout;
        p.sip_enabled = self.settings.sip.enabled;
        let _ = p.sip_tx.send(SipCmd::Configure(self.settings.sip.clone()));
    }
}

fn vgrad(painter: &egui::Painter, r: Rect, top: Color32, bottom: Color32) {
    let mut m = egui::Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.left_bottom(), bottom);
    m.colored_vertex(r.right_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 2, 3);
    painter.add(egui::Shape::mesh(m));
}

fn hgrad(painter: &egui::Painter, r: Rect, left: Color32, right: Color32) {
    let mut m = egui::Mesh::default();
    m.colored_vertex(r.left_top(), left);
    m.colored_vertex(r.right_top(), right);
    m.colored_vertex(r.left_bottom(), left);
    m.colored_vertex(r.right_bottom(), right);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 2, 3);
    painter.add(egui::Shape::mesh(m));
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        ctx.request_repaint();
        self.keyboard(ctx);
        self.rate_import_housekeeping();

        egui::SidePanel::left("payphone").exact_width(440.0).resizable(false).show(ctx, |ui| {
            self.draw_payphone(ui);
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (t, name) in [
                    (Tab::Call, "Call"),
                    (Tab::Line, "Internet line"),
                    (Tab::Service, "Service"),
                    (Tab::Firmware, "FW ROM"),
                    (Tab::Hardware, "Hardware"),
                    (Tab::Voice, "Voice ROM"),
                    (Tab::Rates, "Rate file"),
                    (Tab::Cpu, "CPU"),
                    (Tab::Coverage, "Coverage"),
                    (Tab::Memory, "Memory map"),
                ] {
                    ui.selectable_value(&mut self.tab, t, egui::RichText::new(name).size(15.0));
                }
            });
            ui.separator();
            match self.tab {
                Tab::Call => self.tab_call(ui),
                Tab::Line => self.tab_line(ui),
                Tab::Service => self.tab_service(ui),
                Tab::Firmware => self.tab_firmware(ui),
                Tab::Hardware => self.tab_hardware(ui),
                Tab::Voice => self.tab_voice(ui),
                Tab::Rates => self.tab_rates(ui),
                Tab::Cpu => self.tab_cpu(ui),
                Tab::Coverage => self.tab_coverage(ui),
                Tab::Memory => self.tab_memory(ui),
            }
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
        self.phone.lock().unwrap().save_nvram();
    }

    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(15)
    }
}

impl App {
    fn keyboard(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let evs = ctx.input(|i| i.events.clone());
        let typing = ctx.memory(|m| m.focused().is_some());
        // Releases always count, so a key can't get stuck down.
        for e in &evs {
            if let egui::Event::Key { key, pressed: false, .. } = e {
                let mut p = self.phone.lock().unwrap();
                self.kb_held.retain(|(k, id)| {
                    if k == key {
                        p.key_release(*id);
                        false
                    } else {
                        true
                    }
                });
            }
        }
        if typing {
            return;
        }
        let mut handled_symbol = false;
        for e in &evs {
            if let egui::Event::Key { key, pressed: true, repeat, modifiers, .. } = e {
                if *repeat {
                    // Windows key-repeat: one press is one digit
                    if matches!(
                        key,
                        egui::Key::Num0
                            | egui::Key::Num1
                            | egui::Key::Num2
                            | egui::Key::Num3
                            | egui::Key::Num4
                            | egui::Key::Num5
                            | egui::Key::Num6
                            | egui::Key::Num7
                            | egui::Key::Num8
                            | egui::Key::Num9
                    ) {
                        handled_symbol = true;
                    }
                    continue;
                }
                let digit = match key {
                    egui::Key::Num0 => Some('0'),
                    egui::Key::Num1 => Some('1'),
                    egui::Key::Num2 => Some('2'),
                    egui::Key::Num3 => Some('3'),
                    egui::Key::Num4 => Some('4'),
                    egui::Key::Num5 => Some('5'),
                    egui::Key::Num6 => Some('6'),
                    egui::Key::Num7 => Some('7'),
                    egui::Key::Num8 => Some('8'),
                    egui::Key::Num9 => Some('9'),
                    _ => None,
                };
                if let Some(d) = digit {
                    let c = if modifiers.shift && d == '8' {
                        '*'
                    } else if modifiers.shift && d == '3' {
                        '#'
                    } else if modifiers.shift {
                        continue;
                    } else {
                        d
                    };
                    handled_symbol = true;
                    if self.kb_held.iter().any(|(k, _)| k == key) {
                        continue;
                    }
                    let id = self.phone.lock().unwrap().key_hold(c);
                    self.kb_held.push((*key, id));
                    self.key_flash = Some((c, now));
                } else if *key == egui::Key::Space {
                    let mut p = self.phone.lock().unwrap();
                    let on = p.m.b.on_hook();
                    p.set_hook(!on);
                }
            }
        }
        // '*' and '#' from the number pad (or layouts without a key event)
        if !handled_symbol {
            for e in &evs {
                if let egui::Event::Text(t) = e {
                    for c in t.chars() {
                        if c == '*' || c == '#' {
                            // ignore auto-repeat of the same symbol
                            if let Some((lc, lt)) = self.last_text_key {
                                if lc == c && now - lt < 0.35 {
                                    self.last_text_key = Some((c, now));
                                    continue;
                                }
                            }
                            self.last_text_key = Some((c, now));
                            self.phone.lock().unwrap().press_key(c);
                            self.key_flash = Some((c, now));
                        }
                    }
                }
            }
        }
    }

    fn draw_payphone(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        let painter = ui.painter_at(avail);
        let now = ui.input(|i| i.time);
        // background
        vgrad(&painter, avail, Color32::from_rgb(34, 38, 46), Color32::from_rgb(18, 20, 24));

        let body = Rect::from_min_size(avail.min + Vec2::new(120.0, 24.0), Vec2::new(296.0, avail.height().min(780.0) - 48.0));
        // housing with a brushed look
        painter.rect_filled(body.translate(Vec2::new(6.0, 8.0)), 18.0, Color32::from_black_alpha(120));
        painter.rect_filled(body, 18.0, STEEL);
        let inner = body.shrink(6.0);
        hgrad(&painter, inner, STEEL_LIGHT, STEEL);
        for i in 0..40 {
            let x = inner.left() + i as f32 * inner.width() / 40.0;
            painter.line_segment(
                [Pos2::new(x, inner.top() + 10.0), Pos2::new(x, inner.bottom() - 10.0)],
                Stroke::new(0.6_f32, Color32::from_white_alpha(if i % 2 == 0 { 18 } else { 6 })),
            );
        }
        painter.rect_stroke(body, 18.0, Stroke::new(2.0_f32, STEEL_DARK), StrokeKind::Inside);

        let (state, digits, prompt, on_hook, ringing, ringer, key_down, asleep, credit_note, incoming, typed, pending) = {
            let p = self.phone.lock().unwrap();
            let ringer = self.audio.lock().unwrap().ringer;
            (
                p.line,
                p.dialed.clone(),
                p.current_prompt,
                p.m.b.on_hook(),
                p.m.b.ring_now(),
                ringer,
                p.m.b.key_down(),
                p.m.asleep,
                p.escrow_cents,
                p.incoming_from.clone(),
                p.typed.clone(),
                p.m.b.keys_pending(),
            )
        };

        // ---- status display (instruction card area) ----
        let lcd = Rect::from_min_size(inner.min + Vec2::new(16.0, 18.0), Vec2::new(inner.width() - 32.0, 120.0));
        painter.rect_filled(lcd.expand(4.0), 6.0, STEEL_DARK);
        painter.rect_filled(lcd, 4.0, LCD_BG);
        let mono = FontId::monospace(15.0);
        let l1 = if asleep {
            "BOARD ASLEEP (ON HOOK)".to_string()
        } else if on_hook {
            "ON HOOK - READY".into()
        } else {
            format!("OFF HOOK  {}", state.label().to_uppercase())
        };
        let l2 = if let Some(f) = &incoming {
            format!("CALL FROM {}", f)
        } else if typed.is_empty() {
            "KEYS: -".into()
        } else {
            format!("KEYS: {}{}", typed, if pending > 0 { "…" } else { "" })
        };
        let l2b = if digits.is_empty() { "SENT TO LINE: -".to_string() } else { format!("SENT TO LINE: {}", digits) };
        let l3 = match prompt {
            Some(p) => format!("VOICE PROMPT {:02X}", p),
            None => format!("COINS HELD: {}c", credit_note),
        };
        for (i, l) in [l1, l2, l2b, l3].iter().enumerate() {
            painter.text(lcd.left_top() + Vec2::new(10.0, 12.0 + i as f32 * 26.0), Align2::LEFT_TOP, l, mono.clone(), LCD_FG);
        }

        // ---- coin slot + coins ----
        let slot_area = Rect::from_min_size(Pos2::new(inner.left() + 16.0, lcd.bottom() + 22.0), Vec2::new(inner.width() - 32.0, 70.0));
        painter.rect_filled(slot_area, 10.0, STEEL_DARK);
        let slot = Rect::from_center_size(Pos2::new(slot_area.right() - 36.0, slot_area.center().y), Vec2::new(10.0, 46.0));
        painter.rect_filled(slot, 3.0, Color32::BLACK);
        painter.text(Pos2::new(slot.left() - 8.0, slot_area.top() + 6.0), Align2::RIGHT_TOP, "INSERT COIN", FontId::proportional(11.0), STEEL_LIGHT);
        let coins = [
            ("5¢", 0usize, 15.0f32, Color32::from_rgb(190, 190, 196)),
            ("10¢", 1, 13.0, Color32::from_rgb(210, 212, 220)),
            ("25¢", 2, 19.0, Color32::from_rgb(200, 202, 208)),
        ];
        for (i, (label, which, r, col)) in coins.iter().enumerate() {
            let c = Pos2::new(slot_area.left() + 34.0 + i as f32 * 52.0, slot_area.center().y + 4.0);
            let rect = Rect::from_center_size(c, Vec2::splat(r * 2.0 + 6.0));
            let resp = ui.interact(rect, ui.id().with(("coin", i)), Sense::click());
            let hover = resp.hovered();
            painter.circle_filled(c + Vec2::new(1.5, 2.0), *r, Color32::from_black_alpha(140));
            painter.circle_filled(c, *r, if hover { Color32::WHITE } else { *col });
            painter.circle_stroke(c, *r - 2.5, Stroke::new(1.0_f32, Color32::from_gray(120)));
            painter.text(c, Align2::CENTER_CENTER, *label, FontId::proportional(12.0), Color32::from_gray(40));
            if resp.clicked() {
                self.phone.lock().unwrap().drop_coin(*which);
            }
            resp.on_hover_text(format!("Drop a {} into the slot", label));
        }

        // ---- keypad ----
        let kp_top = slot_area.bottom() + 26.0;
        let keys = [
            ('1', ""),
            ('2', "ABC"),
            ('3', "DEF"),
            ('4', "GHI"),
            ('5', "JKL"),
            ('6', "MNO"),
            ('7', "PRS"),
            ('8', "TUV"),
            ('9', "WXY"),
            ('*', ""),
            ('0', "OPER"),
            ('#', ""),
        ];
        let kw = 66.0;
        let kh = 52.0;
        let gap = 14.0;
        let kp_left = inner.center().x - (kw * 3.0 + gap * 2.0) / 2.0;
        let kp_rect = Rect::from_min_size(Pos2::new(kp_left - 14.0, kp_top - 14.0), Vec2::new(kw * 3.0 + gap * 2.0 + 28.0, kh * 4.0 + gap * 3.0 + 28.0));
        painter.rect_filled(kp_rect, 12.0, STEEL_DARK);
        let flash = self.key_flash.and_then(|(c, t)| if now - t < 0.15 { Some(c) } else { None });
        let kb_down: Vec<char> = self
            .kb_held
            .iter()
            .filter_map(|(k, _)| match k {
                egui::Key::Num0 => Some('0'),
                egui::Key::Num1 => Some('1'),
                egui::Key::Num2 => Some('2'),
                egui::Key::Num3 => Some('3'),
                egui::Key::Num4 => Some('4'),
                egui::Key::Num5 => Some('5'),
                egui::Key::Num6 => Some('6'),
                egui::Key::Num7 => Some('7'),
                egui::Key::Num8 => Some('8'),
                egui::Key::Num9 => Some('9'),
                _ => None,
            })
            .collect();
        let mut mouse_on: Option<char> = None;
        for (i, (k, letters)) in keys.iter().enumerate() {
            let col = (i % 3) as f32;
            let row = (i / 3) as f32;
            let r = Rect::from_min_size(Pos2::new(kp_left + col * (kw + gap), kp_top + row * (kh + gap)), Vec2::new(kw, kh));
            let resp = ui.interact(r, ui.id().with(("key", i)), Sense::click());
            if resp.is_pointer_button_down_on() {
                mouse_on = Some(*k);
            }
            let down = resp.is_pointer_button_down_on() || key_down == Some(*k) || flash == Some(*k) || kb_down.contains(k);
            let rr = if down { r.translate(Vec2::new(0.0, 2.0)) } else { r };
            painter.rect_filled(r.translate(Vec2::new(0.0, 4.0)), 9.0, Color32::from_black_alpha(160));
            vgrad(
                &painter,
                rr.shrink(1.0),
                if down { STEEL } else { Color32::from_rgb(232, 235, 240) },
                if down { STEEL_DARK } else { Color32::from_rgb(150, 155, 162) },
            );
            painter.rect_stroke(rr, 9.0, Stroke::new(1.5_f32, Color32::from_gray(80)), StrokeKind::Inside);
            painter.text(
                rr.center() - Vec2::new(0.0, if letters.is_empty() { 0.0 } else { 6.0 }),
                Align2::CENTER_CENTER,
                k.to_string(),
                FontId::proportional(24.0),
                Color32::from_gray(25),
            );
            if !letters.is_empty() {
                painter.text(rr.center() + Vec2::new(0.0, 15.0), Align2::CENTER_CENTER, *letters, FontId::proportional(10.0), Color32::from_gray(50));
            }
        }
        // The key is held on the board for exactly as long as the mouse
        // button is held on it (with a short minimum).
        let current = self.mouse_key.map(|(c, _)| c);
        if current != mouse_on {
            let mut p = self.phone.lock().unwrap();
            if let Some((_, id)) = self.mouse_key.take() {
                p.key_release(id);
            }
            if let Some(c) = mouse_on {
                let id = p.key_hold(c);
                self.mouse_key = Some((c, id));
            }
        }
        // Clicking anywhere on the phone takes the keyboard back from text boxes.
        if ui.input(|i| i.pointer.any_pressed()) && ui.rect_contains_pointer(avail) {
            if let Some(id) = ui.memory(|m| m.focused()) {
                ui.memory_mut(|m| m.surrender_focus(id));
            }
        }

        // ---- coin return ----
        let cr = Rect::from_min_size(Pos2::new(inner.center().x - 50.0, kp_rect.bottom() + 24.0), Vec2::new(100.0, 64.0));
        painter.rect_filled(cr, 10.0, STEEL_DARK);
        painter.rect_filled(cr.shrink(8.0), 6.0, Color32::from_rgb(20, 20, 22));
        let returned = self.phone.lock().unwrap().returned_cents;
        let crresp = ui.interact(cr, ui.id().with("coinreturn"), Sense::click());
        if returned > 0 {
            for k in 0..(returned / 5).min(6) {
                painter.circle_filled(Pos2::new(cr.left() + 22.0 + k as f32 * 11.0, cr.bottom() - 18.0), 9.0, Color32::from_rgb(200, 202, 208));
            }
            painter.text(
                cr.center() - Vec2::new(0.0, 10.0),
                Align2::CENTER_CENTER,
                format!("{}c", returned),
                FontId::proportional(13.0),
                Color32::from_rgb(255, 220, 120),
            );
        }
        if crresp.clicked() && returned > 0 {
            let mut p = self.phone.lock().unwrap();
            p.note(format!("Took {} cents from the coin return", returned));
            p.returned_cents = 0;
        }
        crresp.on_hover_text("Refunded coins land here; click to take them");
        painter.text(Pos2::new(cr.center().x, cr.bottom() + 12.0), Align2::CENTER_CENTER, "COIN RETURN", FontId::proportional(11.0), STEEL_DARK);

        // ---- cradle + handset ----
        let cradle = Rect::from_min_size(Pos2::new(avail.left() + 30.0, body.top() + 140.0), Vec2::new(90.0, 300.0));
        painter.rect_filled(cradle, 10.0, STEEL_DARK);
        let hook_lever = Rect::from_min_size(Pos2::new(cradle.right() - 18.0, cradle.top() + 40.0), Vec2::new(14.0, if on_hook { 18.0 } else { 34.0 }));
        painter.rect_filled(hook_lever, 3.0, Color32::from_gray(30));
        let shake = if ringer && ringing { ((now * 40.0).sin() * 3.0) as f32 } else { 0.0 };
        let handset = if on_hook {
            Rect::from_min_size(Pos2::new(cradle.left() + 8.0 + shake, cradle.top() - 20.0), Vec2::new(62.0, 340.0))
        } else {
            Rect::from_min_size(Pos2::new(avail.left() + 6.0, avail.top() + 20.0), Vec2::new(62.0, 340.0))
        };
        // cord
        let cord_start = Pos2::new(cradle.center().x, cradle.bottom() - 10.0);
        let cord_end = Pos2::new(handset.center().x, handset.bottom() - 6.0);
        let mut pts = Vec::new();
        for i in 0..=40 {
            let f = i as f32 / 40.0;
            let p = cord_start + (cord_end - cord_start) * f + Vec2::new(((f * 60.0).sin()) * 6.0, 40.0 * (f * std::f32::consts::PI).sin());
            pts.push(p);
        }
        painter.add(egui::Shape::line(pts, Stroke::new(3.0_f32, Color32::from_gray(25))));
        let hresp = ui.interact(handset, ui.id().with("handset"), Sense::click());
        painter.rect_filled(handset.translate(Vec2::new(5.0, 6.0)), 26.0, Color32::from_black_alpha(140));
        let hcol = if hresp.hovered() { Color32::from_rgb(40, 40, 44) } else { Color32::from_rgb(22, 22, 25) };
        painter.rect_filled(handset, 26.0, hcol);
        painter.rect_filled(Rect::from_min_size(handset.min + Vec2::new(14.0, 70.0), Vec2::new(34.0, 200.0)), 12.0, Color32::from_rgb(34, 34, 38));
        for cap in [handset.top() + 34.0, handset.bottom() - 34.0] {
            painter.circle_filled(Pos2::new(handset.center().x, cap), 22.0, Color32::from_rgb(30, 30, 34));
            for k in 0..5 {
                painter.circle_filled(Pos2::new(handset.center().x - 10.0 + k as f32 * 5.0, cap), 1.6, Color32::from_gray(70));
            }
        }
        if hresp.clicked() {
            let mut p = self.phone.lock().unwrap();
            p.set_hook(!on_hook);
        }
        hresp.on_hover_text(if on_hook { "Lift the handset (or press Space)" } else { "Hang up (or press Space)" });

        // ringing bell indicator
        if ringer {
            let c = Pos2::new(body.right() - 26.0, body.top() + 4.0);
            painter.circle_filled(c, 14.0, if ringing { Color32::from_rgb(250, 200, 60) } else { Color32::from_rgb(120, 100, 40) });
            painter.text(c, Align2::CENTER_CENTER, "🔔", FontId::proportional(14.0), Color32::BLACK);
        }
        painter.text(Pos2::new(body.center().x, body.bottom() - 16.0), Align2::CENTER_CENTER, "SERIES 5 EMULATOR", FontId::proportional(11.0), STEEL_DARK);
        let _ = state;
    }

    fn tab_call(&mut self, ui: &mut egui::Ui) {
        let p = self.phone.lock().unwrap();
        let a = self.audio.lock().unwrap();
        egui::Grid::new("callgrid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            ui.label("Handset");
            ui.strong(if p.m.b.on_hook() { "on hook" } else { "lifted" });
            ui.end_row();
            ui.label("Line");
            ui.strong(p.line.label());
            ui.end_row();
            ui.label("Number dialled by the firmware");
            ui.strong(if p.dialed.is_empty() { "-".to_string() } else { p.dialed.clone() });
            ui.end_row();
            ui.label("Internet line");
            ui.strong(format!("{}{}", p.sip_status, if p.sip_registered { " (registered)" } else { "" }));
            ui.end_row();
            ui.label("Coins held / cash box / returned");
            ui.strong(format!("{}c / {}c / {}c", p.escrow_cents, p.cashbox_cents, p.returned_cents));
            ui.end_row();
            ui.label("Board");
            ui.strong(if p.m.asleep {
                "asleep (powered down on hook, like the real phone)".to_string()
            } else {
                format!("running firmware {} at {:.1} MHz", p.firmware_version(), CPU_HZ as f64 / 1e6)
            });
            ui.end_row();
            ui.label("Sound");
            ui.strong(if let Some(e) = &a.error { e.clone() } else { a.device_name.clone() });
            ui.end_row();
        });
        ui.horizontal(|ui| {
            ui.label("Speaker");
            ui.add(egui::ProgressBar::new((a.out_level * 2.0).min(1.0)).desired_width(120.0));
            ui.label("Microphone");
            ui.add(egui::ProgressBar::new((a.mic_level * 2.0).min(1.0)).desired_width(120.0));
        });
        drop(a);
        ui.add_space(6.0);
        ui.label(egui::RichText::new("How to use it: lift the handset (click it or press Space), dial with the keypad or your number keys, drop coins when the phone asks. The firmware decides the price, speaks the prompts and dials the number out over the internet line.").weak());
        ui.add_space(6.0);
        // ELCOMPILER programs PRINT to the board's second serial port
        let printed = String::from_utf8_lossy(&p.m.b.serial_out[1]).replace('\r', "");
        if crate::phone::is_compiled(&p.m.b.rom) || !printed.is_empty() {
            ui.horizontal(|ui| {
                ui.heading("Program output");
                ui.label(egui::RichText::new("(what the BASIC program PRINTs)").weak());
            });
            let lines: Vec<&str> = printed.lines().collect();
            let shown = lines[lines.len().saturating_sub(12)..].join("\n");
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.monospace(if shown.is_empty() { "(nothing yet)".to_string() } else { shown });
            });
            ui.add_space(6.0);
        }
        ui.heading("What happened");
        let log: Vec<_> = p.log.iter().rev().take(200).cloned().collect();
        let prompts: Vec<_> = p.prompts.iter().rev().take(16).cloned().collect();
        drop(p);
        ui.horizontal(|ui| {
            ui.label("Recent voice prompts:");
            for (_, pr) in prompts.iter().rev() {
                ui.monospace(format!("{:02X}", pr));
            }
        });
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for l in log {
                ui.horizontal(|ui| {
                    ui.monospace(format!("{:9.2}s", l.t));
                    ui.label(l.text);
                });
            }
        });
    }

    fn tab_line(&mut self, ui: &mut egui::Ui) {
        ui.heading("Internet calling (SIP)");
        ui.label("The phone dials out through a SIP account (a VoIP provider, a PBX such as Asterisk/FreePBX, or another softphone). Calls are sent as sip:<number>@<domain>.");
        ui.add_space(4.0);
        let d = &mut self.sip_draft;
        egui::Grid::new("sipgrid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Use internet calling");
            ui.checkbox(&mut d.enabled, "");
            ui.end_row();
            ui.label("SIP server");
            ui.add(egui::TextEdit::singleline(&mut d.server).hint_text("your provider's server, e.g. sip.example.com or host:port").desired_width(320.0));
            ui.end_row();
            ui.label("User name");
            ui.add(egui::TextEdit::singleline(&mut d.username).desired_width(260.0));
            ui.end_row();
            ui.label("Password");
            ui.add(egui::TextEdit::singleline(&mut d.password).password(true).desired_width(260.0));
            ui.end_row();
        });
        egui::CollapsingHeader::new("More SIP settings (usually left alone)").default_open(false).show(ui, |ui| {
            egui::Grid::new("sipgrid2").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Login name (if different)");
                ui.add(egui::TextEdit::singleline(&mut d.auth_user).desired_width(260.0));
                ui.end_row();
                ui.label("Caller name");
                ui.add(egui::TextEdit::singleline(&mut d.display_name).desired_width(260.0));
                ui.end_row();
                ui.label("Domain for dialled numbers");
                ui.add(egui::TextEdit::singleline(&mut d.domain).hint_text("same as server").desired_width(260.0));
                ui.end_row();
                ui.label("Register (receive calls)");
                ui.checkbox(&mut d.register, "");
                ui.end_row();
                ui.label("Local SIP port");
                ui.add(egui::DragValue::new(&mut d.local_port).range(0..=65535));
                ui.end_row();
            });
        });
        ui.horizontal(|ui| {
            let missing = if self.sip_draft.server.trim().is_empty() {
                Some("enter your SIP server")
            } else if self.sip_draft.username.trim().is_empty() {
                Some("enter the user name")
            } else {
                None
            };
            if ui.add_enabled(missing.is_none(), egui::Button::new("Save and connect")).clicked() {
                self.settings.sip = self.sip_draft.clone();
                self.apply_settings();
            }
            if let Some(m) = missing {
                ui.label(egui::RichText::new(m).weak());
            } else if self.sip_draft != self.settings.sip {
                ui.label(egui::RichText::new("not saved yet").color(Color32::YELLOW));
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Start the call this many seconds after the last digit (complete US numbers go after 1.5 s):");
            if ui.add(egui::Slider::new(&mut self.settings.dial_timeout, 2.0..=12.0)).changed() {
                self.apply_settings();
            }
        });
        if let Some(d) = crate::app_dir() {
            ui.label(egui::RichText::new(format!("Line log: {}", d.join("log.txt").display())).weak());
        }
        ui.horizontal(|ui| {
            if ui.checkbox(&mut self.settings.mic_on, "Microphone on during calls").changed() {
                self.audio.lock().unwrap().mic_on = self.settings.mic_on;
            }
            ui.label("Volume");
            if ui.add(egui::Slider::new(&mut self.settings.volume, 0.0..=1.5)).changed() {
                self.audio.lock().unwrap().volume = self.settings.volume;
            }
        });
        ui.separator();
        let p = self.phone.lock().unwrap();
        ui.label(format!("Status: {}", p.sip_status));
        let log: Vec<String> = p.sip_log.iter().rev().take(120).cloned().collect();
        drop(p);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for l in log {
                ui.monospace(l);
            }
        });
    }

    fn tab_service(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().id_salt("servicescroll").auto_shrink([false, false]).show(ui, |ui| {
            self.service_page(ui);
        });
    }

    fn service_page(&mut self, ui: &mut egui::Ui) {
        let mut p = self.phone.lock().unwrap();
        ui.heading("Service");
        ui.label(format!("Station ID (PNM logs on with this): {}    Telemetry (bypass) code: {}", p.station_id(), p.telemetry_code()))
            .on_hover_text("The code read from the phone's settings. Factory defaults use #99999999; an options file or the custom firmware's built-in options can change it (the custom firmware sets #88888888).");
        // ---- the payphone's own number ----
        ui.horizontal(|ui| {
            let n = p.phone_number();
            let set = n.len() == 10 && n.chars().all(|c| c.is_ascii_digit()) && n.chars().any(|c| c != '0');
            let shown = if set { format!("{}-{}-{}", &n[..3], &n[3..6], &n[6..]) } else { "not set".to_string() };
            ui.label(format!("Payphone number: {}", shown));
            if self.num_draft.is_empty() && set {
                self.num_draft = n.clone();
            }
            ui.add(egui::TextEdit::singleline(&mut self.num_draft).desired_width(120.0).hint_text("10 digits"));
            let digits: String = self.num_draft.chars().filter(|c| c.is_ascii_digit()).collect();
            let digits = if digits.len() == 11 && digits.starts_with('1') { digits[1..].to_string() } else { digits };
            let ok = digits.len() == 10 && digits != n && !p.importing();
            if ui.add_enabled(ok, egui::Button::new("Set in the phone")).clicked() {
                p.set_phone_number(&digits);
            }
        });
        ui.label(egui::RichText::new("The number the firmware reports as its own (register 404, call records) and that PNM logs on with. Use your SIP account's number; setting it stores it in the phone's EEPROM.").weak());
        ui.label(format!("Phone clock: {}  (starts from this computer's local time; PNM sets it when it logs on). The clock has woken the sleeping board {} times for its hourly jobs.", p.m.b.rtc.describe(), p.m.rtc_wakes));
        // ---- options that refuse calls ----
        {
            let mut allow = !p.out_of_state_blocked();
            ui.horizontal_wrapped(|ui| {
                let busy = p.importing();
                let r = ui.add_enabled(!busy, egui::Checkbox::new(&mut allow, "Allow coin-paid long-distance calls to other states (1 + area code)"));
                if r.changed() {
                    p.set_out_of_state_blocked(!allow);
                }
                if busy {
                    ui.spinner();
                    ui.label(p.pnm_status.clone());
                }
            });
            ui.label(
                egui::RichText::new(
                    "Off means the phone's options (byte C242, bit 2) make the firmware answer \"invalid number\" for any area code outside your own state. A PNM Plus options file can set it; switching it here changes only that setting, not the firmware, and stores it in the phone's EEPROM. Sending that options file from PNM Plus again brings the block back.",
                )
                .weak(),
            );
            let mut refund = p.answer_supervision();
            let busy = p.importing();
            if ui.add_enabled(!busy, egui::Checkbox::new(&mut refund, "Refund calls nobody answers (answer supervision)")).changed() {
                p.set_answer_supervision(refund);
            }
            ui.label(
                egui::RichText::new(
                    "With this on (byte C243, bit 1) the firmware only keeps the coins once the line reverses polarity, which the emulated line does when the internet call is answered; busy, failed and unanswered calls are refunded when you hang up. Off, it keeps the coins of every call it dials. Stored in the phone's EEPROM like the setting above.",
                )
                .weak(),
            );
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Factory default (program button + handset)").clicked() {
                p.start_factory_default();
                self.settings.rates_loaded.clear();
            }
            if p.defaulting.is_some() {
                ui.spinner();
                ui.label("defaulting...");
            }
        });
        let mut btn = p.m.b.program_button;
        if ui.checkbox(&mut btn, "Hold the program (reset) button").changed() {
            p.m.b.program_button = btn;
        }
        ui.horizontal(|ui| {
            if ui.button("Power-cycle the board").clicked() {
                p.m.reset();
                p.note("Board power-cycled");
            }
            if ui.button("Remove battery and EEPROM (erase memory)").clicked() {
                p.wipe_memory();
                self.settings.rates_loaded.clear();
            }
        });
        ui.separator();
        ui.heading("Line");
        let mut ring = p.m.b.ring_signal;
        if ui.checkbox(&mut ring, "Ring the line now (test ringing, no caller)").changed() {
            p.m.b.ring_signal = ring;
            self.audio.lock().unwrap().ringer = ring;
        }
        let mut rf = self.settings.ring_firmware;
        if ui.checkbox(&mut rf, "Let the firmware hear incoming internet calls (it counts rings and its modem may answer for remote programming)").changed() {
            self.settings.ring_firmware = rf;
            p.ring_firmware = rf;
        }
        ui.separator();
        ui.heading("Codes you can dial (from the payphone wiki)");
        let tc = p.telemetry_code();
        egui::Grid::new("codes").num_columns(2).striped(true).show(ui, |ui| {
            for (c, d) in [
                (tc.as_str(), "Enter telemetry with this phone's bypass code. The phone says \"thank you\". (Factory default #99999999; the custom firmware's built-in options make it #88888888.)"),
                ("962", "Zero the coin box counters"),
                ("963", "Leave telemetry"),
                ("965 / 966", "Return / collect the coins"),
                ("967 / 972", "Report / clear alarms"),
                ("969", "Copy RAM settings into the EEPROM"),
                ("975 then *2*", "Bright coin line mode (calls without coins)"),
                ("986", "Report line current status"),
                ("999", "Default the board"),
                ("404", "Payphone number (ANI)"),
                ("800 / 801", "Cash box total / totalizer"),
            ] {
                ui.monospace(c);
                ui.label(d);
                ui.end_row();
            }
        });
        ui.label("In telemetry, dial a 3-digit register to hear its value; *value* changes it.");
    }

    fn tab_hardware(&mut self, ui: &mut egui::Ui) {
        let mut p = self.phone.lock().unwrap();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.heading("Hardware: the board's I/O ports");
            ui.label("The Z180 CPU talks to the rest of the board through numbered I/O ports. Reading a port gives the state of switches and detectors (inputs); writing one sets latches that drive relays, the voice chip and the line (outputs). There is no schematic: each meaning below comes from how the firmware uses the port. The phone drives the inputs for you; you can force any bit here to try the firmware's other paths. Hover over a bit for what it does.");
            ui.add_space(6.0);
            ui.heading("Inputs the firmware reads");
            // (port, title, what it is, [(bit name, explanation); 8])
            type PortBits<'a> = (u8, &'a str, &'a str, [(&'a str, &'a str); 8]);
            let inputs: [PortBits; 5] = [
                (
                    0x60,
                    "Hook, line, ring, keypad and coins",
                    "The main status port, read many times a second by the main loop.",
                    [
                        ("b0 hook (1 = on hook)", "The hook switch: 1 while the handset is on the cradle. Lifting it powers the board up."),
                        ("b1 loop current", "The line feeds current while the phone is off hook. If it stays away the firmware takes the call as ended by the far end."),
                        ("b2 loop current reversed", "The line reversed polarity: the far end answered. Only acted on when answer supervision is on."),
                        ("b3 ring detect", "Ringing voltage on the line. It wakes the sleeping board and is counted to answer calls."),
                        ("b4 DTMF valid", "The keypad's DTMF receiver has a digit ready; its code is on port 40 bits 0-3."),
                        ("b5 nickel", "Coin switch: one pulse per nickel."),
                        ("b6 dime", "Coin switch: one pulse per dime."),
                        ("b7 quarter", "Coin switch: one pulse per quarter."),
                    ],
                ),
                (
                    0x61,
                    "Relay feedback, housing, line energy",
                    "Status of the coin housing and the line circuits.",
                    [
                        ("b0 relay feedback", "The line sequencer waits for this after operating the line relays (up to about 5 s)."),
                        ("b1", "Never read by the firmware."),
                        ("b2", "Never read by the firmware."),
                        ("b3", "Never read by the firmware."),
                        ("b4 housing closed", "Coin housing / door switch. Opening it is reported as an alarm."),
                        ("b5", "Never read by the firmware."),
                        ("b6 line energy detect", "Call-progress energy detector: counted while the phone listens for an answer or voice on the line."),
                        ("b7", "Never read by the firmware."),
                    ],
                ),
                (
                    0x40,
                    "Keypad digit, tone detectors, straps",
                    "The low four bits are the keypad digit; the high bits are detectors and board straps.",
                    [
                        ("b0 DTMF code", "Keypad digit code, bit 0 (valid while port 60 bit 4 is set)."),
                        ("b1 DTMF code", "Keypad digit code, bit 1."),
                        ("b2 DTMF code", "Keypad digit code, bit 2."),
                        ("b3 DTMF code", "Keypad digit code, bit 3."),
                        ("b4 tone detector A", "Call-progress tone detector (busy / SIT tones on the line)."),
                        ("b5 board strap", "Board option read together with port A3 bit 7 when the phone goes idle."),
                        ("b6 board strap: line option", "Board option read at start-up that chooses a line setting."),
                        ("b7 tone detector B", "Second call-progress tone detector."),
                    ],
                ),
                (
                    0xA3,
                    "Board strap",
                    "Only bit 7 is read; the port is mostly an output (voice ROM half).",
                    [
                        ("b0", "Never read (output: voice ROM half)."),
                        ("b1", "Never read by the firmware."),
                        ("b2", "Never read by the firmware."),
                        ("b3", "Never read by the firmware."),
                        ("b4", "Never read by the firmware."),
                        ("b5", "Never read by the firmware."),
                        ("b6", "Never read by the firmware."),
                        ("b7 board strap", "Board option read when the phone goes idle."),
                    ],
                ),
                (
                    0xA5,
                    "Line interface OK",
                    "Only bit 7 is read.",
                    [
                        ("b0", "Never read by the firmware."),
                        ("b1", "Never read by the firmware."),
                        ("b2", "Never read by the firmware."),
                        ("b3", "Never read by the firmware."),
                        ("b4", "Never read by the firmware."),
                        ("b5", "Never read by the firmware."),
                        ("b6", "Never read by the firmware."),
                        ("b7 line interface OK", "1 = fine. If it reads 0 six times in a row the firmware steps through four line-interface settings on port A1 bits 0-1."),
                    ],
                ),
            ];
            for (port, title, what, bits) in inputs.iter() {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.monospace(egui::RichText::new(format!("Port {:02X}", port)).strong());
                        ui.strong(*title);
                        ui.monospace(format!("= {:02X}", p.m.b.inputs[*port as usize]));
                    });
                    ui.label(egui::RichText::new(*what).weak());
                    egui::Grid::new(("inbits", *port)).num_columns(4).spacing([18.0, 2.0]).show(ui, |ui| {
                        for row in 0..2 {
                            for col in 0..4 {
                                let b = row * 4 + col;
                                let (name, help) = bits[b];
                                let mut v = p.m.b.inputs[*port as usize] & (1 << b) != 0;
                                let never = help.starts_with("Never");
                                let text = if never { egui::RichText::new(name).weak() } else { egui::RichText::new(name) };
                                if ui.checkbox(&mut v, text).on_hover_text(help).changed() {
                                    if v {
                                        p.m.b.inputs[*port as usize] |= 1 << b;
                                    } else {
                                        p.m.b.inputs[*port as usize] &= !(1 << b);
                                    }
                                }
                            }
                            ui.end_row();
                        }
                    });
                });
            }
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.monospace(egui::RichText::new("Port 41").strong());
                    ui.strong("Line voltage");
                });
                ui.label(egui::RichText::new("An analog-to-digital reading of the line voltage: about 60 hex on hook (around 48 V), 10-18 hex off hook with loop current. The firmware checks it to tell a working line from a dead one.").weak());
                let mut v = p.m.b.inputs[0x41];
                if ui.add(egui::Slider::new(&mut v, 0..=255).hexadecimal(2, false, true).text("reading")).changed() {
                    p.m.b.inputs[0x41] = v;
                }
            });
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.monospace(egui::RichText::new("Port A6").strong());
                    ui.strong("Program (reset) button");
                });
                ui.label(egui::RichText::new("Bit 7 reads 0 while the button inside the housing is pressed. Held while the handset is lifted at power-up, it resets the phone to factory defaults.").weak());
                let mut pb = p.m.b.program_button;
                if ui.checkbox(&mut pb, "hold the program button").changed() {
                    p.m.b.program_button = pb;
                }
            });
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.monospace(egui::RichText::new("Ports C1 / C2").strong());
                    ui.strong("TDK 73K212 modem chip");
                });
                ui.label(egui::RichText::new("The firmware writes a register number to C1 and reads or writes it through C2. It dials DTMF digits with the chip's tone generator and uses it as a 300 / 1200 bit/s modem for remote programming. Register 2 (detect) reports carrier and call progress.").weak());
                let mut v = p.m.b.inputs[0xC2];
                if ui.add(egui::Slider::new(&mut v, 0..=255).hexadecimal(2, false, true).text("detect register")).changed() {
                    p.m.b.inputs[0xC2] = v;
                }
            });
            ui.add_space(6.0);
            ui.heading("Outputs the firmware drives");
            egui::Grid::new("outs").num_columns(4).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                ui.strong("Port");
                ui.strong("Bits (7 ... 0)");
                ui.strong("What it drives");
                ui.strong("Details");
                ui.end_row();
                for (port, what, details) in [
                    (0x60u8, "Control latch", "Line and hook relays; bit 3 pulses during the coin relay sequence."),
                    (0x61, "Voice codec control", "Switches the CVSD speech decoder on (92 hex while a prompt plays)."),
                    (0xA0, "Audio routing", "Chooses which audio goes to the handset and the line, and which tone detectors listen."),
                    (0xA1, "Coin relay", "Bit 3 pulses the relay; bit 2 chooses collect (1) or refund (0). Bits 0-1: line-interface setting."),
                    (0xA2, "Aux relays", "Auxiliary relays pulsed around coin and line operations."),
                    (0xA3, "Voice ROM half", "Bit 0 picks which 64 KB half of the voice ROM the CPU sees."),
                    (0xA4, "EEPROM write enable", "Bit 0 is set only while the firmware writes its settings to the EEPROM."),
                    (0xA8, "Line control", "Bit 0 puts the DTMF dialer on the line; bit 3 mutes the line while a prompt plays; bit 2 is a line option."),
                    (0x63, "Aux latch", "Cleared at start-up and never set."),
                ] {
                    let v = p.m.b.outputs[port as usize];
                    ui.monospace(format!("{:02X}", port));
                    ui.horizontal(|ui| {
                        for b in (0..8).rev() {
                            let on = v & (1 << b) != 0;
                            let (r, _) = ui.allocate_exact_size(Vec2::new(14.0, 14.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 5.0, if on { Color32::from_rgb(255, 80, 60) } else { Color32::from_gray(60) });
                        }
                        ui.monospace(format!("= {:02X}", v));
                    });
                    ui.label(what);
                    ui.label(egui::RichText::new(details).weak());
                    ui.end_row();
                }
            });
            ui.label(egui::RichText::new("Also: ports 80-8F are the MSM6242 real-time clock (time digits and control; it ticks the firmware 64 times a second and wakes the sleeping board every hour). Reading port B8 switches the board off to save power; reading B0 is a strobe whose value is ignored. Ports 00-3F are the Z180's own registers (memory mapping, timers, serial ports, DMA).").weak());
            ui.add_space(6.0);
            ui.heading("Port activity");
            egui::Grid::new("stats").num_columns(5).striped(true).show(ui, |ui| {
                ui.strong("Port");
                ui.strong("Reads");
                ui.strong("Writes");
                ui.strong("Last in / out");
                ui.strong("What it is");
                ui.end_row();
                let stats: Vec<_> = p.m.b.stats.iter().map(|(k, v)| (*k, v.reads, v.writes, v.last_read, v.last_write)).collect();
                for (k, r, w, lr, lw) in stats {
                    ui.monospace(format!("{:02X}", k));
                    ui.monospace(r.to_string());
                    ui.monospace(w.to_string());
                    ui.monospace(format!("{:02X} / {:02X}", lr, lw));
                    ui.label(egui::RichText::new(port_meaning(k as u8)).weak());
                    ui.end_row();
                }
            });
        });
    }

    fn tab_firmware(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().id_salt("fwscroll").auto_shrink([false, false]).show(ui, |ui| {
            self.firmware_page(ui);
        });
    }

    fn firmware_page(&mut self, ui: &mut egui::Ui) {
        let mut p = self.phone.lock().unwrap();
        ui.heading("Firmware (program ROM)");
        ui.label(egui::RichText::new("The program ROM the emulated phone runs. Dumps in the rom folder next to the program are listed; each firmware keeps its own battery memory and EEPROM.").weak());
        // ---- program ROM ----
        let cur = p.firmware_version();
        let cur_file = self.current_rom_file.clone();
        ui.horizontal(|ui| {
            ui.label("Program ROM:");
            let mut pick: Option<usize> = None;
            egui::ComboBox::from_id_salt("romsel").selected_text(format!("{}  ({})", cur, cur_file)).width(420.0).show_ui(ui, |ui| {
                for (i, (path, ver)) in self.roms.iter().enumerate() {
                    let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    let label = format!("{}  ({}){}", ver, name, if ver.starts_with("PCM 05.09") { " - custom, built-in rates" } else { "" });
                    if ui.selectable_label(name == cur_file, label).clicked() && name != cur_file {
                        pick = Some(i);
                    }
                }
            });
            if let Some(i) = pick {
                let (path, _) = self.roms[i].clone();
                let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if let Some((rom, _)) = std::fs::read(&path).ok().and_then(|d| crate::files::fit_program(d).ok()) {
                    p.swap_firmware(rom, crate::nvram_for(&name));
                    self.current_rom_file = name.clone();
                    crate::files::set_choice("firmware", &path.display().to_string());
                }
            }
            if ui.button("Open a firmware file...").on_hover_text("Run another program ROM dump (64 KB, 27C512). Each firmware keeps its own memory.").clicked()
            {
                if let Some(path) = crate::files::pick("Open a program ROM dump", "ROM dumps", &["bin", "rom", "hex"]) {
                    let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    match std::fs::read(&path).map_err(|e| e.to_string()).and_then(crate::files::fit_program) {
                        Ok((rom, warn)) => {
                            let ver = crate::phone::rom_version(&rom);
                            p.swap_firmware(rom, crate::nvram_for(&name));
                            self.current_rom_file = name.clone();
                            crate::files::set_choice("firmware", &path.display().to_string());
                            crate::files::remember_external_rom(&path);
                            if !self.roms.iter().any(|(q, _)| q == &path) {
                                self.roms.push((path.clone(), ver));
                            }
                            self.file_msg = Some(match warn {
                                Some(w) => (false, w),
                                None => (true, format!("Now running {}", name)),
                            });
                        }
                        Err(e) => self.file_msg = Some((false, e)),
                    }
                }
            }
        });
        rom_credits(ui);
        if let Some((ok, m)) = &self.file_msg {
            ui.colored_label(if *ok { Color32::LIGHT_GREEN } else { Color32::YELLOW }, m);
        }
        ui.label(format!("Running {}{}", p.firmware_version(), if p.has_presets() { " - carries its own built-in rates and options" } else { "" }));
        if p.has_presets() {
            ui.horizontal(|ui| {
                let busy = p.presets.is_some() || p.defaulting.is_some();
                if ui.add_enabled(!busy, egui::Button::new(format!("Load the ROM's built-in rates and options ({}, then 998)", p.telemetry_code()))).clicked() {
                    p.start_presets();
                }
                if busy {
                    ui.spinner();
                    ui.label(if p.defaulting.is_some() { "factory default first..." } else { "dialling the service codes..." });
                }
            });
            ui.label(egui::RichText::new("This custom firmware carries its own rates (every area code 50c for 3 minutes, 800 numbers free, 7-digit calls off). They are installed automatically the first time it starts.").weak());
        }
        // ---- the phone's current options ----
        ui.add_space(8.0);
        ui.heading("The phone's current options");
        ui.label(egui::RichText::new("Read live from the phone's battery memory (the options area C000-CFFF that PNM Plus programs). Meanings come from the firmware and the PNM Plus software; anything not yet understood is shown as raw values.").weak());
        let r = &p.m.b.ram;
        let programmed = &r[0x1000..0x100F] == b"RATES FOR PCM 5";
        let factory_rates = r[0xE013] & 0x04 != 0;
        let pw: String = p.pnm_password().iter().map(|&b| if b < 10 { (b'0' + b) as char } else { b as char }).collect();
        let n = p.phone_number();
        let number = if n.chars().all(|c| c.is_ascii_digit()) && n.chars().any(|c| c != '0') {
            format!("{}-{}-{}", &n[..3], &n[3..6], &n[6..])
        } else {
            "not set".into()
        };
        let yes_no = |b: bool| if b { "yes" } else { "no" };
        egui::Grid::new("fwopts").num_columns(2).striped(true).spacing([24.0, 4.0]).show(ui, |ui| {
            ui.label("Options programmed");
            ui.label(if programmed { "yes (options signature present at C000)" } else { "no: factory-default options" });
            ui.end_row();
            ui.label("Factory-default rates in use");
            ui.label(yes_no(factory_rates));
            ui.end_row();
            ui.label("Station ID (PNM logon)");
            ui.monospace(p.station_id());
            ui.end_row();
            ui.label("PNM password");
            ui.monospace(if pw.is_empty() { "-".to_string() } else { pw });
            ui.end_row();
            ui.label("Telemetry bypass code");
            ui.monospace(p.telemetry_code());
            ui.end_row();
            ui.label("Payphone number");
            ui.monospace(number);
            ui.end_row();
            ui.label("Out-of-state 1+ coin calls (C242 bit 2)");
            ui.label(if p.out_of_state_blocked() { "refused (\"invalid number\")" } else { "allowed" });
            ui.end_row();
            ui.label("Answer supervision: refund unanswered calls (C243 bit 1)");
            ui.label(if p.answer_supervision() { "on" } else { "off" });
            ui.end_row();
        });
        ui.label(egui::RichText::new("The two switches above, the station ID and the payphone number can be changed in the Service tab.").weak());

        ui.add_space(6.0);
        egui::CollapsingHeader::new("Call routing by call type (C250 + type)").default_open(true).show(ui, |ui| {
            ui.label(egui::RichText::new("How the firmware handles each kind of call, by the call type numbers PNM Plus uses. 0 means the call type is refused; other values pick how it is routed. Spare types are left out.").weak());
            egui::Grid::new("fwroutes").num_columns(6).striped(true).spacing([14.0, 2.0]).show(ui, |ui| {
                let shown: Vec<(usize, &str)> = CALL_TYPES.iter().enumerate().filter(|(_, n)| !n.contains("Spare")).map(|(i, n)| (i, *n)).collect();
                let half = shown.len().div_ceil(2);
                for row in 0..half {
                    for col in 0..2 {
                        if let Some((t, name)) = shown.get(row + col * half) {
                            let v = r[0x1250 + t];
                            ui.monospace(format!("{:2}", t));
                            ui.label(*name);
                            if v == 0 {
                                ui.label(egui::RichText::new("refused").weak());
                            } else {
                                ui.monospace(format!("route {}", v));
                            }
                        }
                    }
                    ui.end_row();
                }
            });
        });
        egui::CollapsingHeader::new("Option switch bytes (C240-C24F)").default_open(false).show(ui, |ui| {
            ui.label(egui::RichText::new("Each byte holds eight on/off options set by PNM Plus. Known bits: C242 bit 2 refuses out-of-state coin calls; C243 bit 1 turns on answer supervision.").weak());
            egui::Grid::new("fwbits").num_columns(3).striped(true).spacing([18.0, 2.0]).show(ui, |ui| {
                for a in 0x40..0x50usize {
                    let v = r[0x1200 + a];
                    ui.monospace(format!("C2{:02X}", a));
                    ui.monospace(format!("{:02X}", v));
                    ui.monospace(format!("{:08b}", v));
                    ui.end_row();
                }
            });
        });
        egui::CollapsingHeader::new("Raw options memory (C000-C2FF)").default_open(false).show(ui, |ui| {
            for row in 0..0x30usize {
                let a = 0x1000 + row * 16;
                let hex: Vec<String> = r[a..a + 16].iter().map(|b| format!("{:02X}", b)).collect();
                ui.monospace(format!("C{:03X}  {}", row * 16, hex.join(" ")));
            }
        });
    }

    fn tab_voice(&mut self, ui: &mut egui::Ui) {
        ui.heading("Voice ROM prompts");
        ui.label("The voice ROM holds one-bit CVSD speech in two pages. The firmware picks prompts by number and streams them to the codec; here you can play any of them.");
        rom_credits(ui);
        {
            let mut p = self.phone.lock().unwrap();
            // ---- voice ROM ----
            ui.horizontal(|ui| {
                ui.label("Voice ROM:");
                let short =
                    std::path::Path::new(&self.voice_name).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| self.voice_name.clone());
                ui.monospace(short).on_hover_text(&self.voice_name);
                if ui.button("Open a voice ROM file...").on_hover_text("Use another voice ROM dump (up to 128 KB, 27C010) for the spoken prompts.").clicked() {
                    if let Some(path) = crate::files::pick("Open a voice ROM dump", "ROM dumps", &["bin", "rom"]) {
                        match std::fs::read(&path).map_err(|e| e.to_string()).and_then(crate::files::fit_voice) {
                            Ok(v) => {
                                p.swap_voice(v.clone());
                                self.voice_index = vec![cvsd::voice_index(&v, 0), cvsd::voice_index(&v, 1)];
                                self.voice_rom = v;
                                self.voice_name = path.display().to_string();
                                crate::files::set_choice("voice", &self.voice_name);
                                self.file_msg = Some((true, "Voice ROM loaded: the next prompts use it".into()));
                            }
                            Err(e) => self.file_msg = Some((false, e)),
                        }
                    }
                }
                if crate::files::choice("voice").is_some() && ui.button("Use the voice folder's ROM").clicked() {
                    match crate::folder_voice().and_then(|f| std::fs::read(&f).ok().and_then(|d| crate::files::fit_voice(d).ok()).map(|v| (f, v))) {
                        Some((f, v)) => {
                            p.swap_voice(v.clone());
                            self.voice_index = vec![cvsd::voice_index(&v, 0), cvsd::voice_index(&v, 1)];
                            self.voice_rom = v;
                            self.voice_name = f.display().to_string();
                            crate::files::set_choice("voice", "");
                            self.file_msg = Some((true, "Back to the voice ROM in the voice folder".into()));
                        }
                        None => self.file_msg = Some((false, "There's no voice ROM dump in the voice folder next to the program".into())),
                    }
                }
            });
        }
        if let Some((ok, m)) = &self.file_msg {
            ui.colored_label(if *ok { Color32::LIGHT_GREEN } else { Color32::YELLOW }, m);
        }
        ui.add_space(4.0);
        // the two pages side by side, each scrolling on its own
        let mut play: Option<(usize, usize)> = None;
        let index = &self.voice_index;
        ui.columns(2, |cols| {
            for (page, ui) in cols.iter_mut().enumerate() {
                let idx = &index[page];
                let used = idx.iter().filter(|(_, len)| *len > 1).count();
                ui.strong(format!("Page {}  ({} prompts)", page, used));
                ui.separator();
                egui::ScrollArea::vertical().id_salt(("vpage", page)).auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new(("vgrid", page)).num_columns(4).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                        ui.strong("Prompt");
                        ui.strong("Address");
                        ui.strong("Length");
                        ui.label("");
                        ui.end_row();
                        for (i, (start, len)) in idx.iter().enumerate() {
                            ui.monospace(format!("{:02X}", i));
                            ui.monospace(format!("{:05X}", start));
                            ui.monospace(format!("{:.2} s", *len as f32 * 8.0 / crate::board::VOICE_BIT_RATE as f32));
                            if *len > 1 {
                                if ui.small_button("Play").clicked() {
                                    play = Some((*start, *len));
                                }
                            } else {
                                ui.label("-");
                            }
                            ui.end_row();
                        }
                    });
                });
            }
        });
        if let Some((start, len)) = play {
            let end = (start + len).min(self.voice_rom.len());
            let pcm = cvsd::decode_bytes(&self.voice_rom[start..end]);
            let mut a = self.audio.lock().unwrap();
            a.preview.clear();
            a.preview.extend(pcm);
        }
    }

    fn rates_key(&self) -> Option<String> {
        self.rates.as_ref().map(|r| format!("eeprom {} {}", r.id, r.raw.len()))
    }

    /// The rate file as it should be in the phone (with unlisted area codes
    /// opened when that's on), and a key naming that version.
    fn rates_to_send(&self) -> Option<(String, Vec<u8>)> {
        let r = self.rates.as_ref()?;
        let base = format!("eeprom {} {}", r.id, r.raw.len());
        if self.settings.open_unlisted {
            Some((format!("{} open", base), r.with_unlisted_open()))
        } else {
            Some((base, r.raw.clone()))
        }
    }

    /// Download the person's rate file once (and note when it's done).
    fn rate_import_housekeeping(&mut self) {
        if self.rates_key().is_none() {
            return;
        }
        let p = self.phone.lock().unwrap();
        if p.pnm_result.is_some() && !p.importing() {
            if let Some(k) = self.sending_key.take() {
                if p.pnm_result == Some(true) {
                    self.settings.rates_loaded = k;
                }
            }
        }
        self.auto_import_done = true;
    }

    fn tab_rates(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().id_salt("ratesscroll").auto_shrink([false, false]).show(ui, |ui| {
            self.rates_page(ui);
        });
    }

    fn rates_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Rate file");

        // ---- 1. which file ----
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            match &self.rates {
                Some(r) => {
                    let name = std::path::Path::new(&r.path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| r.path.clone());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(format!("{}  {}", r.id, r.description.trim())).strong());
                        ui.label(egui::RichText::new(&name).monospace().weak()).on_hover_text(&r.path);
                    });
                }
                None => {
                    ui.label(
                        egui::RichText::new("No rate file. Put a PNM rate file (.R94) in the rates folder next to the program, or open one.")
                            .color(Color32::YELLOW),
                    );
                }
            }
            ui.horizontal(|ui| {
                if ui.button("Open another rate file...").on_hover_text("A PNM rate file (.R94).").clicked() {
                    if let Some(path) = crate::files::pick("Open a PNM rate file", "PNM rate files", &["r94", "R94"]) {
                        match RateFile::load(&path).filter(|r| !r.bands().is_empty()) {
                            Some(r) => {
                                crate::files::set_choice("rates", &path.display().to_string());
                                self.rates = Some(r);
                                self.file_msg = Some((true, "Rate file opened: load it into the phone below".into()));
                            }
                            None => self.file_msg = Some((false, "That file isn't a PNM rate file the emulator can read".into())),
                        }
                    }
                }
                if crate::files::choice("rates").is_some() && ui.button("Use the rates folder's file").clicked() {
                    crate::files::set_choice("rates", "");
                    self.rates = crate::folder_rate_file().and_then(|p| RateFile::load(&p));
                    self.file_msg = Some(if self.rates.is_some() {
                        (true, "Back to the rate file in the rates folder".into())
                    } else {
                        (false, "There's no .R94 rate file in the rates folder next to the program".into())
                    });
                }
                if let Some((ok, m)) = &self.file_msg {
                    ui.colored_label(if *ok { Color32::LIGHT_GREEN } else { Color32::YELLOW }, m);
                }
            });
        });
        let Some(r) = &self.rates else { return };
        let base = format!("eeprom {} {}", r.id, r.raw.len());
        let f_bands = r.bands();
        let f_counts = r.npa_counts();
        let f_ld = r.long_distance_bands();
        let (r_path, r_id, r_desc, r_raw_len, r_expanded) = (r.path.clone(), r.id.clone(), r.description.clone(), r.raw.len(), r.expanded.clone());
        let in_phone = crate::ratefile::RateFile::from_phone_ram(&self.phone.lock().unwrap().m.b.ram);

        // ---- 2. loading it into the phone ----
        ui.add_space(6.0);
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.strong("Load into the phone");
            let Some((want_key, want_raw)) = self.rates_to_send() else { return };
            let (importing, status, result, on_hook) = {
                let p = self.phone.lock().unwrap();
                (p.importing(), p.pnm_status.clone(), p.pnm_result, p.m.b.on_hook())
            };
            let loaded = self.settings.rates_loaded == want_key;
            ui.label(if loaded {
                egui::RichText::new("This rate file is loaded in the phone.").color(Color32::LIGHT_GREEN)
            } else if self.settings.rates_loaded.starts_with(&base) {
                egui::RichText::new("The phone has this rate file, but not with the setting below: load it again to apply it.").color(Color32::YELLOW)
            } else {
                egui::RichText::new("This rate file is not loaded in the phone yet.").color(Color32::YELLOW)
            });
            ui.checkbox(&mut self.settings.open_unlisted, "Allow area codes the file doesn't list, priced as ordinary long distance")
                .on_hover_text("Newer toll-free codes (833-888) take 800's price. 900, 976, N11, 555 and test codes stay refused.");
            ui.horizontal(|ui| {
                let label = if loaded { "Load it again" } else { "Load this rate file into the phone" };
                if ui.add_enabled(!importing, egui::Button::new(label)).clicked() {
                    self.sending_key = Some(want_key.clone());
                    self.phone.lock().unwrap().import_rates(want_raw.clone());
                }
                if importing {
                    ui.spinner();
                    ui.label(format!("{}{}", status, if on_hook { "" } else { " - hang up the handset to start" }));
                } else if result == Some(false) {
                    ui.colored_label(Color32::LIGHT_RED, status.clone());
                }
            });
            ui.label(
                egui::RichText::new(
                    "The file goes in the way PNM Plus sends it (logon with the phone's station id and password, checked blocks); the firmware stores it in its EEPROM and builds its own price tables from it.",
                )
                .weak(),
            );
        });

        // ---- 3. prices: the file next to what the phone charges ----
        ui.add_space(6.0);
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.strong("Prices");
            let p_bands = in_phone.as_ref().map(|p| p.bands()).unwrap_or_default();
            let p_counts = in_phone.as_ref().map(|p| p.npa_counts()).unwrap_or_default();
            match &in_phone {
                None => ui.label(egui::RichText::new("The phone has no rate tables in its memory yet (factory-default rates).").color(Color32::YELLOW)),
                Some(_) if p_bands == f_bands && p_counts == f_counts => {
                    ui.label(egui::RichText::new("The phone charges exactly what this file says.").color(Color32::LIGHT_GREEN))
                }
                Some(_) => ui.label(egui::RichText::new("The phone's prices differ from this file (differences in yellow).").color(Color32::YELLOW)),
            };
            egui::Grid::new("bands").num_columns(5).striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
                ui.strong("Band");
                ui.strong("Kind");
                ui.strong("In this file");
                ui.strong("In the phone now");
                ui.strong("Area codes");
                ui.end_row();
                let n = f_bands.len().max(p_bands.len());
                for i in 0..n {
                    let k = i as u8 + 1;
                    let fb = f_bands.get(i);
                    let pb = p_bands.get(i);
                    ui.monospace(format!("{}", k));
                    let kind = match fb.or(pb) {
                        Some(b) if f_ld.contains(&k) => {
                            let _ = b;
                            "long distance"
                        }
                        Some(b) if b.first_units == 0 && b.add_units == 0 => "free",
                        Some(_) => "local",
                        None => "",
                    };
                    ui.label(kind);
                    ui.label(fb.map(|b| b.describe()).unwrap_or_else(|| "-".into()));
                    let differs = in_phone.is_some() && fb != pb;
                    let text = pb.map(|b| b.describe()).unwrap_or_else(|| "-".into());
                    if differs {
                        ui.colored_label(Color32::YELLOW, text);
                    } else {
                        ui.label(text);
                    }
                    let npas = f_counts.get(&k).map(|v| v.as_slice()).unwrap_or(&[]);
                    ui.label(npa_summary(npas));
                    ui.end_row();
                }
            });
            let count = |c: &std::collections::BTreeMap<u8, Vec<u16>>, k: u8| c.get(&k).map(|v| v.len()).unwrap_or(0);
            let mut line = format!("File: {} area codes refused, {} priced exchange by exchange (your own area).", count(&f_counts, 0), count(&f_counts, 0xFE));
            if in_phone.is_some() {
                line.push_str(&format!("  Phone: {} refused, {} by exchange.", count(&p_counts, 0), count(&p_counts, 0xFE)));
            }
            ui.label(egui::RichText::new(line).weak());
        });

        // ---- 4. the file itself ----
        ui.add_space(6.0);
        egui::CollapsingHeader::new("File details").default_open(false).show(ui, |ui| {
            egui::Grid::new("rgrid").num_columns(2).show(ui, |ui| {
                ui.label("File");
                ui.monospace(&r_path);
                ui.end_row();
                ui.label("Rate file id");
                ui.monospace(&r_id);
                ui.end_row();
                ui.label("Description");
                ui.monospace(&r_desc);
                ui.end_row();
                ui.label("Size (stored / expanded)");
                ui.monospace(format!("{} / {} bytes", r_raw_len, r_expanded.len()));
                ui.end_row();
            });
            ui.label(egui::RichText::new("A new phone also needs a PNM options file. Without one the phone keeps its factory-default options, with long-distance calls routed like local ones so every number the rate file prices can be dialled.").weak());
        });
        egui::CollapsingHeader::new("File contents (hex, zero runs restored)").default_open(false).show(ui, |ui| {
            for (i, chunk) in r_expanded.chunks(16).enumerate().take(400) {
                let hex: Vec<String> = chunk.iter().map(|b| format!("{:02X}", b)).collect();
                ui.monospace(format!("{:04X}  {}", i * 16, hex.join(" ")));
            }
        });
    }

    fn tab_cpu(&mut self, ui: &mut egui::Ui) {
        let mut p = self.phone.lock().unwrap();
        ui.horizontal(|ui| {
            if ui.button(if p.paused { "Run" } else { "Pause" }).clicked() {
                p.paused = !p.paused;
            }
            if p.paused && ui.button("Step one instruction").clicked() {
                p.m.run(1);
            }
            ui.label("Speed");
            ui.add(egui::Slider::new(&mut p.speed, 0.1..=4.0));
            ui.checkbox(&mut self.dis_follow, "follow PC");
            ui.label(format!("emulator load {:.0}%", p.cpu_load * 100.0));
        });
        let c = p.m.z.cpu.clone();
        let io = p.m.z.io.clone();
        ui.monospace(format!(
            "PC={:04X} SP={:04X} AF={:04X} BC={:04X} DE={:04X} HL={:04X} IX={:04X} IY={:04X}  I={:02X} IM{} IFF1={} {}",
            c.pc,
            c.sp,
            c.af(),
            c.bc(),
            c.de(),
            c.hl(),
            c.ix,
            c.iy,
            c.i,
            c.im,
            c.iff1 as u8,
            if p.m.asleep {
                "BOARD OFF"
            } else if c.sleeping {
                "SLP"
            } else if c.halted {
                "HALT"
            } else {
                ""
            }
        ));
        ui.monospace(format!(
            "MMU CBAR={:02X} CBR={:02X} BBR={:02X}  ITC={:02X} IL={:02X}  TCR={:02X} TMDR0={:04X}/{:04X} TMDR1={:04X}/{:04X}  DSTAT={:02X} MAR1={:05X} BCR1={:04X}",
            io.cbar, io.cbr, io.bbr, io.itc, io.il, io.tcr, io.tmdr[0], io.rldr[0], io.tmdr[1], io.rldr[1], io.dstat, io.mar1, io.bcr1
        ));
        ui.monospace(format!("emulated time {:.2} s, board power-downs {}, wake-ups {}", p.m.seconds(), p.m.sleep_count, p.m.wake_count));
        ui.horizontal_wrapped(|ui| {
            for (i, n) in p.m.z.irq_count.iter().enumerate() {
                if *n > 0 {
                    ui.monospace(format!("{}: {}", z180::IRQ_NAMES[i], n));
                }
            }
        });
        ui.separator();
        // disassembly around PC
        let pc = c.pc;
        let m = &mut p.m;
        let mut lines = Vec::new();
        let mut a = pc;
        for _ in 0..28 {
            let bytes: Vec<u8> = (0..4).map(|i| m.z.read_logical(&mut m.b, a.wrapping_add(i))).collect();
            let rd = |x: u16| bytes[(x.wrapping_sub(a)) as usize % 4];
            let (t, l) = crate::disasm::disasm(&rd, a);
            lines.push((a, t));
            a = a.wrapping_add(l.max(1));
        }
        drop(p);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (addr, t) in lines {
                let s = format!("{} {:04X}  {}", if addr == pc { ">" } else { " " }, addr, t);
                if addr == pc {
                    ui.colored_label(Color32::from_rgb(255, 210, 90), egui::RichText::new(s).monospace());
                } else {
                    ui.monospace(s);
                }
            }
        });
    }
}

/// What the Coverage tab shows (computed on request).
pub struct CovView {
    at: f64,
    ran: usize,
    total: usize,
    regions: Vec<crate::coverage::Region>,
    ports: Vec<(crate::coverage::PortRow, u64, u64, &'static str)>,
    unrun_io: Vec<(u16, bool, u8)>,
    /// Branches reached but never taken: (size, start, deciding pc, context, why).
    frontier: Vec<(u16, u16, u16, String, Option<&'static str>)>,
    /// (label, bytes only read, bytes written, never touched)
    mem: Vec<(String, usize, usize, usize)>,
}

/// What each external port is, as far as the firmware shows.
fn port_meaning(p: u8) -> &'static str {
    match p {
        0x40 => "in: DTMF code b0-3, tone detectors b4/b7, straps b5/b6; out: voice codec data",
        0x41 => "in: line voltage ADC",
        0x60 => "in: hook, loop current b1/b2, ring, DTMF valid, coins; out: control latch",
        0x61 => "in: relay feedback b0, housing b4, line energy b6; out: voice codec control",
        0x63 => "out: aux latch (cleared at start)",
        0x80..=0x8F => "real-time clock (MSM6242)",
        0xA0 => "out: audio routing / tone detector select",
        0xA1 => "out: coin relay b2/b3, line interface setting b0-1",
        0xA2 => "out: aux relays",
        0xA3 => "out: voice ROM half b0; in: strap b7",
        0xA4 => "out: EEPROM write enable",
        0xA5 => "in: line interface OK b7",
        0xA6 => "in: program button b7",
        0xA8 => "out: line (dialer b0, line option b2, mute b3)",
        0xB0 => "in: strobe (value ignored)",
        0xB8 => "in: power the board off",
        0xC1 | 0xC2 => "TDK 73K212 modem (address / data)",
        _ => UNUSED_PORT,
    }
}

const UNUSED_PORT: &str = "probably data read as code (no sign it is used)";

impl App {
    fn analyse_coverage(&mut self) {
        let p = self.phone.lock().unwrap();
        let Some(hit) = p.m.pc_hist.clone() else { return };
        let rom = p.m.b.rom.clone();
        let vb = ((p.m.z.cpu.i as u16) << 8) | (p.m.z.io.il as u16 & 0xE0);
        let stats: Vec<(u8, u64, u64)> = p.m.b.stats.iter().map(|(k, v)| (*k as u8, v.reads, v.writes)).collect();
        let seen = p.m.b.mem_seen.clone();
        let at = p.m.seconds();
        drop(p);
        let mut roots = crate::coverage::roots(&rom, Some(vb));
        roots.extend((0..65536usize).filter(|a| hit[*a] > 0).map(|a| a as u16));
        let sc = crate::coverage::analyse(&rom, &roots);
        let (ran, total) = crate::coverage::percent(&sc, &hit);
        let all_regions = crate::coverage::unvisited(&sc, &hit);
        let frontier = crate::coverage::frontier(&sc, &all_regions, &rom, &hit, 12)
            .into_iter()
            .take(80)
            .map(|(size, start, at)| (size, start, at, crate::coverage::context(&sc, &rom, at), crate::coverage::why_unrun(start)))
            .collect();
        let regions: Vec<_> = all_regions.into_iter().take(80).collect();
        let ports = crate::coverage::port_map(&sc, &hit)
            .into_iter()
            .filter(|r| !r.internal && port_meaning(r.port) != UNUSED_PORT)
            .map(|r| {
                let (rd, wr) = stats.iter().find(|s| s.0 == r.port).map(|s| (s.1, s.2)).unwrap_or((0, 0));
                let m = port_meaning(r.port);
                (r, rd, wr, m)
            })
            .collect();
        let unrun_io = sc
            .io
            .iter()
            .filter(|s| !s.internal && hit[s.pc as usize] == 0)
            .filter_map(|s| s.port.map(|p| (s.pc, s.write, p)))
            .filter(|(_, _, p)| port_meaning(*p) != UNUSED_PORT)
            .collect();
        let mut mem = Vec::new();
        if let Some(m) = seen {
            let mut block = |label: String, base: usize| {
                let s = &m[base..base + 0x1000];
                let w = s.iter().filter(|x| **x & 2 != 0).count();
                let r = s.iter().filter(|x| **x == 1).count();
                let none = s.iter().filter(|x| **x == 0).count();
                mem.push((label, r, w, none));
            };
            for k in 0..8usize {
                block(format!("RAM (low) {:05X}", 0x20000 + k * 0x1000), 0x20000 + k * 0x1000);
            }
            for k in 0..2usize {
                block(format!("EEPROM +{:04X}", k * 0x1000), 0x28000 + k * 0x1000);
            }
            for k in 0..16usize {
                let logical = match k {
                    0..=2 => format!(" = logical {:04X}", 0xB000 + k * 0x1000),
                    14..=15 => format!(" = logical {:04X}", k * 0x1000),
                    _ => String::new(),
                };
                block(format!("battery RAM {:05X}{}", 0x30000 + k * 0x1000, logical), 0x30000 + k * 0x1000);
            }
        }
        self.cov = Some(CovView { at, ran, total, regions, ports, unrun_io, frontier, mem });
    }

    fn tab_coverage(&mut self, ui: &mut egui::Ui) {
        ui.heading("What the emulator has exercised");
        ui.label("Every instruction the CPU runs and every memory byte it reads or writes is counted from start-up. Analysing follows the program's code from its entry points, interrupt vectors and jump tables, then lists what has never run and how the code uses each I/O port. Use the phone (calls, coins, telemetry codes, ringing) and analyse again to see it grow.");
        // refreshed every few seconds while the tab is open
        let stale = self.cov_time.map(|t| t.elapsed().as_secs_f32() > 5.0).unwrap_or(true);
        if ui.button("Analyse now").clicked() || self.cov.is_none() || stale {
            self.analyse_coverage();
            self.cov_time = Some(std::time::Instant::now());
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        let Some(c) = &self.cov else {
            ui.label("Coverage tracking is off.");
            return;
        };
        ui.label(format!(
            "At {:.0} s: {} of {} instructions found in the program ROM have run ({:.1}%). Code never reached is often error handling, options this phone doesn't use, or data the analysis read as code.",
            c.at,
            c.ran,
            c.total,
            c.ran as f64 * 100.0 / c.total.max(1) as f64
        ));
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::CollapsingHeader::new("I/O ports used by the code").default_open(false).show(ui, |ui| {
                egui::Grid::new("covports").num_columns(5).striped(true).show(ui, |ui| {
                    ui.strong("port");
                    ui.strong("read sites run");
                    ui.strong("write sites run");
                    ui.strong("reads / writes so far");
                    ui.strong("what it is");
                    ui.end_row();
                    for (r, rd, wr, m) in &c.ports {
                        ui.monospace(format!("{:02X}", r.port));
                        ui.monospace(format!("{} of {}", r.read_sites_run, r.read_sites));
                        ui.monospace(format!("{} of {}", r.write_sites_run, r.write_sites));
                        ui.monospace(format!("{} / {}", rd, wr));
                        ui.label(*m);
                        ui.end_row();
                    }
                });
                ui.label(egui::RichText::new("Internal Z180 registers (ports 00-3F) are left out; the CPU core handles them.").weak());
            });
            egui::CollapsingHeader::new(format!("Branches the firmware reached but never took ({})", c.frontier.len())).default_open(true).show(ui, |ui| {
                ui.label(egui::RichText::new("Each line is unrun code right next to code that ran: the instruction that decided against it, and why, where that is known. Unexplained ones are worth trying from the phone (other numbers, codes, settings).").weak());
                egui::Grid::new("covfrontier").num_columns(3).striped(true).show(ui, |ui| {
                    ui.strong("unrun code");
                    ui.strong("decided at");
                    ui.strong("why");
                    ui.end_row();
                    for (size, start, _, ctx, why) in &c.frontier {
                        ui.monospace(format!("{:04X} ({} bytes)", start, size));
                        ui.monospace(ctx);
                        match why {
                            Some(w) => ui.label(*w),
                            None => ui.label(egui::RichText::new("not known yet").weak()),
                        };
                        ui.end_row();
                    }
                });
            });
            egui::CollapsingHeader::new(format!("Port accesses never run ({})", c.unrun_io.len())).default_open(true).show(ui, |ui| {
                for (pc, w, port) in &c.unrun_io {
                    ui.monospace(format!("{:04X}  {} {:02X}   {}", pc, if *w { "out" } else { "in " }, port, port_meaning(*port)));
                }
            });
            egui::CollapsingHeader::new(format!("Code never run (largest {} pieces)", c.regions.len())).default_open(false).show(ui, |ui| {
                egui::Grid::new("covregions").num_columns(4).striped(true).show(ui, |ui| {
                    ui.strong("addresses");
                    ui.strong("bytes");
                    ui.strong("reached from");
                    ui.strong("ports");
                    ui.end_row();
                    for g in &c.regions {
                        ui.monospace(format!("{:04X}-{:04X}", g.start, g.end.wrapping_sub(1)));
                        ui.monospace(format!("{}", g.end.wrapping_sub(g.start)));
                        let callers: Vec<String> = g.callers.iter().take(5).map(|a| format!("{:04X}", a)).collect();
                        ui.monospace(if callers.is_empty() { "-".to_string() } else { callers.join(" ") });
                        let ports: Vec<String> = g.ports.iter().map(|(p, w)| format!("{} {:02X}", if *w { "out" } else { "in" }, p)).collect();
                        ui.monospace(ports.join(", "));
                        ui.end_row();
                    }
                });
            });
            egui::CollapsingHeader::new("Memory touched").default_open(false).show(ui, |ui| {
                egui::Grid::new("covmem").num_columns(4).striped(true).show(ui, |ui| {
                    ui.strong("4 KB block");
                    ui.strong("written");
                    ui.strong("only read");
                    ui.strong("never touched");
                    ui.end_row();
                    for (label, r, w, none) in &c.mem {
                        ui.monospace(label);
                        ui.monospace(w.to_string());
                        ui.monospace(r.to_string());
                        ui.monospace(none.to_string());
                        ui.end_row();
                    }
                });
            });
        });
    }
}

/// Areas of the map with what the firmware keeps there (physical address,
/// length, name), with the usual MMU set-up (bank B000-DFFF = 0x30000,
/// common E000-FFFF = 0x3E000).
const MEM_LABELS: &[(u32, u32, &str)] = &[
    (0x20000, 0x8000, "program-upgrade area (checked at start for a newer image; empty)"),
    (0x200B6, 8, "upgrade image signature (8 bytes summing to 34h)"),
    (0x28000, 0x2000, "EEPROM: stored copy of options and rates (written by PNM command 29h)"),
    (0x30000, 0x400, "battery RAM, bank B000: working variables"),
    (0x30400, 0x960, "dialling patterns (B400-BD5F, in the options checksum)"),
    (0x30FFE, 2, "combined options checksum (BFFE)"),
    (0x31000, 0x20, "options signature \"RATES FOR PCM 5...\" (C000)"),
    (0x310D5, 6, "station id: length + digits (C0D5)"),
    (0x310DB, 10, "telemetry password: length, digits, sum (C0DB)"),
    (0x31250, 0x10, "call-type routing (C250 + type)"),
    (0x31000, 0x1000, "PNM options (C000-CFFF)"),
    (0x31FFE, 2, "options checksum (CFFE)"),
    (0x32000, 0x320, "rates: price band of each area code 200-999 (D000)"),
    (0x32360, 0x8, "rates: band counts per call category"),
    (0x32000, 0x1000, "rate tables (D000-DFFF): area codes, categories, price bands"),
    (0x33000, 0x1000, "rate tables: exchange (NXX) tables, reached by bank switching"),
    (0x3E001, 1, "busy flags (E001): the board powers off when all clear"),
    (0x3E002, 1, "line latch shadow (port A8)"),
    (0x3E009, 1, "control latch shadow (port 60)"),
    (0x3E00A, 1, "audio routing shadow (port A0)"),
    (0x3E00B, 1, "coin relay shadow (port A1)"),
    (0x3E00D, 1, "voice ROM half shadow (port A3)"),
    (0x3E013, 1, "mode flags (bit 2 = factory-default rates)"),
    (0x3E000, 0x100, "system flags and port shadows (E000)"),
    (0x3F093, 1, "minutes into the call"),
    (0x3F09B, 1, "minutes left in the paid period (FE = unlimited)"),
    (0x3F0DB, 0x10, "number being called (digits)"),
    (0x3F0E8, 2, "last call's length in seconds"),
    (0x3F10F, 2, "seconds into the call"),
    (0x3F111, 1, "second within the minute"),
    (0x3F1AA, 0x38, "28 software timers (F1AA-F1E1), counted down at 64 Hz"),
    (0x3F1CC, 2, "timer: line sequencer"),
    (0x3F1CE, 2, "timer: back-to-idle cleanup"),
    (0x3F20A, 1, "line sequencer step"),
    (0x3F217, 1, "last port 60 reading (hook, loop current, coins)"),
    (0x3FB00, 2, "warm-start signature (AA)"),
    (0x3FB00, 0x152, "stack (grows down from FC52)"),
    (0x3E000, 0x2000, "firmware variables (E000-FFFF)"),
];

/// The narrowest label covering a physical address.
fn mem_label(phys: u32) -> Option<&'static str> {
    MEM_LABELS.iter().filter(|(a, n, _)| phys >= *a && phys < a + n).min_by_key(|(_, n, _)| *n).map(|(_, _, s)| *s)
}

/// Regions the map can show: name, physical base, length, pixels per row.
const MEM_REGIONS: [(&str, u32, u32, usize); 3] = [
    ("Battery RAM (64 KB, 30000-3FFFF)", 0x30000, 0x10000, 256),
    ("EEPROM (8 KB, 28000-29FFF)", 0x28000, 0x2000, 128),
    ("Low RAM / upgrade area (32 KB, 20000-27FFF)", 0x20000, 0x8000, 256),
];

impl App {
    fn tab_memory(&mut self, ui: &mut egui::Ui) {
        // the whole page scrolls (the map, the hex view and the list of areas)
        egui::ScrollArea::vertical().id_salt("memscroll").auto_shrink([false, false]).show(ui, |ui| {
            self.tab_memory_page(ui);
        });
    }

    fn tab_memory_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Memory map");
        ui.label("Every byte of RAM and EEPROM, one dot each. Bright orange: written in the last moments; bright cyan: read recently; they fade as time passes. Dim green: written at some time; dim blue: only ever read; dark: never touched. Hover for the address and what the firmware keeps there; click to watch it in the hex view.");
        let (region, window) = (self.mem_region, self.mem_window);
        ui.horizontal(|ui| {
            for (i, (name, _, _, _)) in MEM_REGIONS.iter().enumerate() {
                if ui.selectable_label(self.mem_region == i, *name).clicked() {
                    self.mem_region = i;
                    self.mem_sel = None;
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Recent means the last");
            ui.add(egui::Slider::new(&mut self.mem_window, 0.2..=60.0).logarithmic(true).suffix(" s"));
        });
        let (_, base, len, width) = MEM_REGIONS[region];
        let height = len as usize / width;
        let p = self.phone.lock().unwrap();
        let Some(age) = p.m.b.mem_age.as_ref() else {
            ui.label("Memory tracking is off.");
            return;
        };
        let now = crate::board::mem_tick(p.m.b.cycles);
        let ticks_per_s = crate::board::CPU_HZ as f32 / 16384.0;
        let win = (window * ticks_per_s).max(1.0);
        let off = (base - crate::board::MEM_AGE_BASE) as usize;
        let mut img = egui::ColorImage::new([width, height], vec![Color32::BLACK; width * height]);
        let (mut used, mut recent_r, mut recent_w) = (0usize, 0usize, 0usize);
        for i in 0..len as usize {
            let r = age.rd[off + i];
            let w = age.wr[off + i];
            let c = if r == 0 && w == 0 {
                Color32::from_rgb(28, 30, 34)
            } else {
                used += 1;
                let last_w = if w > 0 { now.saturating_sub(w) as f32 } else { f32::MAX };
                let last_r = if r > 0 { now.saturating_sub(r) as f32 } else { f32::MAX };
                if last_w <= last_r && last_w < win {
                    recent_w += 1;
                    let f = 1.0 - last_w / win;
                    mix(Color32::from_rgb(50, 110, 60), Color32::from_rgb(255, 140, 40), f)
                } else if last_r < win {
                    recent_r += 1;
                    let f = 1.0 - last_r / win;
                    let dim = if w > 0 { Color32::from_rgb(50, 110, 60) } else { Color32::from_rgb(45, 65, 115) };
                    mix(dim, Color32::from_rgb(80, 230, 255), f)
                } else if w > 0 {
                    Color32::from_rgb(50, 110, 60)
                } else {
                    Color32::from_rgb(45, 65, 115)
                }
            };
            img.pixels[i] = c;
        }
        let (mmu_cbar, mmu_cbr, mmu_bbr) = (p.m.z.io.cbar, p.m.z.io.cbr, p.m.z.io.bbr);
        let value_at = |phys: u32| -> u8 {
            match phys {
                0x20000..=0x27FFF => p.m.b.ram_lo[(phys - 0x20000) as usize],
                0x28000..=0x29FFF => p.m.b.eeprom[(phys - 0x28000) as usize],
                0x30000..=0x3FFFF => p.m.b.ram[(phys - 0x30000) as usize],
                _ => 0xFF,
            }
        };
        let ago = |t: u32| -> String {
            if t == 0 {
                "never".into()
            } else {
                format!("{:.1} s ago", now.saturating_sub(t) as f32 / ticks_per_s)
            }
        };
        let logical = |phys: u32| -> Option<u16> {
            let ba = ((mmu_cbar & 0x0F) as u32) << 12;
            let ca = ((mmu_cbar >> 4) as u32) << 12;
            let l1 = phys.wrapping_sub((mmu_bbr as u32) << 12);
            let l2 = phys.wrapping_sub((mmu_cbr as u32) << 12);
            if l2 >= ca && l2 < 0x10000 {
                Some(l2 as u16)
            } else if l1 >= ba && l1 < ca {
                Some(l1 as u16)
            } else {
                None
            }
        };
        ui.label(format!("{} of {} bytes used so far; in the last {:.1} s: {} written, {} read.", used, len, window, recent_w, recent_r));
        let tex = match &mut self.mem_tex {
            Some(t) => {
                t.set(img, egui::TextureOptions::NEAREST);
                t.clone()
            }
            None => {
                let t = ui.ctx().load_texture("memmap", img, egui::TextureOptions::NEAREST);
                self.mem_tex = Some(t.clone());
                t
            }
        };
        let mut sel = self.mem_sel;
        ui.columns(2, |cols| {
            let ui = &mut cols[0];
            let scale = ((ui.available_width() / width as f32).floor()).clamp(1.0, 4.0);
            let size = Vec2::new(width as f32 * scale, height as f32 * scale);
            let resp = ui.add(egui::Image::new((tex.id(), size)).sense(Sense::click()));
            let at = |pos: Pos2| -> Option<u32> {
                let v = (pos - resp.rect.min) / scale;
                let (x, y) = (v.x as i64, v.y as i64);
                (x >= 0 && y >= 0 && (x as usize) < width && (y as usize) < height).then(|| base + (y as u32) * width as u32 + x as u32)
            };
            if let Some(a) = sel.filter(|a| *a >= base && *a < base + len) {
                let i = (a - base) as usize;
                let r = Rect::from_min_size(resp.rect.min + Vec2::new((i % width) as f32 * scale, (i / width) as f32 * scale), Vec2::splat(scale.max(3.0)));
                ui.painter().rect_stroke(r.expand(2.0), 0.0, Stroke::new(1.5f32, Color32::WHITE), StrokeKind::Outside);
            }
            if let Some(pos) = resp.hover_pos() {
                if let Some(a) = at(pos) {
                    let i = (a - crate::board::MEM_AGE_BASE) as usize;
                    let l = logical(a).map(|l| format!(" (CPU address {:04X})", l)).unwrap_or_default();
                    resp.clone().on_hover_text(format!(
                        "{:05X}{}  = {:02X}\nlast written {}\nlast read {}\n{}",
                        a,
                        l,
                        value_at(a),
                        ago(age.wr[i]),
                        ago(age.rd[i]),
                        mem_label(a).unwrap_or("")
                    ));
                }
            }
            if resp.clicked() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    sel = at(pos);
                }
            }
            let ui = &mut cols[1];
            match sel {
                None => {
                    ui.label("Click the map to watch an address here.");
                }
                Some(a) => {
                    let l = logical(a).map(|l| format!("  CPU address {:04X}", l)).unwrap_or_default();
                    ui.strong(format!("{:05X}{}", a, l));
                    ui.label(mem_label(a).unwrap_or("(no name yet)"));
                    let start = (a & !0x7F).max(crate::board::MEM_AGE_BASE);
                    egui::Grid::new("memhex").spacing(Vec2::new(4.0, 2.0)).show(ui, |ui| {
                        for row in 0..8u32 {
                            let ra = start + row * 16;
                            ui.monospace(format!("{:05X}", ra));
                            for k in 0..16u32 {
                                let b = ra + k;
                                let i = (b - crate::board::MEM_AGE_BASE) as usize;
                                if i >= crate::board::MEM_AGE_LEN {
                                    continue;
                                }
                                let (r, w) = (age.rd[i], age.wr[i]);
                                let recent = |t: u32| t > 0 && (now.saturating_sub(t) as f32) < win;
                                let color = if recent(w) {
                                    Color32::from_rgb(255, 150, 60)
                                } else if recent(r) {
                                    Color32::from_rgb(90, 230, 255)
                                } else if r == 0 && w == 0 {
                                    Color32::from_gray(90)
                                } else {
                                    Color32::from_gray(200)
                                };
                                let mut text = egui::RichText::new(format!("{:02X}", value_at(b))).monospace().color(color);
                                if b == a {
                                    text = text.underline().strong();
                                }
                                ui.label(text);
                            }
                            ui.end_row();
                        }
                    });
                    let i = (a - crate::board::MEM_AGE_BASE) as usize;
                    ui.label(format!("Last written {}, last read {}.", ago(age.wr[i]), ago(age.rd[i])));
                }
            }
        });
        self.mem_sel = sel;
        drop(p);
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        egui::CollapsingHeader::new("What the firmware keeps where").default_open(false).show(ui, |ui| {
            egui::Grid::new("memlabels").num_columns(2).striped(true).show(ui, |ui| {
                for (a, n, s) in MEM_LABELS {
                    ui.monospace(if *n > 1 { format!("{:05X}-{:05X}", a, a + n - 1) } else { format!("{:05X}", a) });
                    ui.label(*s);
                    ui.end_row();
                }
            });
        });
    }
}

fn mix(a: Color32, b: Color32, f: f32) -> Color32 {
    let f = f.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Call types as PNM Plus names them (index = the firmware's call type).
const CALL_TYPES: [&str; 76] = [
    "Restricted",
    "Free Emer(911)",
    "Spare",
    "Information",
    "Sent Paid Local",
    "SentPaid IntraLATA",
    "SentPaid InterLATA",
    "SentPaid InterState",
    "SentPaid Ext NPAs",
    "10-XXX 0+ && 0-",
    "800-NXX-XXXX",
    "900-NXX-XXXX",
    "976-XXXX",
    "950-XXXX",
    "01+ Intnl Op Assist",
    "011+SentPaid Intln",
    "10-XXX-1+",
    "Speed Dial",
    "Restricted 1+",
    "Coin-Corridor",
    "Spare",
    "Call Home (Alr/SMDR)",
    "VDC Record Dump",
    "Reroute Busy on 0+",
    "Service Desk",
    "Spare",
    "Spare",
    "Spare",
    "VMF for 0+",
    "VMF for Coin",
    "Spare",
    "Spare",
    "Spare",
    "*0- call",
    "Splash back to Opr",
    "Restricted 0+",
    "0- call",
    "00- call",
    "0+ Invalid fm key/swipe",
    "0+ Ext NPA",
    "0+ No CC Local",
    "0+ No CC IntraLata",
    "0+ No CC InterLata",
    "No CC InterState",
    "0+ BellCard Local",
    "0+ BellCard IntraLata",
    "0+ BellCard InterLata",
    "0+ BellCard InterState",
    "0+ CommCC Local",
    "0+ CommCC IntraLata",
    "0+ CommCC InterLata",
    "0+ CommCC InterState",
    "0+ Unk CC Local",
    "0+ Unk CC IntraLata",
    "0+ Unk CC InterLata",
    "0+ Unk CC InterState",
    "Spare",
    "Spare",
    "Spare",
    "PAOF Validate Request",
    "PAOF 0+ to local",
    "PAOF 0+ to 1+IntraLata",
    "PAOF 0+ to 1+InterLata",
    "PAOF 0+ to 1+InterState",
    "Spare",
    "Spare",
    "Spare",
    "Spare",
    "Spare",
    "Spare",
    "0+ Unbill from VDC",
    "0+ No CC Corridor",
    "0+ BellCard Corr",
    "CommCard Corridor",
    "Unk Card Corridor",
    "PAOF 0+ to IXC Cor",
];

/// Credit for the ROM dumps the emulator was built and tested with.
fn rom_credits(ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(egui::RichText::new("Stock firmware (PCM 05.04.04), custom firmware (PCM 05.09.00) and voice ROM dumps courtesy of").weak());
        ui.hyperlink_to("payphones.telesfor.org", "https://payphones.telesfor.org");
    });
}

/// "62 area codes: 201, 202, 203 ..." for the price table.
fn npa_summary(npas: &[u16]) -> String {
    match npas.len() {
        0 => "used for exchanges in your own area".into(),
        n => {
            let shown: Vec<String> = npas.iter().take(6).map(|n| n.to_string()).collect();
            format!("{} area code{}: {}{}", n, if n == 1 { "" } else { "s" }, shown.join(", "), if n > 6 { " ..." } else { "" })
        }
    }
}
