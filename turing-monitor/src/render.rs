// SPDX-License-Identifier: GPL-3.0-or-later
//! Turns a [`Theme`] plus a [`Metrics`] table into pixels.
//!
//! Layout is a vertical flow: sections stack from the top (or the bottom, for
//! anchored ones) and rows stack inside them. Nothing in a theme names a pixel
//! coordinate, so the same theme works on any panel size.

use std::collections::{HashMap, VecDeque};

use turing_lcd::canvas::{Canvas, Rect};
use turing_lcd::widgets::{progress_bar, sparkline};
use turing_lcd::{Align, Font, Rgb, VAlign};

use crate::metrics::{format_value, Metrics, Value};
use crate::theme::{Anchor, ColorSpec, Row, RowEntry, Scale, Section, Style, TextItem, Theme};

/// Fixed-capacity history of raw samples, oldest first.
struct History {
    values: VecDeque<f32>,
    /// Kept separately: `VecDeque` rounds its capacity up, so it is not a cap.
    cap: usize,
}

impl History {
    fn new(cap: usize) -> Self {
        History {
            values: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn push(&mut self, v: f32) {
        if self.values.len() == self.cap {
            self.values.pop_front();
        }
        self.values.push_back(v);
    }

    fn max(&self) -> f32 {
        self.values.iter().copied().fold(0.0, f32::max)
    }
}

/// The content area split into side-by-side columns. `columns = 1` gives a
/// single column the full width, which is the layout every theme had before
/// columns existed.
struct Columns {
    count: usize,
    /// Left edge of each column.
    x: Vec<u16>,
    width: u16,
    /// Left edge and width of the whole content area, for spanning sections.
    full_x: u16,
    full_width: u16,
}

impl Columns {
    fn new(theme: &Theme, margin: u16, content: u16) -> Columns {
        let count = theme.columns.max(1);
        let gaps = theme.column_gap.saturating_mul(count - 1);
        // Gaps wider than the panel leave no room; fall back to one column
        // rather than laying out zero-width ones.
        let (count, gap, width) = match content.checked_sub(gaps).map(|w| w / count) {
            Some(w) if w > 0 => (count as usize, theme.column_gap, w),
            _ => (1, 0, content),
        };
        let x = (0..count)
            .map(|i| margin + i as u16 * (width + gap))
            .collect();
        Columns {
            count,
            x,
            width,
            full_x: margin,
            full_width: content,
        }
    }

    /// Where a section is drawn, and which columns it consumes vertical space in.
    fn place(&self, sec: &Section) -> (u16, u16, std::ops::Range<usize>) {
        if sec.span || self.count == 1 {
            return (self.full_x, self.full_width, 0..self.count);
        }
        let c = sec.column.min(self.count - 1);
        (self.x[c], self.width, c..c + 1)
    }
}

/// What a render pass did, so tooling can report layout problems that would
/// otherwise show up only as a silently missing section.
#[derive(Clone, Debug, Default)]
pub struct RenderReport {
    /// Sections dropped because they did not fit.
    pub skipped: Vec<String>,
    /// Pixels consumed by the top-anchored flow.
    pub used_height: u16,
    /// Pixels the top flow had available.
    pub available_height: u16,
}

pub struct Renderer {
    regular: Font,
    bold: Font,
    /// One history per metric that some plot references.
    history: HashMap<String, History>,
    history_cap: usize,
}

impl Renderer {
    pub fn new(
        font_regular: Option<&str>,
        font_bold: Option<&str>,
        width: u16,
    ) -> Result<Renderer, String> {
        let regular = match font_regular {
            Some(p) => Font::load(p)?,
            None => Font::from_bytes(include_bytes!("../../assets/RobotoMono-Regular.ttf"))?,
        };
        let bold = match font_bold {
            Some(p) => Font::load(p)?,
            None => Font::from_bytes(include_bytes!("../../assets/RobotoMono-Bold.ttf"))?,
        };
        Ok(Renderer {
            regular,
            bold,
            history: HashMap::new(),
            // One sample per horizontal pixel is all a plot can show.
            history_cap: width.max(32) as usize,
        })
    }

    pub fn render(&mut self, canvas: &mut Canvas, theme: &Theme, m: &Metrics) -> RenderReport {
        let mut report = RenderReport::default();
        self.record_history(theme, m);
        canvas.clear(theme.background);

        let margin = theme.margin;
        let content = canvas.width().saturating_sub(2 * margin);
        if content == 0 {
            return report;
        }
        let cols = Columns::new(theme, margin, content);

        // Each column flows on its own, so every one carries its own pair of
        // cursors. With `columns = 1` this is exactly the old single flow.
        let n = cols.count;
        let mut top = vec![margin; n];
        let mut bottom = vec![canvas.height().saturating_sub(margin); n];

        // Bottom-anchored sections are placed first so the top flows know
        // where they have to stop.
        for sec in theme.sections.iter().rev() {
            if sec.anchor != Anchor::Bottom || !self.section_visible(sec, m) {
                continue;
            }
            let (x, w, span) = cols.place(sec);
            let h = self.section(canvas, sec, m, x, 0, w, false);
            // A spanning section has to clear the lowest of the columns it covers.
            let y = span
                .clone()
                .map(|c| bottom[c])
                .min()
                .unwrap_or(margin)
                .saturating_sub(h);
            self.section(canvas, sec, m, x, y, w, true);
            for c in span {
                bottom[c] = y.saturating_sub(sec.gap);
            }
        }

        for sec in &theme.sections {
            if sec.anchor != Anchor::Top || !self.section_visible(sec, m) {
                continue;
            }
            let (x, w, span) = cols.place(sec);
            let h = self.section(canvas, sec, m, x, 0, w, false);
            // Start below everything already in any column it covers.
            let y = span.clone().map(|c| top[c]).max().unwrap_or(margin);
            let floor = span.clone().map(|c| bottom[c]).min().unwrap_or(0);
            if y + h > floor {
                // Out of room: drop the section rather than overlap the footer.
                report
                    .skipped
                    .push(sec.title.clone().unwrap_or_else(|| "untitled".into()));
                continue;
            }
            self.section(canvas, sec, m, x, y, w, true);
            for c in span {
                top[c] = y + h + sec.gap;
            }
        }

        // The report speaks for the tightest column, which is the one that
        // decides whether the theme fits.
        report.used_height = top
            .iter()
            .copied()
            .max()
            .unwrap_or(margin)
            .saturating_sub(margin);
        report.available_height = bottom
            .iter()
            .copied()
            .min()
            .unwrap_or(margin)
            .saturating_sub(margin);
        report
    }

    /// Feed every plotted metric into its history, once per frame.
    fn record_history(&mut self, theme: &Theme, m: &Metrics) {
        for sec in &theme.sections {
            for entry in &sec.rows {
                let Row::Plot { metrics, .. } = &entry.row else {
                    continue;
                };
                for key in metrics {
                    let Some(v) = m.num(key) else { continue };
                    let cap = self.history_cap;
                    self.history
                        .entry(key.to_string())
                        .or_insert_with(|| History::new(cap))
                        .push(v as f32);
                }
            }
        }
    }

    fn section_visible(&self, sec: &Section, m: &Metrics) -> bool {
        match &sec.require {
            Some(key) => m.has(key),
            None => true,
        }
    }

    /// Draw (or just measure, when `draw` is false) one section. Returns its
    /// height so the caller can advance the flow.
    fn section(
        &mut self,
        canvas: &mut Canvas,
        sec: &Section,
        m: &Metrics,
        x: u16,
        y: u16,
        content: u16,
        draw: bool,
    ) -> u16 {
        let mut cursor = y;

        if sec.title.is_some() || sec.value.is_some() {
            if draw {
                let load = sec
                    .color_from
                    .as_ref()
                    .and_then(|k| m.num(k).map(|v| v as f32))
                    .or_else(|| sec.value.as_ref().and_then(|v| self.load_of(&v.template, m)))
                    .unwrap_or(0.0);
                let color = sec.color.resolve(load);
                if let Some(title) = &sec.title {
                    let title = resolve_label(title, m);
                    self.bold.draw(
                        canvas,
                        &title,
                        x as i32,
                        cursor as i32,
                        sec.title_size,
                        self.text_color(Style::Bold, None, 0.0, color),
                        Align::Left,
                        VAlign::Top,
                    );
                }
                if let Some(v) = &sec.value {
                    if let Some(text) = resolve_template(&v.template, m) {
                        self.bold.draw(
                            canvas,
                            &text,
                            (x + content) as i32,
                            (cursor as i32) - 2,
                            v.size,
                            color,
                            Align::Right,
                            VAlign::Top,
                        );
                    }
                }
            }
            // The heading line is as tall as the larger of the two faces on
            // it, so a theme can scale titles and readings up together.
            let value_size = sec.value.as_ref().map_or(0, |v| v.size);
            cursor += sec.title_size.max(value_size) + 2;
        }

        for entry in &sec.rows {
            if !row_visible(entry, m) {
                continue;
            }
            cursor += self.row(canvas, &entry.row, sec, m, x, cursor, content, draw);
        }
        cursor - y
    }

    fn row(
        &mut self,
        canvas: &mut Canvas,
        row: &Row,
        sec: &Section,
        m: &Metrics,
        x: u16,
        y: u16,
        content: u16,
        draw: bool,
    ) -> u16 {
        match row {
            Row::Gap(n) => *n,

            Row::Rule(color) => {
                if draw {
                    canvas.fill_rect(Rect::new(x, y, content, 1), *color);
                }
                5
            }

            Row::Text { left, right } => {
                let h = left
                    .as_ref()
                    .map(|t| t.size)
                    .max(right.as_ref().map(|t| t.size))
                    .unwrap_or(11)
                    + 4;
                if draw {
                    if let Some(t) = left {
                        self.draw_text_item(canvas, t, m, sec, x as i32, y, Align::Left);
                    }
                    if let Some(t) = right {
                        self.draw_text_item(
                            canvas,
                            t,
                            m,
                            sec,
                            (x + content) as i32,
                            y,
                            Align::Right,
                        );
                    }
                }
                h
            }

            Row::Bar {
                metric,
                height,
                color,
                track,
                scale,
            } => {
                if draw {
                    let v = self.normalized(metric, *scale, m);
                    let c = color.as_ref().unwrap_or(&sec.color).resolve(v);
                    progress_bar(canvas, Rect::new(x, y, content, *height), v, c, *track);
                }
                height + 4
            }

            Row::Cores {
                metric,
                height,
                color,
                track,
            } => {
                if draw {
                    if let Some(values) = m.series(metric) {
                        let n = values.len().max(1) as u16;
                        let gap = 2u16;
                        let bw = content.saturating_sub(gap * (n - 1)) / n;
                        let bw = bw.max(1);
                        for (i, v) in values.iter().enumerate() {
                            let bx = x + i as u16 * (bw + gap);
                            if bx + bw > x + content {
                                break;
                            }
                            let v = v.clamp(0.0, 1.0);
                            let filled = (*height as f32 * v).round() as u16;
                            canvas.fill_rect(Rect::new(bx, y, bw, *height), *track);
                            if filled > 0 {
                                let c = color.as_ref().unwrap_or(&sec.color).resolve(v);
                                canvas.fill_rect(
                                    Rect::new(bx, y + height - filled, bw, filled),
                                    c,
                                );
                            }
                        }
                    }
                }
                height + 4
            }

            Row::Plot {
                metrics,
                height,
                colors,
                fill,
                track,
                scale,
            } => {
                if draw {
                    // All series in one plot share an axis so their relative
                    // size stays meaningful.
                    let divisor = self.plot_divisor(metrics, *scale);
                    let rect = Rect::new(x, y, content, *height);
                    let mut first = true;
                    for (i, key) in metrics.iter().enumerate() {
                        let Some(hist) = self.history.get(key) else {
                            continue;
                        };
                        let norm: Vec<f32> =
                            hist.values.iter().map(|v| v / divisor).collect();
                        let color = colors
                            .get(i)
                            .map(|c| c.resolve(norm.last().copied().unwrap_or(0.0)))
                            .unwrap_or([0x58, 0xa6, 0xff]);
                        if first {
                            sparkline(
                                canvas,
                                rect,
                                &norm,
                                color,
                                fill.unwrap_or(dim_towards(color, *track)),
                                *track,
                            );
                            first = false;
                        } else {
                            // Later series are outlines so the first stays visible.
                            overlay_line(canvas, rect, &norm, color);
                        }
                    }
                }
                height + 4
            }
        }
    }

    fn draw_text_item(
        &mut self,
        canvas: &mut Canvas,
        item: &TextItem,
        m: &Metrics,
        sec: &Section,
        x: i32,
        y: u16,
        align: Align,
    ) {
        let Some(text) = resolve_template(&item.template, m) else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        let load = item
            .load_from
            .as_ref()
            .and_then(|k| m.num(k).map(|v| v as f32))
            .or_else(|| self.load_of(&item.template, m))
            .unwrap_or(0.0);
        let fallback = sec.color.resolve(load);
        let color = self.text_color(item.style, item.color.as_ref(), load, fallback);
        let font = match item.style {
            Style::Bold => &mut self.bold,
            _ => &mut self.regular,
        };
        font.draw(canvas, &text, x, y as i32, item.size, color, align, VAlign::Top);
    }

    /// Explicit colour wins; otherwise dim text uses the palette's dim and
    /// everything else inherits the section colour.
    fn text_color(
        &self,
        style: Style,
        explicit: Option<&ColorSpec>,
        load: f32,
        fallback: Rgb,
    ) -> Rgb {
        match explicit {
            Some(c) => c.resolve(load),
            None if style == Style::Dim => [0x8b, 0x94, 0x9e],
            None => fallback,
        }
    }

    /// The 0..=1 figure a colour ramp reacts to, taken from the first metric
    /// the template mentions.
    fn load_of(&self, template: &str, m: &Metrics) -> Option<f32> {
        let key = first_metric_name(template)?;
        Some(m.num(&key)? as f32)
    }

    fn normalized(&self, metric: &str, scale: Scale, m: &Metrics) -> f32 {
        let Some(v) = m.num(metric) else { return 0.0 };
        let d = match scale {
            Scale::Unit => 1.0,
            Scale::Max(max) => max,
            Scale::Auto => self
                .history
                .get(metric)
                .map(|h| h.max() as f64)
                .unwrap_or(1.0)
                .max(1e-9),
        };
        (v / d).clamp(0.0, 1.0) as f32
    }

    fn plot_divisor(&self, metrics: &[String], scale: Scale) -> f32 {
        match scale {
            Scale::Unit => 1.0,
            Scale::Max(max) => max as f32,
            // A floor keeps an idle series from filling the plot with noise.
            Scale::Auto => metrics
                .iter()
                .filter_map(|k| self.history.get(k))
                .map(|h| h.max())
                .fold(0.0, f32::max)
                .max(1024.0),
        }
    }
}

/// A row is drawn only when its `require` metric exists.
fn row_visible(entry: &RowEntry, m: &Metrics) -> bool {
    match &entry.require {
        Some(key) => m.has(key),
        None => true,
    }
}

/// Blend a series colour towards the track to get a readable fill.
fn dim_towards(color: Rgb, track: Rgb) -> Rgb {
    turing_lcd::widgets::lerp_color(color, track, 0.72)
}

/// Draw only the top edge of a series, over an existing plot.
fn overlay_line(canvas: &mut Canvas, r: Rect, values: &[f32], color: Rgb) {
    if values.is_empty() || r.h < 2 {
        return;
    }
    let cols = r.w as usize;
    let start = values.len().saturating_sub(cols);
    let shown = &values[start..];
    let offset = cols - shown.len();
    let mut prev: Option<u16> = None;
    for (i, v) in shown.iter().enumerate() {
        let x = r.x + (offset + i) as u16;
        let top = r.bottom() - 1 - (v.clamp(0.0, 1.0) * (r.h - 1) as f32).round() as u16;
        canvas.fill_rect(Rect::new(x, top, 1, 1), color);
        if let Some(p) = prev {
            let (lo, hi) = if p < top { (p, top) } else { (top, p) };
            if hi - lo > 1 {
                canvas.fill_rect(Rect::new(x, lo, 1, hi - lo), color);
            }
        }
        prev = Some(top);
    }
}

// ---------------------------------------------------------------- templates

/// `metric | formatter`, or a `{...}` template mixing several of them with
/// literal text. Returns `None` when nothing could be resolved.
pub fn resolve_template(spec: &str, m: &Metrics) -> Option<String> {
    if !spec.contains('{') {
        return eval_expr(spec, m);
    }
    let mut out = String::new();
    let mut rest = spec;
    let mut resolved_any = false;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // Unbalanced brace: emit the rest literally.
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        match eval_expr(&after[..close], m) {
            Some(v) => {
                out.push_str(&v);
                resolved_any = true;
            }
            // Missing metric: leave a blank rather than an error string.
            None => {}
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    if resolved_any || !out.trim().is_empty() {
        Some(out)
    } else {
        None
    }
}

/// Section titles are literal text; only `{...}` parts are substituted.
pub fn resolve_label(label: &str, m: &Metrics) -> String {
    if !label.contains('{') {
        return label.to_string();
    }
    resolve_template(label, m).unwrap_or_else(|| label.to_string())
}

fn eval_expr(expr: &str, m: &Metrics) -> Option<String> {
    let (key, formatter) = split_expr(expr);
    match m.get(&key)? {
        Value::Num(v) => Some(format_value(*v, formatter.as_deref().unwrap_or("int"))),
        Value::Text(t) => Some(t.clone()),
        Value::Series(_) => None,
    }
}

fn split_expr(expr: &str) -> (String, Option<String>) {
    match expr.split_once('|') {
        Some((k, f)) => (k.trim().to_string(), Some(f.trim().to_string())),
        None => (expr.trim().to_string(), None),
    }
}

/// First metric name a template refers to, used to drive colour ramps.
fn first_metric_name(template: &str) -> Option<String> {
    if !template.contains('{') {
        return Some(split_expr(template).0);
    }
    let open = template.find('{')?;
    let after = &template[open + 1..];
    let close = after.find('}')?;
    Some(split_expr(&after[..close]).0)
}

/// Every metric a theme mentions, so startup can warn about typos.
pub fn referenced_metrics(theme: &Theme) -> Vec<String> {
    let mut out = Vec::new();
    for sec in &theme.sections {
        if let Some(k) = &sec.require {
            out.push(k.clone());
        }
        if let Some(k) = &sec.color_from {
            out.push(k.clone());
        }
        if let Some(t) = &sec.title {
            // Titles are literal text, so only braced parts name metrics.
            if t.contains('{') {
                out.extend(all_metric_names(t));
            }
        }
        if let Some(v) = &sec.value {
            out.extend(all_metric_names(&v.template));
        }
        for entry in &sec.rows {
            if let Some(k) = &entry.require {
                out.push(k.clone());
            }
            match &entry.row {
                Row::Text { left, right } => {
                    for t in [left, right].into_iter().flatten() {
                        out.extend(all_metric_names(&t.template));
                        if let Some(k) = &t.load_from {
                            out.push(k.clone());
                        }
                    }
                }
                Row::Bar { metric, .. } | Row::Cores { metric, .. } => out.push(metric.clone()),
                Row::Plot { metrics, .. } => out.extend(metrics.iter().cloned()),
                Row::Rule(_) | Row::Gap(_) => {}
            }
        }
    }
    out.retain(|k| !k.is_empty());
    out.sort();
    out.dedup();
    out
}

fn all_metric_names(template: &str) -> Vec<String> {
    if !template.contains('{') {
        return vec![split_expr(template).0];
    }
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        out.push(split_expr(&after[..close]).0);
        rest = &after[close + 1..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> Metrics {
        let mut m = Metrics::new();
        m.set_num("cpu.usage", 0.456);
        m.set_num("mem.used", 2_500_000_000.0);
        m.set_num("mem.total", 33_000_000_000.0);
        m.set_text("cpu.model", "Ryzen 5 5600G");
        m
    }

    fn theme_with(columns: u16, gap: u16) -> Theme {
        Theme {
            name: "t".into(),
            author: None,
            margin: 8,
            background: [0, 0, 0],
            columns,
            column_gap: gap,
            sections: Vec::new(),
        }
    }

    fn section(column: usize, span: bool) -> Section {
        Section {
            title: None,
            title_size: 15,
            value: None,
            color: ColorSpec::Fixed([255, 255, 255]),
            color_from: None,
            require: None,
            anchor: Anchor::Top,
            column,
            span,
            gap: 6,
            rows: Vec::new(),
        }
    }

    #[test]
    fn one_column_gives_every_section_the_full_width() {
        let cols = Columns::new(&theme_with(1, 10), 8, 464);
        let (x, w, span) = cols.place(&section(0, false));
        assert_eq!((x, w), (8, 464));
        assert_eq!(span, 0..1);
    }

    #[test]
    fn two_columns_split_the_content_around_the_gap() {
        let cols = Columns::new(&theme_with(2, 12), 8, 464);
        assert_eq!(cols.place(&section(0, false)), (8, 226, 0..1));
        assert_eq!(cols.place(&section(1, false)), (246, 226, 1..2));
        // A spanning section covers the whole width and both columns.
        assert_eq!(cols.place(&section(0, true)), (8, 464, 0..2));
    }

    #[test]
    fn a_gap_wider_than_the_panel_falls_back_to_one_column() {
        // Better a cramped single column than an underflowed width.
        let cols = Columns::new(&theme_with(2, 600), 8, 464);
        assert_eq!(cols.count, 1);
        assert_eq!(cols.place(&section(1, false)), (8, 464, 0..1));
    }

    #[test]
    fn bare_expression_resolves() {
        assert_eq!(
            resolve_template("cpu.usage | percent", &metrics()).as_deref(),
            Some("46%")
        );
    }

    #[test]
    fn template_mixes_literals_and_metrics() {
        assert_eq!(
            resolve_template("{mem.used|bytes} / {mem.total|bytes}", &metrics()).as_deref(),
            Some("2.3 GB / 30.7 GB")
        );
    }

    #[test]
    fn missing_metric_yields_nothing_to_draw() {
        assert_eq!(resolve_template("nope.missing | percent", &metrics()), None);
    }

    #[test]
    fn text_metric_ignores_the_formatter() {
        assert_eq!(
            resolve_template("cpu.model", &metrics()).as_deref(),
            Some("Ryzen 5 5600G")
        );
    }

    #[test]
    fn template_keeps_literal_text_when_one_metric_is_missing() {
        let got = resolve_template("{cpu.usage|percent} / {nope|percent}", &metrics());
        assert_eq!(got.as_deref(), Some("46% / "));
    }

    #[test]
    fn colour_ramp_uses_the_first_metric_named() {
        assert_eq!(
            first_metric_name("{cpu.usage|percent} of {mem.total|bytes}").as_deref(),
            Some("cpu.usage")
        );
    }
}
