// SPDX-License-Identifier: GPL-3.0-or-later
//! Turns a [`Theme`] plus a [`Metrics`] table into pixels.
//!
//! Layout is a vertical flow: sections stack from the top (or the bottom, for
//! anchored ones) and rows stack inside them. Nothing in a theme names a pixel
//! coordinate, so the same theme works on any panel size.

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use turing_lcd::canvas::{Canvas, Rect};
use turing_lcd::widgets::{outline, progress_bar, sparkline};
use turing_lcd::{Align, Font, Image, Rgb, VAlign};

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
    /// Width of each column; equal shares unless the theme weighted them.
    widths: Vec<u16>,
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
        let Some(avail) = content.checked_sub(gaps).filter(|w| *w >= count) else {
            return Columns {
                count: 1,
                x: vec![margin],
                widths: vec![content],
                full_x: margin,
                full_width: content,
            };
        };
        // `column_widths` are relative shares, so a theme stays independent of
        // the panel size. Without them every column gets the same slice.
        let weights: Vec<u16> = match theme.column_widths.len() == count as usize {
            true => theme.column_widths.clone(),
            false => vec![1; count as usize],
        };
        let total: u32 = weights.iter().map(|w| *w as u32).sum::<u32>().max(1);
        let widths: Vec<u16> = weights
            .iter()
            .map(|w| ((avail as u32 * *w as u32 / total) as u16).max(1))
            .collect();
        let mut x = Vec::with_capacity(count as usize);
        let mut cursor = margin;
        for w in &widths {
            x.push(cursor);
            cursor += w + theme.column_gap;
        }
        Columns {
            count: count as usize,
            x,
            widths,
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
        (self.x[c], self.widths[c], c..c + 1)
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
    /// The cover, already resampled to the box it is drawn in. Kept until the
    /// track or the layout changes, so a frame is a memcpy and nothing else.
    art: Option<(Rc<Image>, u16, Image)>,
    /// Index of the framed section drawn in the theme's `focus_color`.
    focus: Option<usize>,
}

/// How much busier another section must be to take the focus away, so two
/// tiles at about the same load do not trade it every frame.
const FOCUS_HYSTERESIS: f32 = 0.05;

