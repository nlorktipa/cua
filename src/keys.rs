//! Keymaps for the virtual keyboard. Keys are sent with standard US-layout
//! keycodes (so app shortcuts that look at physical keys work); characters a
//! US layout can't produce are typed through a temporary custom keymap.

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use xkbcommon::xkb;

pub struct Keyboard {
    us: String,
    /// keysym -> (evdev keycode, modifier mask)
    by_sym: HashMap<u32, (u32, u32)>,
    shift: u32,
    ctrl: u32,
    alt: u32,
    logo: u32,
    altgr: u32,
}

impl Keyboard {
    pub fn new() -> Result<Keyboard> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(&ctx, "evdev", "pc104", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
            .ok_or_else(|| anyhow!("could not build a US keymap (is xkeyboard-config installed?)"))?;
        let mask = |name: &str| {
            let i = keymap.mod_get_index(name);
            if i == xkb::MOD_INVALID { 0 } else { 1u32 << i }
        };
        let (shift, ctrl, alt, logo, altgr) =
            (mask(xkb::MOD_NAME_SHIFT), mask(xkb::MOD_NAME_CTRL), mask(xkb::MOD_NAME_ALT), mask(xkb::MOD_NAME_LOGO), mask("Mod5"));

        let mut by_sym: HashMap<u32, (u32, u32)> = HashMap::new();
        let mut best_level: HashMap<u32, u32> = HashMap::new();
        for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            let kc = xkb::Keycode::new(raw);
            for level in 0..keymap.num_levels_for_key(kc, 0) {
                let mods = match level {
                    0 => 0,
                    1 => shift,
                    2 => altgr,
                    3 => shift | altgr,
                    _ => continue,
                };
                for sym in keymap.key_get_syms_by_level(kc, 0, level) {
                    let s = sym.raw();
                    if best_level.get(&s).is_none_or(|l| level < *l) {
                        best_level.insert(s, level);
                        by_sym.insert(s, (raw - 8, mods));
                    }
                }
            }
        }
        Ok(Keyboard { us: keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1), by_sym, shift, ctrl, alt, logo, altgr })
    }

    pub fn us_keymap(&self) -> &str {
        &self.us
    }

    fn char_sym(c: char) -> u32 {
        match c {
            '\n' => xkb::keysyms::KEY_Return,
            '\t' => xkb::keysyms::KEY_Tab,
            '\u{8}' => xkb::keysyms::KEY_BackSpace,
            '\u{1b}' => xkb::keysyms::KEY_Escape,
            _ => xkb::utf32_to_keysym(c as u32).raw(),
        }
    }

    /// Keycode and modifiers that produce `c` on the US layout.
    pub fn lookup_char(&self, c: char) -> Option<(u32, u32)> {
        self.by_sym.get(&Self::char_sym(c)).copied()
    }

    /// A one-off keymap holding exactly these characters, one per key.
    pub fn custom_keymap(&self, chars: &[char]) -> (String, Vec<u32>) {
        let mut syms: Vec<u32> = Vec::new();
        let codes = chars
            .iter()
            .map(|c| {
                let s = Self::char_sym(*c);
                let i = syms.iter().position(|x| *x == s).unwrap_or_else(|| {
                    syms.push(s);
                    syms.len() - 1
                });
                i as u32 + 1 // evdev keycode; xkb keycode is +8
            })
            .collect();
        let mut text = String::from("xkb_keymap {\nxkb_keycodes \"cua\" {\nminimum = 8;\n");
        text.push_str(&format!("maximum = {};\n", syms.len() + 9));
        for i in 0..syms.len() {
            text.push_str(&format!("<K{}> = {};\n", i + 1, i + 9));
        }
        text.push_str("};\nxkb_types \"cua\" { include \"complete\" };\nxkb_compatibility \"cua\" { include \"complete\" };\nxkb_symbols \"cua\" {\n");
        for (i, s) in syms.iter().enumerate() {
            text.push_str(&format!("key <K{}> {{[ {} ]}};\n", i + 1, xkb::keysym_get_name(xkb::Keysym::new(*s))));
        }
        text.push_str("};\n};\n");
        (text, codes)
    }

    /// Modifier mask and key name for a modifier word like "ctrl".
    fn modifier(&self, word: &str) -> Option<(u32, &'static str)> {
        Some(match word.to_lowercase().as_str() {
            "ctrl" | "control" | "ctl" => (self.ctrl, "Control_L"),
            "shift" => (self.shift, "Shift_L"),
            "alt" | "option" | "opt" | "meta" => (self.alt, "Alt_L"),
            "super" | "win" | "windows" | "cmd" | "command" | "logo" | "mod4" => (self.logo, "Super_L"),
            "altgr" => (self.altgr, "ISO_Level3_Shift"),
            _ => return None,
        })
    }

    /// Parse "ctrl+shift" into a modifier mask ("" is no modifiers).
    pub fn parse_mods(&self, spec: &str) -> Result<u32> {
        spec.split('+')
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .try_fold(0, |acc, w| Ok(acc | self.modifier(w).ok_or_else(|| anyhow!("unknown modifier {w:?}"))?.0))
    }

    /// Parse "ctrl+shift+t", "alt+F4", "Return", "super" into keycode + modifiers.
    pub fn parse_combo(&self, combo: &str) -> Result<(u32, u32)> {
        let parts: Vec<&str> = combo.split('+').map(str::trim).collect();
        // "ctrl++" means ctrl and the plus key.
        let parts: Vec<String> = if combo.ends_with("++") {
            let mut p: Vec<String> = parts[..parts.len() - 2].iter().map(|s| s.to_string()).collect();
            p.push("plus".into());
            p
        } else {
            parts.iter().map(|s| s.to_string()).collect()
        };
        let mut mods = 0;
        let mut key: Option<String> = None;
        let mut last_mod: Option<&str> = None;
        for p in &parts {
            if p.is_empty() {
                bail!("bad key combination {combo:?}");
            }
            if let Some((mask, keysym)) = self.modifier(p) {
                mods |= mask;
                last_mod = Some(keysym);
            } else {
                if key.is_some() {
                    bail!("{combo:?} has more than one non-modifier key; send them as separate combos");
                }
                key = Some(p.clone());
            }
        }
        // A bare modifier ("super") taps that modifier key itself.
        let key = match key {
            Some(k) => k,
            None => last_mod.ok_or_else(|| anyhow!("empty key combination"))?.to_string(),
        };
        let sym = self.resolve(&key).ok_or_else(|| anyhow!("unknown key {key:?}"))?;
        let mut sym_lookup = sym;
        // Uppercase letters are their lowercase key with Shift.
        if let Some(c) = char::from_u32(xkb::keysym_to_utf32(xkb::Keysym::new(sym)))
            && c.is_ascii_uppercase() {
                sym_lookup = xkb::utf32_to_keysym(c.to_ascii_lowercase() as u32).raw();
                mods |= self.shift;
            }
        let (code, extra) = self
            .by_sym
            .get(&sym_lookup)
            .copied()
            .ok_or_else(|| anyhow!("key {key:?} is not on the keyboard layout"))?;
        Ok((code, mods | extra))
    }

    fn resolve(&self, name: &str) -> Option<u32> {
        let alias = match name.to_lowercase().as_str() {
            "enter" | "return" | "ret" => "Return",
            "esc" | "escape" => "Escape",
            "backspace" | "bksp" => "BackSpace",
            "del" | "delete" => "Delete",
            "ins" | "insert" => "Insert",
            "tab" => "Tab",
            "space" | "spc" => "space",
            "up" | "arrowup" => "Up",
            "down" | "arrowdown" => "Down",
            "left" | "arrowleft" => "Left",
            "right" | "arrowright" => "Right",
            "pageup" | "page_up" | "pgup" | "prior" => "Prior",
            "pagedown" | "page_down" | "pgdn" | "next" => "Next",
            "home" => "Home",
            "end" => "End",
            "menu" => "Menu",
            "print" | "printscreen" => "Print",
            "capslock" => "Caps_Lock",
            "plus" => "plus",
            "minus" => "minus",
            "volumeup" => "XF86AudioRaiseVolume",
            "volumedown" => "XF86AudioLowerVolume",
            "mute" => "XF86AudioMute",
            _ => "",
        };
        if !alias.is_empty() {
            return Some(xkb::keysym_from_name(alias, xkb::KEYSYM_NO_FLAGS).raw());
        }
        let mut chars = name.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return Some(Self::char_sym(c));
        }
        for flags in [xkb::KEYSYM_NO_FLAGS, xkb::KEYSYM_CASE_INSENSITIVE] {
            let sym = xkb::keysym_from_name(name, flags);
            if sym.raw() != 0 {
                return Some(sym.raw());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        let k = Keyboard::new().unwrap();
        let (a, m) = k.parse_combo("a").unwrap();
        assert_eq!((a, m), (30, 0)); // KEY_A
        let (t, m) = k.parse_combo("ctrl+shift+t").unwrap();
        assert_eq!(t, 20);
        assert_eq!(m, k.ctrl | k.shift);
        assert_eq!(k.parse_combo("ctrl+T").unwrap(), (20, k.ctrl | k.shift));
        assert_eq!(k.parse_combo("Return").unwrap().0, 28);
        assert_eq!(k.parse_combo("super").unwrap(), (125, k.logo));
        assert_eq!(k.parse_combo("alt+F4").unwrap(), (62, k.alt));
        assert_eq!(k.parse_combo("ctrl++").unwrap(), (13, k.ctrl | k.shift));
        assert!(k.parse_combo("ctrl+a+b").is_err());
    }

    #[test]
    fn chars() {
        let k = Keyboard::new().unwrap();
        assert_eq!(k.lookup_char('A'), Some((30, k.shift)));
        assert_eq!(k.lookup_char('\n').unwrap().0, 28);
        assert!(k.lookup_char('é').is_none());
        let (text, codes) = k.custom_keymap(&['é', 'ü', 'é']);
        assert_eq!(codes, vec![1, 2, 1]);
        assert!(text.contains("eacute"));
    }
}
