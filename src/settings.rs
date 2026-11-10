//! Per-installation display settings: typeface, weight and size.
//!
//! `settings.json` lives next to the executable, like `quotes.json`, and is
//! exported with the defaults on first run so the knobs are discoverable
//! without reading the source. Every field is optional and falls back to a
//! built-in default, so a partially edited file still works. A file that
//! cannot be parsed, or values the renderer cannot express, are reported to
//! the user once at startup and replaced by the defaults — the strip always
//! renders.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Where the settings live relative to the executable.
pub const FILE_NAME: &str = "settings.json";

/// Default face: Microsoft YaHei UI, the standard Simplified-Chinese UI
/// face on Windows (GDI substitutes another face when it is absent).
pub const DEFAULT_FACE: &str = "Microsoft YaHei UI";
/// Default weight: 400 (regular). Light 300 read as washed out at strip
/// size; bold 700 was heavier than the taskbar's own text.
pub const DEFAULT_WEIGHT: i32 = 400;
/// Default size in points: matches the taskbar's own text height
/// (16 px em at 96 DPI).
pub const DEFAULT_PT: i32 = 11;
/// Default text colour: `auto` follows the taskbar's light/dark theme
/// (black text on a light bar, white on a dark one). A literal `#RRGGBB`
/// or `#RGB` pins the colour instead.
pub const DEFAULT_COLOR: &str = "auto";
/// Colour used when `color` is `auto` and the taskbar is in light mode.
pub const AUTO_COLOR_LIGHT: (u8, u8, u8) = (0, 0, 0);
/// Colour used when `color` is `auto` and the taskbar is in dark mode.
pub const AUTO_COLOR_DARK: (u8, u8, u8) = (255, 255, 255);
/// Default text opacity: fully opaque, like the taskbar's text.
pub const DEFAULT_OPACITY: f64 = 1.0;
/// Default strip placement: 0 = bottom-left of the taskbar band /
/// desktop, 1 = bottom-right, ending at the notification area's left
/// edge (stepping over TrafficMonitor's taskbar window when needed).
pub const DEFAULT_POSITION: i32 = 0;
/// Default tray-icon visibility: shown. The tray menu is the app's only
/// built-in way to exit, so hiding the icon is opt-in.
pub const DEFAULT_TRAY_ICON: i32 = 1;

/// `LOGFONTW::lfFaceName` is `[u16; 32]`: 31 units plus the terminator.
const MAX_FACE_UNITS: usize = 31;
const MIN_WEIGHT: i32 = 100;
const MAX_WEIGHT: i32 = 900;
const MIN_PT: i32 = 8;
const MAX_PT: i32 = 48;

/// Display settings as written in `settings.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Font face name as GDI knows it: `Microsoft YaHei UI`, `SimHei`,
    /// `SimSun`, `KaiTi`, `DengXian`, ...
    pub font: String,
    /// Font weight: 400 = regular, 700 = bold.
    pub weight: i32,
    /// Font size in points.
    pub size: i32,
    /// Text colour: `#RGB` or `#RRGGBB`.
    pub color: String,
    /// Text opacity in `0.0..=1.0` (1.0 = fully opaque).
    pub opacity: f64,
    /// Strip placement: 0 = bottom-left, 1 = bottom-right (left of the
    /// tray, avoiding taskbar obstacles such as TrafficMonitor).
    pub position: i32,
    /// Tray-icon visibility: 1 = show the notification-area icon (and
    /// its menu: edit quotes, autostart, exit), 0 = hide it. With the
    /// icon hidden the app can only be exited via Task Manager, and the
    /// other settings take effect by editing this file and restarting.
    pub tray_icon: i32,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            font: DEFAULT_FACE.to_string(),
            weight: DEFAULT_WEIGHT,
            size: DEFAULT_PT,
            color: DEFAULT_COLOR.to_string(),
            opacity: DEFAULT_OPACITY,
            position: DEFAULT_POSITION,
            tray_icon: DEFAULT_TRAY_ICON,
        }
    }
}

impl Settings {
    /// RGB triple for the configured colour. `auto` resolves against the
    /// taskbar's theme (`light` = Windows light mode); a literal colour is
    /// returned as-is. Always valid after
    /// [`sanitized`](Settings::sanitized).
    pub fn rgb(&self, light: bool) -> (u8, u8, u8) {
        if self.color.eq_ignore_ascii_case("auto") {
            return if light {
                AUTO_COLOR_LIGHT
            } else {
                AUTO_COLOR_DARK
            };
        }
        parse_color(&self.color).unwrap_or(AUTO_COLOR_LIGHT)
    }

