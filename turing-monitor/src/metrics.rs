// SPDX-License-Identifier: GPL-3.0-or-later
//! The metric table: every value a theme can reference, keyed by name.
//!
//! Themes bind to metrics by string (`cpu.usage`, `gpu.temp`), so adding a
//! new metric is a matter of inserting it here — the render engine never
//! needs to know it exists.

use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub enum Value {
    /// A number in its natural unit; formatters turn it into text.
    Num(f64),
    Text(String),
    /// A set of related values, e.g. per-core load.
    Series(Vec<f32>),
}

#[derive(Default)]
pub struct Metrics {
    map: BTreeMap<String, Value>,
}

impl Metrics {
    pub fn new() -> Self {
        Metrics::default()
    }

    pub fn set_num(&mut self, key: &str, v: f64) {
        self.map.insert(key.to_string(), Value::Num(v));
    }

    pub fn set_text(&mut self, key: &str, v: impl Into<String>) {
        self.map.insert(key.to_string(), Value::Text(v.into()));
    }

    pub fn set_series(&mut self, key: &str, v: Vec<f32>) {
        self.map.insert(key.to_string(), Value::Series(v));
    }

    /// Insert only when present, so absent hardware leaves the key undefined
    /// and `require` in a theme can drop the whole section.
    pub fn set_opt_num(&mut self, key: &str, v: Option<f64>) {
        if let Some(v) = v {
            self.set_num(key, v);
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(key)
    }

    pub fn num(&self, key: &str) -> Option<f64> {
        match self.map.get(key)? {
            Value::Num(v) => Some(*v),
            _ => None,
        }
    }

    pub fn series(&self, key: &str) -> Option<&[f32]> {
        match self.map.get(key)? {
            Value::Series(v) => Some(v),
            _ => None,
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// Every metric name currently defined, for `--list-metrics`.
    pub fn keys(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.map.iter().map(|(k, v)| (k.as_str(), v))
    }
}

// ---------------------------------------------------------------- formatters

/// Render a number the way a theme asked for it.
pub fn format_value(v: f64, formatter: &str) -> String {
    match formatter {
        "percent" => format!("{:.0}%", v * 100.0),
        "percent1" => format!("{:.1}%", v * 100.0),
        "celsius" => format!("{v:.0}\u{00b0}C"),
        "bytes" => fmt_bytes(v.max(0.0) as u64),
        "rate" => format!("{}/s", fmt_bytes(v.max(0.0) as u64)),
        "mhz" => format!("{v:.0} MHz"),
        "ghz" => format!("{:.2} GHz", v / 1000.0),
        "uptime" => fmt_uptime(v.max(0.0) as u64),
        "int" => format!("{v:.0}"),
        "float1" => format!("{v:.1}"),
        "float2" => format!("{v:.2}"),
        // Unknown formatter: show the raw number rather than failing silently.
        _ => format!("{v:.0}"),
    }
}

pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    // One decimal only where it carries information.
    if u == 0 || v >= 100.0 {
        format!("{:.0} {}", v, UNITS[u])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

pub fn fmt_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatters_cover_the_documented_set() {
        assert_eq!(format_value(0.456, "percent"), "46%");
        assert_eq!(format_value(62.4, "celsius"), "62\u{00b0}C");
        assert_eq!(format_value(3813.0, "ghz"), "3.81 GHz");
        assert_eq!(format_value(1536.0, "rate"), "1.5 kB/s");
        assert_eq!(format_value(0.42, "float2"), "0.42");
    }

    #[test]
    fn unknown_formatter_falls_back_to_a_number() {
        assert_eq!(format_value(7.0, "nonsense"), "7");
    }

    #[test]
    fn bytes_drops_the_decimal_when_it_is_noise() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 kB");
        // Above 100 in a unit the decimal adds nothing.
        assert_eq!(fmt_bytes(150 * 1024 * 1024), "150 MB");
    }

    #[test]
    fn optional_metrics_stay_absent() {
        let mut m = Metrics::new();
        m.set_opt_num("gpu.usage", None);
        assert!(!m.has("gpu.usage"));
        m.set_opt_num("gpu.usage", Some(0.5));
        assert_eq!(m.num("gpu.usage"), Some(0.5));
    }
}
