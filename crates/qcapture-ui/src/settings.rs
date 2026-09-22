//! Widget settings persistence: remember target, encoder, audio, output
//! folder and toggles across restarts.
//!
//! Location is OS-appropriate (`%APPDATA%\QCapture\settings.json` on
//! Windows, `~/.config/qcapture/settings.json` elsewhere) so recordings can
//! start from CWD without polluting it. Every field is `#[serde(default)]`:
//! a corrupt or future-version file falls back to defaults field by field,
//! never failing startup. Only the widget reads/writes this; the CLI stays
//! stateless (explicit flags win over memory).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::widget::{AdvCfg, TargetSel};

pub const SETTINGS_VERSION: u32 = 1;

/// Everything the widget restores on launch. dB (not linear) gains so the
/// file stays human-tweakable; empty `output_dir` means the current folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSettings {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub target: Option<TargetSel>,
    #[serde(default)]
    pub region_screen: u32,
    #[serde(default)]
    pub window_text: String,
    #[serde(default)]
    pub mic_name: Option<String>,
    #[serde(default = "default_true")]
    pub audio_on: bool,
    #[serde(default)]
    pub sys_gain_db: f32,
    #[serde(default)]
    pub sys_muted: bool,
    #[serde(default)]
    pub mic_gain_db: f32,
    #[serde(default)]
    pub mic_muted: bool,
    #[serde(default)]
    pub adv: Option<AdvCfg>,
    #[serde(default)]
    pub draw_live: bool,
    #[serde(default)]
    pub cursor_highlight: bool,
    #[serde(default)]
    pub cursor_ripple: bool,
    #[serde(default)]
    pub output_dir: String,
    #[serde(default)]
    pub countdown_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            target: None,
            region_screen: 0,
            window_text: String::new(),
            mic_name: None,
            audio_on: true,
            sys_gain_db: 0.0,
            sys_muted: false,
            mic_gain_db: 0.0,
            mic_muted: false,
            adv: None,
            draw_live: false,
            cursor_highlight: false,
            cursor_ripple: false,
            output_dir: String::new(),
            countdown_enabled: false,
        }
    }
}

/// Clamp persisted dB into the slider range before rebuilding linear gains.
pub fn clamp_db(db: f32) -> f32 {
    db.clamp(-60.0, 12.0)
}

pub fn settings_path() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata)
                .join("QCapture")
                .join("settings.json");
        }
        // No APPDATA (service sessions): fall back next to the temp dir.
        std::env::temp_dir().join("qcapture-settings.json")
    }
    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".config")
                .join("qcapture")
                .join("settings.json");
        }
        std::env::temp_dir().join("qcapture-settings.json")
    }
}

/// Load settings; None when missing or unreadable (caller uses defaults).
/// Corrupt files warn on stderr instead of failing startup.
pub fn load() -> Option<PersistedSettings> {
    load_from(&settings_path())
}

fn load_from(path: &std::path::Path) -> Option<PersistedSettings> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<PersistedSettings>(&text) {
        Ok(mut s) => {
            if s.version > SETTINGS_VERSION {
                eprintln!(
                    "settings v{} newer than supported v{SETTINGS_VERSION} — using what fits",
                    s.version
                );
            }
            s.sys_gain_db = clamp_db(s.sys_gain_db);
            s.mic_gain_db = clamp_db(s.mic_gain_db);
            Some(s)
        }
        Err(e) => {
            eprintln!(
                "warning: ignoring unreadable settings ({}): {e}",
                path.display()
            );
            None
        }
    }
}

/// Save settings, creating the parent dir. Errors are returned for the
/// caller to log — a failed save must never break recording.
pub fn save(s: &PersistedSettings) -> Result<(), String> {
    save_to(&settings_path(), s)
}