    /// Text opacity as the 0–255 alpha byte the blender uses.
    pub fn alpha(&self) -> u8 {
        (self.opacity.clamp(0.0, 1.0) * 255.0).round() as u8
    }

    /// Replace values the renderer cannot express, reporting what changed.
    pub fn sanitized(mut self) -> (Settings, Option<String>) {
        let mut problems: Vec<String> = Vec::new();
        let face = self.font.trim().to_string();
        if face.is_empty() {
            problems.push("font 不能为空".to_string());
            self.font = DEFAULT_FACE.to_string();
        } else if face.encode_utf16().count() > MAX_FACE_UNITS {
            problems.push(format!("font 不能超过 {MAX_FACE_UNITS} 个字符"));
            self.font = DEFAULT_FACE.to_string();
        } else {
            self.font = face;
        }
        if !(MIN_WEIGHT..=MAX_WEIGHT).contains(&self.weight) {
            problems.push(format!(
                "weight 需在 {MIN_WEIGHT}–{MAX_WEIGHT} 之间，收到 {}",
                self.weight
            ));
            self.weight = DEFAULT_WEIGHT;
        }
        let color = self.color.trim().to_string();
        if parse_color(&color).is_none() && !color.eq_ignore_ascii_case("auto") {
            problems.push(format!(
                "color 需为 auto、#RRGGBB 或 #RGB，收到 \"{}\"",
                self.color
            ));
            self.color = DEFAULT_COLOR.to_string();
        } else {
            self.color = color;
        }
        if !(0.0..=1.0).contains(&self.opacity) {
            problems.push(format!("opacity 需在 0.0–1.0 之间，收到 {}", self.opacity));
            self.opacity = DEFAULT_OPACITY;
        }
        if !(0..=1).contains(&self.position) {
            problems.push(format!(
                "position 需为 0（左下角）或 1（右下角），收到 {}",
                self.position
            ));
            self.position = DEFAULT_POSITION;
        }
        if !(0..=1).contains(&self.tray_icon) {
            problems.push(format!(
                "tray_icon 需为 0（隐藏）或 1（显示），收到 {}",
                self.tray_icon
            ));
            self.tray_icon = DEFAULT_TRAY_ICON;
        }
        if !(MIN_PT..=MAX_PT).contains(&self.size) {
            problems.push(format!(
                "size 需在 {MIN_PT}–{MAX_PT} pt 之间，收到 {}",
                self.size
            ));
            self.size = DEFAULT_PT;
        }
        let note = (!problems.is_empty()).then(|| {
            format!(
                "settings.json 中有无法使用的值：{}。\n本次运行使用默认值，修正该文件后重启应用即可。",
                problems.join("；")
            )
        });
        (self, note)
    }
}

/// Parse `#RGB` or `#RRGGBB` (the `#` is required). Pure.
pub fn parse_color(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.trim().strip_prefix('#')?;
    let mut digits = [0u8; 6];
    for (i, b) in hex.bytes().enumerate() {
        if i >= 6 {
            return None;
        }
        digits[i] = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
    }
    match hex.len() {
        3 => Some((digits[0] * 17, digits[1] * 17, digits[2] * 17)),
        6 => Some((
            digits[0] * 16 + digits[1],
            digits[2] * 16 + digits[3],
            digits[4] * 16 + digits[5],
        )),
        _ => None,
    }
}

/// Parse `settings.json` (UTF-8, optional BOM). Pure.
///
/// Missing fields take their defaults; unknown fields are ignored so a
/// newer file never breaks an older build.
pub fn parse(raw: &str) -> Result<Settings, String> {
    let s = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if s.trim().is_empty() {
        return Err("文件为空".to_string());
    }
    serde_json::from_str(s).map_err(|e| format!("JSON 解析失败: {e}"))
}

/// Serialize the shape the user is meant to edit, with a trailing newline.
pub fn to_json(settings: &Settings) -> Result<String, String> {
    serde_json::to_string_pretty(settings)
        .map(|text| text + "\n")
        .map_err(|e| e.to_string())
}

