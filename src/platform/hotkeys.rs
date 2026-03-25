use anyhow::{Context, Result};
use eframe::egui;
use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager};
use log::info;

pub struct HotkeyManager {
    manager: GlobalHotKeyManager,
    save_hotkey: HotKey,
    save_hotkey_id: u32,
    record_hotkey: Option<HotKey>,
    record_hotkey_id: Option<u32>,
}

impl HotkeyManager {
    pub fn new(save_key: &str, record_key: Option<&str>) -> Result<Self> {
        let manager = GlobalHotKeyManager::new().context("Failed to create GlobalHotKeyManager")?;

        let save_hotkey: HotKey = save_key
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid save hotkey '{}': {}", save_key, e))?;
        let save_hotkey_id = save_hotkey.id();
        manager
            .register(save_hotkey)
            .context(format!("Failed to register save hotkey {}", save_key))?;
        info!(
            "Save hotkey registered: {} (id={})",
            save_key, save_hotkey_id
        );

        let (record_hotkey, record_hotkey_id) = if let Some(rk) = record_key {
            if !rk.is_empty() {
                let hk: HotKey = rk
                    .parse()
                    .map_err(|e| anyhow::anyhow!("Invalid record hotkey '{}': {}", rk, e))?;
                let id = hk.id();
                manager
                    .register(hk)
                    .context(format!("Failed to register record hotkey {}", rk))?;
                info!("Record hotkey registered: {} (id={})", rk, id);
                (Some(hk), Some(id))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        Ok(Self {
            manager,
            save_hotkey,
            save_hotkey_id,
            record_hotkey,
            record_hotkey_id,
        })
    }

    pub fn save_hotkey_id(&self) -> u32 {
        self.save_hotkey_id
    }

    pub fn record_hotkey_id(&self) -> Option<u32> {
        self.record_hotkey_id
    }

    pub fn unregister_all(&self) {
        let _ = self.manager.unregister(self.save_hotkey);
        if let Some(hk) = self.record_hotkey {
            let _ = self.manager.unregister(hk);
        }
        info!("Hotkeys unregistered");
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        self.unregister_all();
    }
}

pub fn poll_hotkey_event() -> Option<GlobalHotKeyEvent> {
    GlobalHotKeyEvent::receiver().try_recv().ok()
}

pub fn egui_key_to_code_str(key: &egui::Key) -> Option<&'static str> {
    use egui::Key;
    Some(match key {
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        Key::F13 => "F13",
        Key::F14 => "F14",
        Key::F15 => "F15",
        Key::F16 => "F16",
        Key::F17 => "F17",
        Key::F18 => "F18",
        Key::F19 => "F19",
        Key::F20 => "F20",
        Key::Home => "Home",
        Key::End => "End",
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::Backtick => "Backquote",
        Key::A => "KeyA",
        Key::B => "KeyB",
        Key::C => "KeyC",
        Key::D => "KeyD",
        Key::E => "KeyE",
        Key::F => "KeyF",
        Key::G => "KeyG",
        Key::H => "KeyH",
        Key::I => "KeyI",
        Key::J => "KeyJ",
        Key::K => "KeyK",
        Key::L => "KeyL",
        Key::M => "KeyM",
        Key::N => "KeyN",
        Key::O => "KeyO",
        Key::P => "KeyP",
        Key::Q => "KeyQ",
        Key::R => "KeyR",
        Key::S => "KeyS",
        Key::T => "KeyT",
        Key::U => "KeyU",
        Key::V => "KeyV",
        Key::W => "KeyW",
        Key::X => "KeyX",
        Key::Y => "KeyY",
        Key::Z => "KeyZ",
        Key::Num0 => "Digit0",
        Key::Num1 => "Digit1",
        Key::Num2 => "Digit2",
        Key::Num3 => "Digit3",
        Key::Num4 => "Digit4",
        Key::Num5 => "Digit5",
        Key::Num6 => "Digit6",
        Key::Num7 => "Digit7",
        Key::Num8 => "Digit8",
        Key::Num9 => "Digit9",
        Key::Space => "Space",
        Key::Tab => "Tab",
        Key::Escape => "Escape",
        Key::Backspace => "Backspace",
        Key::Enter => "Enter",
        Key::Minus => "Minus",
        Key::Plus => "Equal",
        Key::OpenBracket => "BracketLeft",
        Key::CloseBracket => "BracketRight",
        Key::Backslash => "Backslash",
        Key::Semicolon => "Semicolon",
        Key::Comma => "Comma",
        Key::Period => "Period",
        Key::Slash => "Slash",
        _ => return None,
    })
}

pub fn build_hotkey_string(modifiers: &egui::Modifiers, key: &egui::Key) -> Option<String> {
    let code_str = egui_key_to_code_str(key)?;

    let mut parts = Vec::new();
    if modifiers.ctrl {
        parts.push("Control");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    parts.push(code_str);

    Some(parts.join("+"))
}

pub fn display_hotkey_string(modifiers: &egui::Modifiers, key: &egui::Key) -> Option<String> {
    let key_name = egui_key_to_code_str(key)?;
    let display_key = if let Some(stripped) = key_name.strip_prefix("Key") {
        stripped
    } else if let Some(stripped) = key_name.strip_prefix("Digit") {
        stripped
    } else {
        key_name
    };

    let mut parts = Vec::new();
    if modifiers.ctrl {
        parts.push("Ctrl");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    parts.push(display_key);

    Some(parts.join(" + "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_egui_key_to_code_str_f_keys() {
        assert_eq!(egui_key_to_code_str(&egui::Key::F1), Some("F1"));
        assert_eq!(egui_key_to_code_str(&egui::Key::F8), Some("F8"));
        assert_eq!(egui_key_to_code_str(&egui::Key::F12), Some("F12"));
    }

    #[test]
    fn test_egui_key_to_code_str_letters() {
        assert_eq!(egui_key_to_code_str(&egui::Key::A), Some("KeyA"));
        assert_eq!(egui_key_to_code_str(&egui::Key::Z), Some("KeyZ"));
    }

    #[test]
    fn test_egui_key_to_code_str_digits() {
        assert_eq!(egui_key_to_code_str(&egui::Key::Num0), Some("Digit0"));
        assert_eq!(egui_key_to_code_str(&egui::Key::Num9), Some("Digit9"));
    }

    #[test]
    fn test_build_hotkey_string_no_modifiers() {
        let mods = egui::Modifiers::NONE;
        let result = build_hotkey_string(&mods, &egui::Key::F8);
        assert_eq!(result, Some("F8".to_string()));
    }

    #[test]
    fn test_build_hotkey_string_with_modifiers() {
        let mods = egui::Modifiers {
            ctrl: true,
            shift: true,
            alt: false,
            ..Default::default()
        };
        let result = build_hotkey_string(&mods, &egui::Key::F8);
        assert_eq!(result, Some("Control+Shift+F8".to_string()));
    }

    #[test]
    fn test_display_hotkey_string_strips_key_prefix() {
        let mods = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let result = display_hotkey_string(&mods, &egui::Key::A);
        assert_eq!(result, Some("Ctrl + A".to_string()));
    }

    #[test]
    fn test_display_hotkey_string_strips_digit_prefix() {
        let mods = egui::Modifiers::NONE;
        let result = display_hotkey_string(&mods, &egui::Key::Num5);
        assert_eq!(result, Some("5".to_string()));
    }
}
