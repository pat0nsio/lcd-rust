// SPDX-License-Identifier: GPL-3.0-or-later
//! Declarative themes.
//!
//! A theme is a TOML file describing a stack of sections. Nothing carries
//! absolute pixel coordinates: sections flow top to bottom and rows are laid
//! out inside them, so one theme fits any panel size. Colours are named
//! tokens defined once in `[palette]`, and widgets bind to metrics by name.

use std::collections::BTreeMap;

use serde::Deserialize;
use turing_lcd::Rgb;

// ---------------------------------------------------------------- file model

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    #[serde(default)]
    theme: Meta,
    #[serde(default)]
    palette: BTreeMap<String, String>,
    #[serde(default)]
    section: Vec<SectionDef>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Meta {
    name: Option<String>,
    author: Option<String>,
    margin: Option<u16>,
    background: Option<String>,
    /// How many side-by-side columns the content area is split into.
    columns: Option<u16>,
    /// Relative widths of those columns; `[2, 3]` gives the second half again
    /// as much room. Defaults to equal shares.
    column_widths: Option<Vec<u16>>,
    /// Horizontal space between those columns.
    column_gap: Option<u16>,
    /// Stroke of the frame round `frame = true` sections.
    frame_width: Option<u16>,
    /// Frame colour; defaults to the palette's `line`. A ramp follows each
    /// section's own load, so a frame can light up only when it is busy.
    frame_color: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SectionDef {
    title: Option<String>,
    title_size: Option<u16>,
    /// Title colour, when it should not follow the section's.
    title_color: Option<String>,
    value: Option<String>,
    value_size: Option<u16>,
    value_style: Option<String>,
    color: Option<String>,
    /// Metric that drives the colour ramp, when it is not the value itself.
    color_from: Option<String>,
    /// Skip the whole section when this metric is undefined.
    require: Option<String>,
    /// "top" (default) stacks in order; "bottom" pins to the bottom edge.
    anchor: Option<String>,
    /// 1-based column this section flows down, for multi-column themes.
    column: Option<u16>,
    /// Draw across every column instead of inside one.
    span: Option<bool>,
    gap: Option<u16>,
    /// Draw a window-style frame round the section.
    frame: Option<bool>,
    /// Space between the frame and the content.
    padding: Option<u16>,
    /// Stretch the section down to whatever is below it in its column.
    grow: Option<bool>,
    #[serde(default)]
    rows: Vec<RowDef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RowDef {
    text: Option<String>,
    right: Option<String>,
    size: Option<u16>,
    right_size: Option<u16>,
    style: Option<String>,
    right_style: Option<String>,
    color: Option<String>,
    right_color: Option<String>,
    color_from: Option<String>,
    right_color_from: Option<String>,
    bar: Option<String>,
    cores: Option<String>,
    art: Option<String>,
    plot: Option<PlotRef>,
    height: Option<u16>,
    max: Option<f64>,
    scale: Option<String>,
    fill: Option<String>,
    track: Option<String>,
    rule: Option<bool>,
    gap: Option<u16>,
    /// Let the left text wrap onto up to this many lines.
    lines: Option<u16>,
    /// Skip just this row when the metric is undefined.
    require: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PlotRef {
    One(String),
    Many(Vec<String>),
}

// ---------------------------------------------------------------- compiled

/// A colour that may depend on the widget's current load.
#[derive(Clone, Debug)]
pub enum ColorSpec {
    Fixed(Rgb),
    /// Interpolate `from`→`to` as the value goes `start`→1.0.
    Ramp { from: Rgb, to: Rgb, start: f32 },
}

impl ColorSpec {
    pub fn resolve(&self, load: f32) -> Rgb {
        match self {
            ColorSpec::Fixed(c) => *c,
            ColorSpec::Ramp { from, to, start } => {
                let t = if *start >= 1.0 {
                    0.0
                } else {
                    ((load - start) / (1.0 - start)).clamp(0.0, 1.0)
                };
                turing_lcd::widgets::lerp_color(*from, *to, t)
            }
        }
    }
}

/// How a raw metric maps onto the 0..=1 a bar or plot needs.
#[derive(Clone, Copy, Debug)]
pub enum Scale {
    /// The metric is already a fraction.
    Unit,
    Max(f64),
    /// Divide by the largest value seen in the plot's own history.
    Auto,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Normal,
    Bold,
    Dim,
}

#[derive(Clone, Debug)]
pub struct TextItem {
    /// Either a bare `metric | formatter` or a `{...}` template.
    pub template: String,
    pub size: u16,
    pub style: Style,
    pub color: Option<ColorSpec>,
    /// Metric feeding the colour ramp, when it is not the one shown.
    pub load_from: Option<String>,
    /// Most lines the text may wrap onto; 1 cuts it with an ellipsis.
    pub lines: u16,
}

#[derive(Clone, Debug)]
pub enum Row {
    Text {
        left: Option<TextItem>,
        right: Option<TextItem>,
    },
    Bar {
        metric: String,
        height: u16,
        color: Option<ColorSpec>,
        track: Rgb,
        scale: Scale,
    },
    /// A square image — the album cover — filling the column width.
    Art {
        metric: String,
        /// Cap on the side, when the column is wider than the art should be.
        max: Option<u16>,
    },
    /// One slim vertical bar per element of a series.
    Cores {
        metric: String,
        height: u16,
        color: Option<ColorSpec>,
        track: Rgb,
    },
    Plot {
        metrics: Vec<String>,
        height: u16,
        colors: Vec<ColorSpec>,
        fill: Option<Rgb>,
        track: Rgb,
        scale: Scale,
    },
    Rule(Rgb),
    Gap(u16),
}

/// A row plus the condition under which it is drawn at all.
#[derive(Clone, Debug)]
pub struct RowEntry {
    pub require: Option<String>,
    pub row: Row,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Anchor {
    Top,
    Bottom,
}

#[derive(Clone, Debug)]
pub struct Section {
    pub title: Option<String>,
    pub title_size: u16,
    pub title_color: Option<ColorSpec>,
    pub value: Option<TextItem>,
    pub color: ColorSpec,
    pub color_from: Option<String>,
    pub require: Option<String>,
    pub anchor: Anchor,
    /// 0-based column index; ignored when `span` is set.
    pub column: usize,
    pub span: bool,
    pub gap: u16,
    pub frame: bool,
    pub padding: u16,
    pub grow: bool,
    pub rows: Vec<RowEntry>,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub author: Option<String>,
    pub margin: u16,
    pub background: Rgb,
    pub columns: u16,
    pub column_gap: u16,
    /// One weight per column; empty means equal shares.
    pub column_widths: Vec<u16>,
    pub frame_width: u16,
    pub frame_color: ColorSpec,
    pub sections: Vec<Section>,
}

// ---------------------------------------------------------------- compiling

/// Resolves colour names while compiling.
struct Palette {
    map: BTreeMap<String, Rgb>,
}

impl Palette {
    /// A literal (`#rrggbb`, `r,g,b`) or a name from `[palette]`.
    fn color(&self, spec: &str) -> Result<Rgb, String> {
        let s = spec.trim();
        if let Some(c) = self.map.get(s) {
            return Ok(*c);
        }
        crate::config::parse_color(s)
            .map_err(|_| format!("unknown colour `{s}` (not a palette name, #rrggbb or r,g,b)"))
    }

    /// Also understands the ramp form `from -> to @ start`.
    fn color_spec(&self, spec: &str) -> Result<ColorSpec, String> {
        let s = spec.trim();
        let Some((from, rest)) = s.split_once("->") else {
            return Ok(ColorSpec::Fixed(self.color(s)?));
        };
        let (to, start) = match rest.split_once('@') {
            Some((to, at)) => {
                let start: f32 = at
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad ramp threshold in `{s}`"))?;
                (to, start)
            }
            None => (rest, 0.0),
        };
        Ok(ColorSpec::Ramp {
            from: self.color(from)?,
            to: self.color(to)?,
            start,
        })
    }

    fn opt_spec(&self, spec: &Option<String>) -> Result<Option<ColorSpec>, String> {
        match spec {
            Some(s) => Ok(Some(self.color_spec(s)?)),
            None => Ok(None),
        }
    }
}

fn parse_style(s: &Option<String>) -> Result<Style, String> {
    match s.as_deref() {
        None | Some("normal") => Ok(Style::Normal),
        Some("bold") => Ok(Style::Bold),
        Some("dim") => Ok(Style::Dim),
        Some(other) => Err(format!("unknown style `{other}` (normal, bold, dim)")),
    }
}

fn parse_scale(row: &RowDef) -> Result<Scale, String> {
    match (row.scale.as_deref(), row.max) {
        (Some("auto"), _) => Ok(Scale::Auto),
        (Some("unit"), _) | (None, None) => Ok(Scale::Unit),
        (None, Some(m)) if m > 0.0 => Ok(Scale::Max(m)),
        (None, Some(_)) => Err("`max` must be greater than zero".into()),
        (Some(other), _) => Err(format!("unknown scale `{other}` (auto, unit)")),
    }
}

impl Theme {
    pub fn parse(text: &str) -> Result<Theme, String> {
        let file: ThemeFile = toml::from_str(text).map_err(|e| e.to_string())?;

        let mut map = BTreeMap::new();
        for (name, spec) in &file.palette {
            let c = crate::config::parse_color(spec)
                .map_err(|e| format!("palette `{name}`: {e}"))?;
            map.insert(name.clone(), c);
        }
        let palette = Palette { map };

        let background = match &file.theme.background {
            Some(s) => palette.color(s)?,
            None => [0, 0, 0],
        };
        // Default track colour for bars and plots, so themes need not repeat it.
        let default_track = palette
            .map
            .get("panel")
            .copied()
            .unwrap_or([0x21, 0x26, 0x2d]);

        let columns = file.theme.columns.unwrap_or(1);
        if columns == 0 {
            return Err("`columns` must be at least 1".into());
        }
        let column_widths = file.theme.column_widths.clone().unwrap_or_default();
        if !column_widths.is_empty() {
            if column_widths.len() != columns as usize {
                return Err(format!(
                    "`column_widths` has {} entries but the theme declares {columns} column(s)",
                    column_widths.len()
                ));
            }
            if column_widths.iter().all(|w| *w == 0) {
                return Err("`column_widths` cannot be all zeros".into());
            }
        }

        let mut sections = Vec::new();
        for (i, s) in file.section.iter().enumerate() {
            sections.push(
                compile_section(s, &palette, default_track, columns)
                    .map_err(|e| format!("section {} ({}): {e}", i + 1, s.title.as_deref().unwrap_or("untitled")))?,
            );
        }

        Ok(Theme {
            name: file.theme.name.clone().unwrap_or_else(|| "unnamed".into()),
            author: file.theme.author.clone(),
            margin: file.theme.margin.unwrap_or(8),
            background,
            columns,
            column_gap: file.theme.column_gap.unwrap_or(10),
            column_widths,
            frame_width: file.theme.frame_width.unwrap_or(2),
            frame_color: match &file.theme.frame_color {
                Some(s) => palette.color_spec(s)?,
                None => ColorSpec::Fixed(
                    palette.map.get("line").copied().unwrap_or([0x2c, 0x2c, 0x2c]),
                ),
            },
            sections,
        })
    }

    pub fn load(path: &str) -> Result<Theme, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Theme::parse(&text).map_err(|e| format!("{path}: {e}"))
    }
}

fn compile_section(
    s: &SectionDef,
    p: &Palette,
    track: Rgb,
    columns: u16,
) -> Result<Section, String> {
    let color = match &s.color {
        Some(c) => p.color_spec(c)?,
        None => ColorSpec::Fixed(p.map.get("text").copied().unwrap_or([255, 255, 255])),
    };
    let anchor = match s.anchor.as_deref() {
        None | Some("top") => Anchor::Top,
        Some("bottom") => Anchor::Bottom,
        Some(other) => return Err(format!("unknown anchor `{other}` (top, bottom)")),
    };
    // Columns are 1-based in the file so `column = 2` reads as "the second one".
    let column = match s.column {
        None => 0,
        Some(0) => return Err("`column` is 1-based, so 0 is not a column".into()),
        Some(n) if n <= columns => (n - 1) as usize,
        Some(n) => {
            return Err(format!(
                "`column = {n}` but the theme only declares {columns} column(s)"
            ))
        }
    };
    let value = match &s.value {
        Some(v) => Some(TextItem {
            template: v.clone(),
            size: s.value_size.unwrap_or(20),
            style: parse_style(&s.value_style)?,
            color: None, // Falls back to the section colour.
            load_from: s.color_from.clone(),
            lines: 1,
        }),
        None => None,
    };

    let mut rows = Vec::new();
    for (i, r) in s.rows.iter().enumerate() {
        let row = compile_row(r, p, track).map_err(|e| format!("row {}: {e}", i + 1))?;
        rows.push(RowEntry {
            require: r.require.clone(),
            row,
        });
    }

    Ok(Section {
        title: s.title.clone(),
        title_size: s.title_size.unwrap_or(15),
        title_color: p.opt_spec(&s.title_color)?,
        value,
        color,
        color_from: s.color_from.clone(),
        require: s.require.clone(),
        anchor,
        column,
        span: s.span.unwrap_or(false),
        gap: s.gap.unwrap_or(6),
        frame: s.frame.unwrap_or(false),
        padding: s.padding.unwrap_or(8),
        grow: s.grow.unwrap_or(false),
        rows,
    })
}

fn compile_row(r: &RowDef, p: &Palette, default_track: Rgb) -> Result<Row, String> {
    let track = match &r.track {
        Some(t) => p.color(t)?,
        None => default_track,
    };

    if let Some(gap) = r.gap {
        return Ok(Row::Gap(gap));
    }
    if r.rule == Some(true) {
        return Ok(Row::Rule(track));
    }
    if let Some(metric) = &r.bar {
        return Ok(Row::Bar {
            metric: metric.clone(),
            height: r.height.unwrap_or(10),
            color: p.opt_spec(&r.color)?,
            track,
            scale: parse_scale(r)?,
        });
    }
    if let Some(metric) = &r.art {
        return Ok(Row::Art {
            metric: metric.clone(),
            max: r.height,
        });
    }
    if let Some(metric) = &r.cores {
        return Ok(Row::Cores {
            metric: metric.clone(),
            height: r.height.unwrap_or(14),
            color: p.opt_spec(&r.color)?,
            track,
        });
    }
    if let Some(plot) = &r.plot {
        let metrics = match plot {
            PlotRef::One(m) => vec![m.clone()],
            PlotRef::Many(m) => m.clone(),
        };
        if metrics.is_empty() {
            return Err("`plot` needs at least one metric".into());
        }
        // A plot of several series takes one colour per series; `color` may be
        // a single spec or the list is filled from the palette-free default.
        let mut colors = Vec::new();
        if let Some(c) = &r.color {
            for part in c.split(';') {
                colors.push(p.color_spec(part)?);
            }
        }
        while colors.len() < metrics.len() {
            colors.push(ColorSpec::Fixed([0x58, 0xa6, 0xff]));
        }
        return Ok(Row::Plot {
            metrics,
            height: r.height.unwrap_or(30),
            colors,
            fill: match &r.fill {
                Some(f) => Some(p.color(f)?),
                None => None,
            },
            track,
            scale: parse_scale(r)?,
        });
    }
    if r.text.is_some() || r.right.is_some() {
        let left = match &r.text {
            Some(t) => Some(TextItem {
                template: t.clone(),
                size: r.size.unwrap_or(11),
                style: parse_style(&r.style)?,
                color: p.opt_spec(&r.color)?,
                load_from: r.color_from.clone(),
                lines: r.lines.unwrap_or(1),
            }),
            None => None,
        };
        let right = match &r.right {
            Some(t) => Some(TextItem {
                template: t.clone(),
                size: r.right_size.or(r.size).unwrap_or(11),
                style: parse_style(&r.right_style)?,
                color: p.opt_spec(&r.right_color)?,
                load_from: r.right_color_from.clone(),
                lines: 1,
            }),
            None => None,
        };
        return Ok(Row::Text { left, right });
    }
    Err("row has no recognised key (text, right, bar, cores, art, plot, rule, gap)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r##"
[palette]
accent = "#58a6ff"
warn = "#f85149"
panel = "#21262d"

[[section]]
title = "CPU"
value = "cpu.usage | percent"
color = "accent -> warn @ 0.7"
rows = [ { bar = "cpu.usage", height = 10 } ]
"##;

    #[test]
    fn compiles_a_minimal_theme() {
        let t = Theme::parse(MINIMAL).expect("should parse");
        assert_eq!(t.sections.len(), 1);
        assert_eq!(t.sections[0].title.as_deref(), Some("CPU"));
    }

    #[test]
    fn ramp_stays_at_base_colour_below_threshold() {
        let t = Theme::parse(MINIMAL).unwrap();
        let c = &t.sections[0].color;
        assert_eq!(c.resolve(0.0), [0x58, 0xa6, 0xff]);
        assert_eq!(c.resolve(0.7), [0x58, 0xa6, 0xff]);
        assert_eq!(c.resolve(1.0), [0xf8, 0x51, 0x49]);
    }

    #[test]
    fn palette_names_and_literals_both_work() {
        let t = Theme::parse(
            r##"
[palette]
mine = "#010203"
[[section]]
rows = [ { bar = "x", color = "mine" }, { bar = "y", color = "#040506" } ]
"##,
        )
        .unwrap();
        let colors: Vec<Rgb> = t.sections[0]
            .rows
            .iter()
            .map(|r| match &r.row {
                Row::Bar { color, .. } => color.as_ref().unwrap().resolve(0.0),
                _ => panic!("expected bars"),
            })
            .collect();
        assert_eq!(colors, vec![[1, 2, 3], [4, 5, 6]]);
    }

    #[test]
    fn unknown_key_is_an_error_not_a_silent_no_op() {
        let err = Theme::parse("[[section]]\nrows = [ { bar = \"x\", heigth = 4 } ]\n")
            .expect_err("typo must be rejected");
        assert!(err.contains("heigth"), "error should name the typo: {err}");
    }

    #[test]
    fn unknown_colour_is_reported() {
        let err = Theme::parse("[[section]]\nrows = [ { bar = \"x\", color = \"nope\" } ]\n")
            .expect_err("unknown colour must be rejected");
        assert!(err.contains("nope"), "error should name the colour: {err}");
    }

    #[test]
    fn require_is_captured_at_both_levels() {
        let t = Theme::parse(
            r##"
[[section]]
title = "RAM"
require = "mem.usage"
rows = [
  { bar = "mem.usage" },
  { bar = "swap.usage", require = "swap.usage" },
]
"##,
        )
        .unwrap();
        let sec = &t.sections[0];
        assert_eq!(sec.require.as_deref(), Some("mem.usage"));
        assert_eq!(sec.rows[0].require, None);
        assert_eq!(sec.rows[1].require.as_deref(), Some("swap.usage"));
    }

    #[test]
    fn the_builtin_theme_compiles() {
        // Guards against the shipped theme drifting away from the parser.
        let t = Theme::parse(include_str!("../../themes/default.toml"))
            .expect("built-in theme must parse");
        assert!(!t.sections.is_empty());
        let t = Theme::parse(include_str!("../../themes/spotify.toml"))
            .expect("spotify theme must parse");
        assert!(!t.sections.is_empty());
    }

    #[test]
    fn a_theme_without_columns_is_a_single_full_width_one() {
        let t = Theme::parse(MINIMAL).unwrap();
        assert_eq!(t.columns, 1);
        assert_eq!(t.sections[0].column, 0);
        assert!(!t.sections[0].span);
    }

    #[test]
    fn columns_are_one_based_in_the_file_and_zero_based_after() {
        let t = Theme::parse(
            r##"
[theme]
columns = 2
[[section]]
column = 2
rows = [ { bar = "x" } ]
[[section]]
span = true
rows = [ { rule = true } ]
"##,
        )
        .unwrap();
        assert_eq!(t.sections[0].column, 1);
        assert!(!t.sections[0].span);
        assert!(t.sections[1].span);
    }

    #[test]
    fn column_widths_must_match_the_column_count() {
        let ok = Theme::parse(
            "[theme]\ncolumns = 2\ncolumn_widths = [2, 3]\n[[section]]\nrows = [ { bar = \"x\" } ]\n",
        )
        .unwrap();
        assert_eq!(ok.column_widths, vec![2, 3]);
        let err = Theme::parse(
            "[theme]\ncolumns = 2\ncolumn_widths = [1]\n[[section]]\nrows = [ { bar = \"x\" } ]\n",
        )
        .expect_err("a short list must be rejected");
        assert!(err.contains("1 entries"), "error should say why: {err}");
    }

    #[test]
    fn a_column_the_theme_does_not_have_is_an_error() {
        let err = Theme::parse("[[section]]\ncolumn = 2\nrows = [ { bar = \"x\" } ]\n")
            .expect_err("column 2 of a 1-column theme must be rejected");
        assert!(err.contains("1 column"), "error should say why: {err}");
        assert!(Theme::parse("[[section]]\ncolumn = 0\nrows = [ { bar = \"x\" } ]\n").is_err());
    }

    #[test]
    fn empty_row_is_rejected() {
        assert!(Theme::parse("[[section]]\nrows = [ { } ]\n").is_err());
    }
}
