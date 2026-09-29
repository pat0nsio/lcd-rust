// SPDX-License-Identifier: GPL-3.0-or-later
//! Plain `key = value` configuration.
//!
//! Deliberately not TOML: the whole config is two dozen scalars, and a real
//! parser would cost more dependencies than the rest of the binary.

use std::collections::HashMap;

use turing_lcd::{Orientation, Rgb};

pub struct Config {
    pub port: Option<String>,
    pub orientation: Orientation,
    pub brightness: u8,
    pub refresh_ms: u64,
    pub net_interface: Option<String>,
    pub disk: String,
    pub font_regular: Option<String>,
    pub font_bold: Option<String>,
    /// Theme file; `None` uses the one built into the binary.
    pub theme: Option<String>,
    /// Second theme. Con el puesto, SIGUSR1 alterna entre los dos.
    pub theme_alt: Option<String>,
    pub cpu_temp_chip: String,
    pub cpu_temp_label: Option<String>,
    /// Blank the panel on exit instead of leaving the last frame up.
    pub screen_off_on_exit: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: None,
            orientation: Orientation::Portrait,
            brightness: 60,
            refresh_ms: 1000,
            net_interface: None,
            disk: "/".into(),
            font_regular: None,
            font_bold: None,
            theme: None,
            theme_alt: None,
            cpu_temp_chip: "k10temp".into(),
            cpu_temp_label: Some("Tctl".into()),
            screen_off_on_exit: false,
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Config::parse(&text).map_err(|e| format!("{path}: {e}"))
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let mut kv: HashMap<&str, &str> = HashMap::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("line {}: expected `key = value`", n + 1))?;
            kv.insert(k.trim(), v.trim());
        }

        let mut c = Config::default();
        // `auto`/`none`/empty all mean "fall back to the default behaviour".
        let opt = |v: &str| {
            let v = v.trim();
            (!v.is_empty() && !v.eq_ignore_ascii_case("auto") && !v.eq_ignore_ascii_case("none"))
                .then(|| v.to_string())
        };

        for (k, v) in kv {
            match k {
                "port" => c.port = opt(v),
                "orientation" => c.orientation = parse_orientation(v)?,
                "brightness" => c.brightness = parse_num(k, v)?,
                "refresh_ms" => c.refresh_ms = parse_num::<u64>(k, v)?.max(100),
                "net_interface" => c.net_interface = opt(v),
                "disk" => c.disk = v.to_string(),
                "font_regular" => c.font_regular = opt(v),
                "font_bold" => c.font_bold = opt(v),
                "theme" => c.theme = opt(v),
                "theme_alt" => c.theme_alt = opt(v),
                "cpu_temp_chip" => c.cpu_temp_chip = v.to_string(),
                "cpu_temp_label" => c.cpu_temp_label = opt(v),
                "screen_off_on_exit" => c.screen_off_on_exit = parse_bool(k, v)?,
                other => return Err(format!("unknown key `{other}`")),
            }
        }
        if c.brightness > 100 {
            return Err("brightness must be 0-100".into());
        }
        Ok(c)
    }
}

fn parse_num<T: std::str::FromStr>(key: &str, v: &str) -> Result<T, String> {
    v.parse()
        .map_err(|_| format!("{key}: `{v}` is not a valid number"))
}

fn parse_bool(key: &str, v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Ok(true),
        "false" | "no" | "0" | "off" => Ok(false),
        _ => Err(format!("{key}: `{v}` is not a boolean")),
    }
}

fn parse_orientation(v: &str) -> Result<Orientation, String> {
    match v.to_ascii_lowercase().as_str() {
        "portrait" => Ok(Orientation::Portrait),
        "reverse_portrait" => Ok(Orientation::ReversePortrait),
        "landscape" => Ok(Orientation::Landscape),
        "reverse_landscape" => Ok(Orientation::ReverseLandscape),
        _ => Err(format!("unknown orientation `{v}`")),
    }
}

/// `#rrggbb`, `rrggbb`, or `r,g,b`.
pub fn parse_color(v: &str) -> Result<Rgb, String> {
    let s = v.trim();
    if let Some(hex) = s.strip_prefix('#').or(Some(s)).filter(|h| h.len() == 6) {
        if let Ok(n) = u32::from_str_radix(hex, 16) {
            return Ok([(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        }
    }
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() == 3 {
        let mut out = [0u8; 3];
        for (i, p) in parts.iter().enumerate() {
            out[i] = p
                .trim()
                .parse()
                .map_err(|_| format!("bad colour component `{p}`"))?;
        }
        return Ok(out);
    }
    Err(format!("bad colour `{v}`, expected #rrggbb or r,g,b"))
}