/// Load the settings from `dir` (the executable's directory).
///
/// Missing file → write the defaults out, then use them. Present but
/// unreadable/unparseable → defaults plus a human-readable reason.
pub fn load(dir: &Path) -> (Settings, Option<String>) {
    let path = dir.join(FILE_NAME);
    let defaults = Settings::default();
    match fs::read_to_string(&path) {
        Ok(raw) => match parse(&raw) {
            Ok(settings) => settings.sanitized(),
            Err(reason) => (
                defaults,
                Some(format!(
                    "{}: {reason}\n本次运行使用默认字体。修正该文件后重启应用即可。",
                    path.display()
                )),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Ok(text) = to_json(&defaults) {
                let _ = fs::write(&path, text);
            }
            (defaults, None)
        }
        Err(e) => (
            defaults,
            Some(format!(
                "无法读取 {}: {e}\n本次运行使用默认字体。",
                path.display()
            )),
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d =
            std::env::temp_dir().join(format!("jinju-set-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn defaults_are_the_shipped_look() {
        let d = Settings::default();
        assert_eq!(d.font, DEFAULT_FACE);
        assert_eq!(d.weight, 400);
        assert_eq!(d.size, 11);
        assert_eq!(d.color, "auto");
        assert_eq!(d.opacity, 1.0);
        assert_eq!(d.position, 0);
        assert_eq!(d.tray_icon, 1);
        assert!(d.color.eq_ignore_ascii_case("auto"));
        assert_eq!(d.alpha(), 255);
    }

    #[test]
    fn auto_colour_follows_the_taskbar_theme() {
        let d = Settings::default();
        assert_eq!(d.rgb(true), AUTO_COLOR_LIGHT, "light bar -> black text");
        assert_eq!(d.rgb(false), AUTO_COLOR_DARK, "dark bar -> white text");
        // An explicit colour ignores the theme.
        let pinned = parse(r##"{"color":"#1E1E1E"}"##).unwrap();
        assert_eq!(pinned.rgb(true), (30, 30, 30));
        assert_eq!(pinned.rgb(false), (30, 30, 30));
        assert!(!pinned.color.eq_ignore_ascii_case("auto"));
    }

    #[test]
    fn parse_color_accepts_short_and_long_forms() {
        assert_eq!(parse_color("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_color("#FFFFFF"), Some((255, 255, 255)));
        assert_eq!(parse_color("auto"), None, "auto is not a colour literal");
        assert_eq!(parse_color("#1E1E1E"), Some((30, 30, 30)));
        assert_eq!(parse_color("#FFF"), Some((255, 255, 255)));
        assert_eq!(parse_color("#08f"), Some((0, 136, 255)));
        assert_eq!(parse_color("#ffff"), None, "4 digits are not a colour");
        assert_eq!(parse_color("1E1E1E"), None, "the # is required");
        assert_eq!(parse_color("#GGGGGG"), None, "not hex");
        assert_eq!(parse_color("#1234567"), None, "too long");
    }

    #[test]
    fn sanitize_rejects_bad_color_and_opacity() {
        let (s, note) = parse(r##"{"color":"#12345","opacity":1.5}"##)
            .unwrap()
            .sanitized();
        assert_eq!(s.color, DEFAULT_COLOR);
        assert_eq!(s.opacity, DEFAULT_OPACITY);
        let note = note.expect("a note for bad values");
        assert!(note.contains("color"), "{note}");
        assert!(note.contains("opacity"), "{note}");
    }

    #[test]
    fn sanitize_keeps_a_valid_opaque_black() {
        // The "look like taskbar text" preset: one note-free sanitize.
        let (s, note) = parse(r##"{"color":"#000","opacity":1.0}"##)
            .unwrap()
            .sanitized();
        assert_eq!(s.rgb(true), (0, 0, 0));
        assert_eq!(s.alpha(), 255);
        assert!(note.is_none());
    }

    #[test]
    fn parse_fills_missing_fields_with_defaults() {
        let s = parse(r#"{"font":"SimHei"}"#).unwrap();
        assert_eq!(s.font, "SimHei");
        assert_eq!(s.weight, DEFAULT_WEIGHT);
        assert_eq!(s.size, DEFAULT_PT);
    }

    #[test]
    fn parse_ignores_unknown_fields() {
        let s = parse(r#"{"font":"SimHei","colour":"red","size":12}"#).unwrap();
        assert_eq!(s.font, "SimHei");
        assert_eq!(s.size, 12);
    }

    #[test]
    fn parse_accepts_bom_and_windows_newlines() {
        let raw = "\u{feff}{\r\n  \"font\": \"KaiTi\"\r\n}";
        assert_eq!(parse(raw).unwrap().font, "KaiTi");
    }

    #[test]
    fn parse_rejects_empty_and_broken_files() {
        assert!(parse("  \n ").unwrap_err().contains("文件为空"));
        assert!(parse("{{{ nope").unwrap_err().contains("JSON"));
    }

    #[test]
    fn sanitize_rejects_out_of_range_values() {
        let (s, note) = parse(r#"{"weight":12,"size":400}"#).unwrap().sanitized();
        assert_eq!(s.weight, DEFAULT_WEIGHT);
        assert_eq!(s.size, DEFAULT_PT);
        let note = note.expect("a note for out-of-range values");
        assert!(note.contains("weight"), "{note}");
        assert!(note.contains("size"), "{note}");
    }

    #[test]
    fn sanitize_rejects_a_bad_position() {
        let (s, note) = parse(r#"{"position":2}"#).unwrap().sanitized();
        assert_eq!(s.position, DEFAULT_POSITION);
        assert!(
            note.expect("a note for a bad position")
                .contains("position")
        );

        let (s, note) = parse(r#"{"position":-1}"#).unwrap().sanitized();
        assert_eq!(s.position, DEFAULT_POSITION);
        assert!(note.is_some());
    }

    #[test]
    fn sanitize_accepts_both_positions() {
        let (s, note) = parse(r#"{"position":0}"#).unwrap().sanitized();
        assert_eq!(s.position, 0);
        assert!(note.is_none());
        let (s, note) = parse(r#"{"position":1}"#).unwrap().sanitized();
        assert_eq!(s.position, 1);
        assert!(note.is_none());
    }

    #[test]
    fn sanitize_rejects_a_bad_tray_icon() {
        let (s, note) = parse(r#"{"tray_icon":2}"#).unwrap().sanitized();
        assert_eq!(s.tray_icon, DEFAULT_TRAY_ICON);
        assert!(
            note.expect("a note for a bad tray_icon")
                .contains("tray_icon")
        );

        let (s, note) = parse(r#"{"tray_icon":0}"#).unwrap().sanitized();
        assert_eq!(s.tray_icon, 0);
        assert!(note.is_none());
        let (s, note) = parse(r#"{"tray_icon":1}"#).unwrap().sanitized();
        assert_eq!(s.tray_icon, 1);
        assert!(note.is_none());
    }

    #[test]
    fn sanitize_rejects_blank_and_overlong_faces() {
        let (s, note) = parse(r#"{"font":"   "}"#).unwrap().sanitized();
        assert_eq!(s.font, DEFAULT_FACE);
        assert!(note.unwrap().contains("font"));

        let long = format!(r#"{{"font":"{}"}}"#, "X".repeat(40));
        let (s, note) = parse(&long).unwrap().sanitized();
        assert_eq!(s.font, DEFAULT_FACE);
        assert!(note.unwrap().contains("31"));
    }

    #[test]
    fn sanitize_trims_a_user_supplied_face() {
        let (s, note) = parse(r#"{"font":"  SimHei  "}"#).unwrap().sanitized();
        assert_eq!(s.font, "SimHei");
        assert!(note.is_none());
    }

    #[test]
    fn to_json_round_trips() {
        let s = Settings {
            font: "SimHei".to_string(),
            weight: 400,
            size: 13,
            color: "#000000".to_string(),
            opacity: 1.0,
            position: 1,
            tray_icon: 0,
        };
        assert_eq!(parse(&to_json(&s).unwrap()).unwrap(), s);
    }

    #[test]
    fn load_exports_defaults_when_missing() {
        let dir = temp_dir("export");
        assert!(!dir.join(FILE_NAME).exists());
        let (s, note) = load(&dir);
        assert_eq!(s, Settings::default());
        assert!(note.is_none());
        let on_disk = fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert_eq!(parse(&on_disk).unwrap(), s);
        assert!(
            on_disk.contains("font"),
            "the exported file documents the knobs"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_reads_user_settings() {
        let dir = temp_dir("user");
        fs::write(
            dir.join(FILE_NAME),
            r#"{"font":"SimHei","weight":400,"size":13}"#,
        )
        .unwrap();
        let (s, note) = load(&dir);
        assert_eq!(s.font, "SimHei");
        assert_eq!((s.weight, s.size), (400, 13));
        assert!(note.is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_falls_back_on_broken_file() {
        let dir = temp_dir("broken");
        fs::write(dir.join(FILE_NAME), "not json").unwrap();
        let (s, note) = load(&dir);
        assert_eq!(s, Settings::default());
        assert!(note.unwrap().contains("JSON"));
        fs::remove_dir_all(&dir).ok();
    }
}