fn save_to(path: &std::path::Path, s: &PersistedSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("settings dir {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(s).map_err(|e| format!("settings json: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("settings write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_everything() {
        let mut s = PersistedSettings::default();
        s.target = Some(TargetSel::Region {
            screen: 1,
            rect: qcapture_core::Rect::new(10, 20, 640, 480),
        });
        s.mic_name = Some("Test Mic".into());
        s.sys_gain_db = -6.0;
        s.mic_muted = true;
        s.draw_live = true;
        s.output_dir = "D:\\Videos".into();
        let json = serde_json::to_string(&s).unwrap();
        let back: PersistedSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.output_dir, "D:\\Videos");
        assert!(back.draw_live && back.mic_muted);
        assert_eq!(back.sys_gain_db, -6.0);
        assert!(matches!(back.target, Some(TargetSel::Region { .. })));
    }

    #[test]
    fn corrupt_file_falls_back_to_none() {
        let dir = std::env::temp_dir();
        let path = dir.join("qcapture-settings-corrupt-test.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(load_from(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_and_reload_from_disk() {
        let dir = std::env::temp_dir();
        let path = dir.join("qcapture-settings-roundtrip-test.json");
        let mut s = PersistedSettings::default();
        s.window_text = "Notepad".into();
        s.cursor_ripple = true;
        save_to(&path, &s).unwrap();
        let back = load_from(&path).unwrap();
        assert_eq!(back.window_text, "Notepad");
        assert!(back.cursor_ripple);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn db_clamped_on_load() {
        assert_eq!(clamp_db(99.0), 12.0);
        assert_eq!(clamp_db(-99.0), -60.0);
        assert_eq!(clamp_db(0.0), 0.0);
    }

    #[test]
    fn rgba_settings_file_loads() {
        // Current files carry [r,g,b,a] cursor colors.
        let dir = std::env::temp_dir();
        let path = dir.join("qcapture-settings-rgba-test.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "adv": {
                    "fps": 30,
                    "bitrate_kbps": 8000,
                    "canvas": null,
                    "encoder": "Auto",
                    "show_cursor": true,
                    "rc": "Cbr",
                    "qp": 23,
                    "crf": 23,
                    "maxrate_kbps": 12000,
                    "cursor_color": [255, 0, 0, 128],
                    "cursor_size": 14.0,
                    "ripple_color": [255, 255, 255, 255],
                    "ripple_size": 42.0,
                    "ripple_ms": 600,
                    "additive": true
                }
            }"#,
        )
        .unwrap();
        let back = load_from(&path).unwrap();
        let adv = back.adv.unwrap();
        assert_eq!(adv.cursor_color, [255, 0, 0, 128]);
        assert!(adv.additive);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rgb_settings_file_loads_opaque() {
        // Files from the RGB-only release carry [r,g,b] — alpha defaults
        // to opaque instead of failing the whole load.
        let dir = std::env::temp_dir();
        let path = dir.join("qcapture-settings-rgb-test.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "adv": {
                    "fps": 30,
                    "bitrate_kbps": 8000,
                    "canvas": null,
                    "encoder": "Auto",
                    "show_cursor": true,
                    "rc": "Cbr",
                    "qp": 23,
                    "crf": 23,
                    "maxrate_kbps": 12000,
                    "cursor_color": [255, 210, 0],
                    "cursor_size": 14.0,
                    "ripple_color": [255, 255, 255],
                    "ripple_size": 42.0,
                    "ripple_ms": 600
                }
            }"#,
        )
        .unwrap();
        let back = load_from(&path).unwrap();
        let adv = back.adv.unwrap();
        assert_eq!(adv.cursor_color, [255, 210, 0, 255]);
        assert_eq!(adv.ripple_color, [255, 255, 255, 255]);
        assert!(!adv.additive);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pre_style_settings_file_loads_with_style_defaults() {
        // A settings file written before cursor style existed has no
        // cursor_* keys inside `adv` — it must still load, with defaults.
        let dir = std::env::temp_dir();
        let path = dir.join("qcapture-settings-pre-style-test.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "target": null,
                "region_screen": 0,
                "window_text": "",
                "mic_name": null,
                "audio_on": true,
                "sys_gain_db": 0.0,
                "sys_muted": false,
                "mic_gain_db": 0.0,
                "mic_muted": false,
                "adv": {
                    "fps": 60,
                    "bitrate_kbps": 8000,
                    "canvas": null,
                    "encoder": "Nvenc",
                    "show_cursor": true,
                    "rc": "Cbr",
                    "qp": 23,
                    "crf": 23,
                    "maxrate_kbps": 12000
                },
                "draw_live": false,
                "cursor_highlight": false,
                "cursor_ripple": false,
                "output_dir": ""
            }"#,
        )
        .unwrap();
        let back = load_from(&path).unwrap();
        let adv = back.adv.unwrap();
        assert_eq!(adv.fps, 60);
        assert_eq!(adv.encoder, crate::widget::EncoderSel::Nvenc);
        assert_eq!(adv.cursor_color, [255, 210, 0, 255]);
        assert_eq!(adv.cursor_size, 14.0);
        assert_eq!(adv.ripple_color, [255, 255, 255, 255]);
        assert_eq!(adv.ripple_size, 42.0);
        assert_eq!(adv.ripple_ms, 600);
        let _ = std::fs::remove_file(&path);
    }
}