/// Every row leaves this much space under itself.
const ROW_TRAIL: u16 = 4;

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
            art: None,
            focus: None,
        })
    }

    pub fn render(&mut self, canvas: &mut Canvas, theme: &Theme, m: &Metrics) -> RenderReport {
        let mut report = RenderReport::default();
        self.record_history(theme, m);
        self.pick_focus(theme, m);
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
        let mut used = margin;

        // Bottom-anchored sections are placed first so the top flows know
        // where they have to stop.
        for (i, sec) in theme.sections.iter().enumerate().rev() {
            if sec.anchor != Anchor::Bottom || !self.section_visible(sec, m) {
                continue;
            }
            let (x, w, span) = cols.place(sec);
            let h = self.tile(canvas, theme, i, m, x, 0, w, None);
            // A spanning section has to clear the lowest of the columns it covers.
            let y = span
                .clone()
                .map(|c| bottom[c])
                .min()
                .unwrap_or(margin)
                .saturating_sub(h);
            self.tile(canvas, theme, i, m, x, y, w, Some(h));
            for c in span {
                bottom[c] = y.saturating_sub(sec.gap);
            }
        }

        for (i, sec) in theme.sections.iter().enumerate() {
            if sec.anchor != Anchor::Top || !self.section_visible(sec, m) {
                continue;
            }
            let (x, w, span) = cols.place(sec);
            let mut h = self.tile(canvas, theme, i, m, x, 0, w, None);
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
            if sec.grow {
                h = floor - y;
            }
            self.tile(canvas, theme, i, m, x, y, w, Some(h));
            used = used.max(y + h);
            for c in span {
                top[c] = y + h + sec.gap;
            }
        }

        // The report speaks for the tightest column, which is the one that
        // decides whether the theme fits.
        // Measured to the last pixel drawn: the gap after the last section is
        // not space the theme needs.
        report.used_height = used.saturating_sub(margin);
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

    /// The framed section that gets `focus_color`: the busiest one, like the
    /// window with focus on the desktop. A section at zero load never takes it.
    fn pick_focus(&mut self, theme: &Theme, m: &Metrics) {
        if theme.focus_color.is_none() {
            self.focus = None;
            return;
        }
        let loads: Vec<(usize, f32)> = theme
            .sections
            .iter()
            .enumerate()
            .filter(|(_, s)| s.frame && self.section_visible(s, m))
            .map(|(i, s)| (i, self.section_load(s, m)))
            .filter(|(_, l)| *l > 0.0)
            .collect();
        let best = loads.iter().copied().reduce(|a, b| if b.1 > a.1 { b } else { a });
        let held = self.focus.and_then(|f| loads.iter().copied().find(|(i, _)| *i == f));
        self.focus = match (held, best) {
            (Some(h), Some(b)) if h.1 + FOCUS_HYSTERESIS >= b.1 => Some(h.0),
            (_, b) => b.map(|b| b.0),
        };
    }

    /// The 0..=1 figure a section's colour ramp and its claim on the focus
    /// react to.
    fn section_load(&self, sec: &Section, m: &Metrics) -> f32 {
        sec.color_from
            .as_ref()
            .and_then(|k| m.num(k).map(|v| v as f32))
            .or_else(|| sec.value.as_ref().and_then(|v| self.load_of(&v.template, m)))
            .unwrap_or(0.0)
    }

    /// A section inside its frame, when it has one. `stretch` is `None` to
    /// only measure, or the height to draw it at, if that is taller.
    fn tile(
        &mut self,
        canvas: &mut Canvas,
        theme: &Theme,
        i: usize,
        m: &Metrics,
        x: u16,
        y: u16,
        w: u16,
        stretch: Option<u16>,
    ) -> u16 {
        let sec = &theme.sections[i];
        let draw = stretch.is_some();
        if !sec.frame {
            return self.section(canvas, sec, m, x, y, w, draw);
        }
        let inset = theme.frame_width + sec.padding;
        let inner = self.section(canvas, sec, m, x + inset, y + inset, w.saturating_sub(2 * inset), draw);
        // The last row's trailing space would stack on the bottom padding and
        // leave it deeper than the top one.
        let h = (inner.saturating_sub(ROW_TRAIL) + 2 * inset).max(stretch.unwrap_or(0));
        if draw {
            let color = match theme.focus_color {
                Some(c) if self.focus == Some(i) => c,
                _ => theme.frame_color,
            };
            outline(canvas, Rect::new(x, y, w, h), theme.frame_width, color);
        }
        h
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
                let load = self.section_load(sec, m);
                let color = sec.color.resolve(load);
                if let Some(title) = &sec.title {
                    let title = resolve_label(title, m);
                    self.bold.draw(
                        canvas,
                        &title,
                        x as i32,
                        cursor as i32,
                        sec.title_size,
                        self.text_color(Style::Bold, sec.title_color.as_ref(), load, color),
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
                // Only the left item wraps; every line past the first adds its
                // own height below the row.
                let extra = match left {
                    Some(t) if t.lines > 1 => {
                        let n = self.lines_of(t, m, content).len().max(1) as u16;
                        (n - 1) * (t.size + 4)
                    }
                    _ => 0,
                };
                let h = left
                    .as_ref()
                    .map(|t| t.size)
                    .max(right.as_ref().map(|t| t.size))
                    .unwrap_or(11)
                    + 4
                    + extra;
                if draw {
                    if let Some(t) = left {
                        self.draw_text_item(canvas, t, m, sec, x as i32, y, Align::Left, content);
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
                            content,
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

            Row::Art { metric, max } => {
                // Cuadrada y a lo ancho de su columna: una caratula no es una
                // medida, es el disco. El hueco se reserva aunque no haya
                // imagen todavia, para que el resto no baile.
                let side = max.unwrap_or(content).min(content);
                if draw {
                    if let Some(img) = m.image(metric) {
                        let scaled = self.art_for(img, side);
                        canvas.blit_image(x, y, scaled);
                    }
                }
                side + 4
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
                        // Centred: the rounding remainder splits between the
                        // two sides instead of piling up on the right.
                        let x = x + content.saturating_sub(n * bw + gap * (n - 1)) / 2;
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

    /// The cover resampled to `side`, recomputed only when the track changes
    /// (a different `Rc`) or the theme gives it a different box.
    fn art_for(&mut self, img: &Rc<Image>, side: u16) -> &Image {
        let stale = match &self.art {
            Some((src, cached, _)) => !Rc::ptr_eq(src, img) || *cached != side,
            None => true,
        };
        if stale {
            self.art = Some((img.clone(), side, img.scaled(side, side)));
        }
        &self.art.as_ref().expect("just filled").2
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
        avail: u16,
    ) {
        let lines = self.lines_of(item, m, avail);
        if lines.is_empty() {
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
        for (i, line) in lines.iter().enumerate() {
            let y = y as i32 + i as i32 * (item.size as i32 + 4);
            font.draw(canvas, line, x, y, item.size, color, align, VAlign::Top);
        }
    }

    /// What a text item shows, already cut to fit `avail`: one line, or up to
    /// `lines` of them. Empty when there is nothing to draw.
    fn lines_of(&mut self, item: &TextItem, m: &Metrics, avail: u16) -> Vec<String> {
        let Some(text) = resolve_template(&item.template, m) else {
            return Vec::new();
        };
        if text.trim().is_empty() {
            return Vec::new();
        }
        let font = match item.style {
            Style::Bold => &mut self.bold,
            _ => &mut self.regular,
        };
        wrap(font, &text, item.size, avail, item.lines)
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

/// Cut a line down to what its column can actually show. Without this a long
/// value — the title of a song, the model of a CPU — runs off the edge of the
/// panel and gets sliced mid-glyph.
///
/// ponytail: mide una vez por caracter sobrante, O(n^2) en el peor caso. Solo
/// entra cuando el texto ya no cabe, y ahi n son dos docenas de letras.
fn ellipsize(font: &mut Font, text: &str, size: u16, avail: u16) -> String {
    let avail = avail as f32;
    if avail <= 0.0 || font.measure(text, size) <= avail {
        return text.to_string();
    }
    let mut s = text.to_string();
    while !s.is_empty() && font.measure(&format!("{}\u{2026}", s.trim_end()), size) > avail {
        s.pop();
    }
    format!("{}\u{2026}", s.trim_end())
}

/// Break `text` at spaces into at most `max` lines that fit `avail`; whatever
/// is left over goes on the last one, cut with an ellipsis.
fn wrap(font: &mut Font, text: &str, size: u16, avail: u16, max: u16) -> Vec<String> {
    if max <= 1 {
        return vec![ellipsize(font, text, size, avail)];
    }
    let mut lines = Vec::new();
    let mut words = text.split_whitespace().peekable();
    while lines.len() + 1 < max as usize {
        let mut line = String::new();
        while let Some(w) = words.peek() {
            let next = match line.is_empty() {
                true => w.to_string(),
                false => format!("{line} {w}"),
            };
            // A word wider than the line still goes on its own and is cut below.
            if !line.is_empty() && font.measure(&next, size) > avail as f32 {
                break;
            }
            line = next;
            words.next();
        }
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    let rest: Vec<&str> = words.collect();
    if !rest.is_empty() {
        lines.push(rest.join(" "));
    }
    lines.into_iter().map(|l| ellipsize(font, &l, size, avail)).collect()
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
        // Ni una serie ni una imagen son texto: las dibuja su propia fila.
        Value::Series(_) | Value::Image(_) => None,
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
                Row::Bar { metric, .. }
                | Row::Cores { metric, .. }
                | Row::Art { metric, .. } => out.push(metric.clone()),
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
            column_widths: Vec::new(),
            frame_width: 2,
            frame_color: [0x2c, 0x2c, 0x2c],
            focus_color: None,
            sections: Vec::new(),
        }
    }

    fn section(column: usize, span: bool) -> Section {
        Section {
            title: None,
            title_size: 15,
            title_color: None,
            value: None,
            color: ColorSpec::Fixed([255, 255, 255]),
            color_from: None,
            require: None,
            anchor: Anchor::Top,
            column,
            span,
            gap: 6,
            frame: false,
            padding: 8,
            grow: false,
            rows: Vec::new(),
        }
    }

    #[test]
    fn focus_goes_to_the_busiest_frame_and_holds_through_small_swings() {
        let mut t = theme_with(1, 10);
        t.focus_color = Some([0xb8, 0x45, 0x5a]);
        for key in ["a", "b"] {
            let mut s = section(0, false);
            s.frame = true;
            s.color_from = Some(key.into());
            t.sections.push(s);
        }
        let mut r = Renderer::new(None, None, 480).unwrap();
        let mut canvas = Canvas::new(480, 320, [0, 0, 0]);
        let mut frame = |r: &mut Renderer, a: f64, b: f64| {
            let mut m = Metrics::new();
            m.set_num("a", a);
            m.set_num("b", b);
            r.render(&mut canvas, &t, &m);
            r.focus
        };
        assert_eq!(frame(&mut r, 0.5, 0.3), Some(0));
        // Apenas por encima: el foco no salta.
        assert_eq!(frame(&mut r, 0.5, 0.53), Some(0));
        assert_eq!(frame(&mut r, 0.5, 0.7), Some(1));
        // Nada cargado, nadie con foco.
        assert_eq!(frame(&mut r, 0.0, 0.0), None);
    }

    #[test]
    fn a_growing_frame_reaches_down_to_the_section_below() {
        let mut t = theme_with(1, 10);
        let mut top = section(0, false);
        top.frame = true;
        top.grow = true;
        let mut foot = section(0, false);
        foot.anchor = Anchor::Bottom;
        foot.rows.push(RowEntry { require: None, row: Row::Gap(20) });
        t.sections = vec![top, foot];
        let mut r = Renderer::new(None, None, 480).unwrap();
        let mut canvas = Canvas::new(480, 320, [0, 0, 0]);
        r.render(&mut canvas, &t, &Metrics::new());
        // El pie ocupa 292..312 y deja 6 de hueco: el marco baja hasta 286.
        let px = |y: u16| {
            let i = (y as usize * 480 + 8) * 3;
            canvas.bytes()[i..i + 3].to_vec()
        };
        assert_eq!(px(285), vec![0x2c, 0x2c, 0x2c]);
        assert_eq!(px(286), vec![0, 0, 0]);
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
    fn weights_split_the_content_unevenly() {
        let mut t = theme_with(2, 14);
        t.column_widths = vec![2, 3];
        let cols = Columns::new(&t, 10, 460);
        // 460 - 14 de hueco = 446, repartido 2:3.
        assert_eq!(cols.place(&section(0, false)), (10, 178, 0..1));
        assert_eq!(cols.place(&section(1, false)), (202, 267, 1..2));
    }

    #[test]
    fn a_gap_wider_than_the_panel_falls_back_to_one_column() {
        // Better a cramped single column than an underflowed width.
        let cols = Columns::new(&theme_with(2, 600), 8, 464);
        assert_eq!(cols.count, 1);
        assert_eq!(cols.place(&section(1, false)), (8, 464, 0..1));
    }

    #[test]
    fn long_text_is_cut_to_the_column_instead_of_overflowing() {
        let mut r = Renderer::new(None, None, 480).unwrap();
        let long = "Un titulo absurdamente largo que no cabe ni de lejos en el panel";
        let cut = ellipsize(&mut r.regular, long, 32, 460);
        assert!(cut.ends_with('\u{2026}'), "should end in an ellipsis: {cut}");
        assert!(r.regular.measure(&cut, 32) <= 460.0, "still too wide: {cut}");
        // Lo que ya cabe se deja tal cual, sin puntos suspensivos.
        assert_eq!(ellipsize(&mut r.regular, "corto", 32, 460), "corto");
    }

    #[test]
    fn wrapped_text_breaks_at_spaces_and_cuts_only_the_last_line() {
        let mut r = Renderer::new(None, None, 480).unwrap();
        let long = "Un titulo absurdamente largo que no cabe ni de lejos en el panel";
        let lines = wrap(&mut r.regular, long, 20, 200, 3);
        assert_eq!(lines.len(), 3);
        for l in &lines {
            assert!(r.regular.measure(l, 20) <= 200.0, "too wide: {l}");
        }
        assert!(!lines[0].ends_with('\u{2026}') && lines[2].ends_with('\u{2026}'));
        // Lo que cabe en una linea se queda en una.
        assert_eq!(wrap(&mut r.regular, "Kid A", 20, 200, 3), vec!["Kid A"]);
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

