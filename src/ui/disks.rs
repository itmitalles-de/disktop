//! Per-device labels, capacity and I/O meters.

use super::graphs::draw_disk_graph;
use super::layout::{BlockStyle, DiskLayout, column_width, side_meter_width};
use super::tables::empty_message;
use super::{App, DIM, GRAPH_FLOOR, KEY, UTIL, inverse, plot_color, rd_label, us_label, wr_label};
use crate::config::View;
use crate::i18n::Lang;
use crate::model::Device;
use crate::text::{
    Columns, format_bytes, format_bytes_short, format_bytes_tiny, format_rate, pad, truncate,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub(super) fn bar(fill_ratio: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let fill = (fill_ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    vec![
        Span::raw("["),
        Span::styled("|".repeat(fill), Style::default().fg(color)),
        Span::styled(
            " ".repeat(width.saturating_sub(fill)),
            Style::default().fg(DIM),
        ),
        Span::raw("]"),
    ]
}

/// One htop-style meter: `RD[|||||     12.3M/s]`.
pub(super) struct Meter {
    label: &'static str,
    /// Single-letter label for one-row disks on narrow terminals.
    short: &'static str,
    color: Color,
    pub(super) ratio: f64,
    /// Value texts from longest to shortest; the longest that fits is shown.
    values: Vec<String>,
}

impl Meter {
    pub(super) fn spans(&self, width: u16, short_label: bool) -> Vec<Span<'static>> {
        let label = if short_label { self.short } else { self.label };
        let inner = usize::from(width).saturating_sub(label.columns() + 2);
        let mut spans = vec![Span::styled(label, Style::default().fg(self.color))];
        if usize::from(width) < label.columns() + 2 {
            return spans;
        }
        let value = self
            .values
            .iter()
            .find(|value| value.columns() + 2 <= inner)
            .or_else(|| self.values.iter().find(|value| value.columns() <= inner))
            .cloned()
            .unwrap_or_else(|| truncate(self.values.last().map_or("", String::as_str), inner));
        let text_start = inner - value.columns();
        let fill = ((self.ratio.clamp(0.0, 1.0) * inner as f64).round() as usize).min(text_start);
        spans.push(Span::raw("["));
        spans.push(Span::styled(
            "|".repeat(fill),
            Style::default().fg(self.color),
        ));
        spans.push(Span::raw(" ".repeat(text_start - fill)));
        // Default foreground keeps values readable on light and dark themes.
        spans.push(Span::raw(value));
        spans.push(Span::raw("]"));
        spans
    }
}

pub(super) fn rate_values(rate: Option<f64>) -> Vec<String> {
    match rate {
        Some(rate) => vec![
            format_rate(rate),
            format!("{}/s", format_bytes_short(rate)),
            format_bytes_short(rate),
            format_bytes_tiny(rate),
        ],
        None => vec!["-".to_owned()],
    }
}

/// The full scale of every RD and WR meter: the highest read or write rate
/// of any device in the overview within the visible history, so equal bar
/// lengths mean equal rates on every disk.
pub(super) fn rate_scale(app: &App) -> f64 {
    app.overview_devices()
        .into_iter()
        .filter_map(|device| app.device_history(&device.name))
        .flatten()
        .flat_map(|sample| [sample.read, sample.write])
        .flatten()
        .fold(GRAPH_FLOOR, f64::max)
}

/// The RD, WR, IO and US meters of one device. RD and WR use `peak`, the
/// shared scale of all disks from `rate_scale`.
pub(super) fn disk_meters(app: &App, device: &Device, peak: f64) -> [Meter; 4] {
    let lang = app.lang();
    let rates = device.rates.as_ref();
    let read = rates.map(|rates| rates.read_rate);
    let write = rates.map(|rates| rates.write_rate);
    let util = rates.map(|rates| rates.utilization_percent.clamp(0.0, 100.0));
    let usage = device.usage.as_ref();
    let percent = usage.and_then(|usage| usage.percent());
    let usage_values = match (usage, percent) {
        (Some(usage), Some(percent)) => {
            let total = usage.used_bytes.saturating_add(usage.available_bytes) as f64;
            let used = usage.used_bytes as f64;
            let partial = if usage.unmeasured > 0 { "!" } else { "" };
            vec![
                format!(
                    "{percent:.0}%{partial} {}/{}",
                    format_bytes(used),
                    format_bytes(total)
                ),
                format!(
                    "{percent:.0}%{partial} {}/{}",
                    format_bytes_short(used),
                    format_bytes_short(total)
                ),
                format!("{percent:.0}%{partial}"),
            ]
        }
        (Some(usage), None) if usage.filesystems == 0 && usage.unmeasured == 0 => {
            if usage.swap {
                vec![
                    lang.pick("swap only", "nur Swap").to_owned(),
                    "swap".to_owned(),
                ]
            } else {
                // "-" would mean "not measured", so the shortest text differs.
                vec![
                    lang.pick("not mounted", "nicht eingehängt").to_owned(),
                    lang.pick("no fs", "kein FS").to_owned(),
                    lang.pick("none", "ohne").to_owned(),
                ]
            }
        }
        _ => vec!["-".to_owned()],
    };
    [
        Meter {
            label: rd_label(lang),
            short: lang.pick("R", "L"),
            color: plot_color(app.settings.read_color),
            ratio: read.map_or(0.0, |rate| rate / peak),
            values: rate_values(read),
        },
        Meter {
            label: wr_label(lang),
            short: lang.pick("W", "S"),
            color: plot_color(app.settings.write_color),
            ratio: write.map_or(0.0, |rate| rate / peak),
            values: rate_values(write),
        },
        Meter {
            label: "IO",
            short: "I",
            color: UTIL,
            ratio: util.map_or(0.0, |util| util / 100.0),
            values: util.map_or_else(
                || vec!["-".to_owned()],
                |util| vec![format!("{util:.1}%"), format!("{util:.0}%")],
            ),
        },
        Meter {
            label: us_label(lang),
            short: lang.pick("U", "B"),
            // Nearly full filesystems turn the meter red.
            color: if percent.is_some_and(|percent| percent >= 90.0) {
                Color::Red
            } else {
                plot_color(app.settings.usage_color)
            },
            ratio: percent.map_or(0.0, |percent| percent / 100.0),
            values: usage_values,
        },
    ]
}

/// The label row of a disk block: name, type, model, size and mounts.
pub(super) fn disk_label(app: &App, device: &Device, width: u16, focused: bool) -> Line<'static> {
    let lang = app.lang();
    let mut kind = device.kind.label().to_owned();
    if device.partition
        && let Some(parent) = &device.parent
    {
        kind.push_str(&match lang {
            Lang::En => format!(" partition of {parent}"),
            Lang::De => format!(" Partition von {parent}"),
        });
    }
    if device.removable {
        kind.push_str(lang.pick(" removable", " wechselbar"));
    }
    let identity = [
        device.vendor.as_deref(),
        device.model.as_deref(),
        device.label.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    let mut details = format!(" [{kind}]");
    if let Some(size) = device.size_bytes {
        details.push_str(&format!(" {}", format_bytes(size as f64)));
    }
    let width = usize::from(width);
    let name = truncate(&device.name, width);
    let details = truncate(&details, width - name.columns());
    let mut room = width - name.columns() - details.columns();
    // Mounts before the model: they tell what the disk is used for. Mounts
    // that do not fit are counted as "+N".
    let mut mounts = String::new();
    for (index, mount) in device.mounts.iter().enumerate() {
        let rest = device.mounts.len() - index - 1;
        let more = if rest > 0 {
            format!(" +{rest}")
        } else {
            String::new()
        };
        let separator = if index == 0 { "  " } else { " " };
        if mounts.columns() + separator.len() + mount.columns() + more.len() <= room {
            mounts.push_str(separator);
            mounts.push_str(mount);
        } else {
            let more = format!(
                "{}+{}",
                if index == 0 { "  " } else { " " },
                device.mounts.len() - index
            );
            if mounts.columns() + more.len() <= room {
                mounts.push_str(&more);
            }
            break;
        }
    }
    room -= mounts.columns();
    let identity = if identity.is_empty() || room < 4 {
        String::new()
    } else {
        truncate(&format!("  {identity}"), room)
    };
    if focused {
        // The focused disk is shown in reverse video across the whole row.
        let text = format!("{name}{details}{mounts}{identity}");
        let used = text.columns();
        return Line::from(vec![Span::styled(
            format!("{text}{}", " ".repeat(width - used)),
            inverse(KEY).add_modifier(Modifier::BOLD),
        )]);
    }
    Line::from(vec![
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(details),
        Span::raw(mounts),
        Span::styled(identity, Style::default().fg(DIM)),
    ])
}

pub(super) fn draw_disks(frame: &mut Frame<'_>, app: &App, area: Rect, layout: DiskLayout) {
    let lang = app.lang();
    let devices = app.overview_devices();
    if devices.is_empty() {
        empty_message(
            frame,
            area,
            0,
            lang.pick(
                "No block devices to show (F2 Device: loop, ram, zram).",
                "Keine Blockgeräte (F2 Gerät: loop, ram, zram).",
            ),
            DIM,
        );
        return;
    }
    let columns = layout.columns.max(1);
    let width = column_width(area.width, columns);
    let height = layout.style.height();
    let first = app.disk_scroll * usize::from(columns);
    let shown = layout.visible_rows * usize::from(columns);
    let scale = rate_scale(app);
    // Row style: one shared name width keeps the meters aligned.
    let name_width = devices
        .iter()
        .map(|device| device.name.columns())
        .max()
        .unwrap_or(4)
        .clamp(4, if width < 44 { 7 } else { 12 }) as u16;
    for (slot, device) in devices.iter().skip(first).take(shown).enumerate() {
        let column = slot as u16 % columns;
        let row = (slot / usize::from(columns)) as u16;
        // Distribute the remainder of the width to the first columns.
        let extra = (area.width + 1) - (width + 1) * columns;
        let x = area.x + column * (width + 1) + column.min(extra);
        let block = Rect::new(
            x,
            area.y + row * height,
            width + u16::from(column < extra),
            height,
        );
        let focused = app.device.as_deref() == Some(device.name.as_str());
        draw_disk(
            frame,
            app,
            device,
            block,
            layout.style,
            focused,
            name_width,
            scale,
        );
    }
    if layout.overflow() {
        let hidden = devices.len().saturating_sub(shown);
        let above = first;
        let below = hidden - above.min(hidden);
        let arrows = format!(
            "{}{}",
            if above > 0 {
                format!(" ↑{above}")
            } else {
                String::new()
            },
            if below > 0 {
                format!(" ↓{below}")
            } else {
                String::new()
            }
        );
        let text = match lang {
            Lang::En => format!("+{hidden} more disks{arrows}  Tab / F5 focus scrolls"),
            Lang::De => format!("+{hidden} weitere Disks{arrows}  Tab / F5 Fokus blättert"),
        };
        frame.render_widget(
            Paragraph::new(truncate(&text, usize::from(area.width)))
                .style(Style::default().fg(Color::Yellow)),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_disk(
    frame: &mut Frame<'_>,
    app: &App,
    device: &Device,
    area: Rect,
    style: BlockStyle,
    focused: bool,
    name_width: u16,
    scale: f64,
) {
    let meters = disk_meters(app, device, scale);
    let line = |y: u16, x: u16, width: u16| Rect::new(x, area.y + y, width, 1);
    if app.settings.view != View::Processes {
        if style == BlockStyle::Row {
            let name_width = name_width.min(area.width.saturating_sub(1));
            frame.render_widget(
                Paragraph::new(pad(
                    &truncate(&device.name, usize::from(name_width)),
                    usize::from(name_width),
                ))
                .style(if focused {
                    inverse(KEY)
                } else {
                    Style::default()
                }),
                line(0, area.x, name_width),
            );
            frame_meter(
                frame,
                &meters[3],
                line(
                    0,
                    area.x + name_width + 1,
                    area.width.saturating_sub(name_width + 1),
                ),
            );
            return;
        }
        frame.render_widget(
            Paragraph::new(disk_label(app, device, area.width, focused)),
            line(0, area.x, area.width),
        );
        frame_meter(frame, &meters[3], line(1, area.x, area.width));
        if area.height >= 3 {
            let free = device
                .usage
                .as_ref()
                .filter(|usage| usage.percent().is_some())
                .map_or_else(
                    || "-".to_owned(),
                    |usage| format_bytes(usage.available_bytes as f64),
                );
            frame.render_widget(
                Paragraph::new(format!("{} {free}", app.lang().pick("Free", "Frei"))),
                line(2, area.x, area.width),
            );
        }
        if style.has_graph() && area.height >= 6 {
            draw_disk_graph(
                frame,
                app,
                &device.name,
                Rect::new(area.x, area.y + 3, area.width, area.height - 3),
            );
        }
        return;
    }
    if style == BlockStyle::Row {
        let name_width = name_width.min(area.width.saturating_sub(1));
        let name = truncate(&device.name, usize::from(name_width));
        let name_style = if focused {
            inverse(KEY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        frame.render_widget(
            Paragraph::new(pad(&name, usize::from(name_width))).style(name_style),
            line(0, area.x, name_width),
        );
        let rest = area.width.saturating_sub(name_width + 1);
        let each = rest / 4;
        let short = each < 10;
        // Brackets separate the meters; a gap is added when there is room.
        let gap = u16::from(each >= 9);
        let mut x = area.x + name_width + 1;
        for (index, meter) in meters.iter().enumerate() {
            let width = each + u16::from((index as u16) < rest % 4);
            frame.render_widget(
                Paragraph::new(Line::from(meter.spans(width - gap, short))),
                line(0, x, width),
            );
            x += width;
        }
        return;
    }
    frame.render_widget(
        Paragraph::new(disk_label(app, device, area.width, focused)),
        line(0, area.x, area.width),
    );
    match style {
        BlockStyle::Side(graph) => {
            let width = side_meter_width(area.width);
            for (index, meter) in meters.iter().enumerate() {
                frame_meter(frame, meter, line(1 + index as u16, area.x, width));
            }
            draw_disk_graph(
                frame,
                app,
                &device.name,
                Rect::new(
                    area.x + width + 1,
                    area.y + 1,
                    area.width - width - 1,
                    graph.max(4),
                ),
            );
        }
        BlockStyle::Below(graph) => {
            for (index, meter) in meters.iter().enumerate() {
                frame_meter(frame, meter, line(1 + index as u16, area.x, area.width));
            }
            draw_disk_graph(
                frame,
                app,
                &device.name,
                Rect::new(area.x, area.y + 5, area.width, graph),
            );
        }
        BlockStyle::Meters => {
            for (index, meter) in meters.iter().enumerate() {
                frame_meter(frame, meter, line(1 + index as u16, area.x, area.width));
            }
        }
        _ => {
            let left = (area.width - 1) / 2;
            let right = area.width - left - 1;
            for (index, meter) in meters.iter().enumerate() {
                let row = 1 + index as u16 / 2;
                if index % 2 == 0 {
                    frame_meter(frame, meter, line(row, area.x, left));
                } else {
                    frame_meter(frame, meter, line(row, area.x + left + 1, right));
                }
            }
        }
    }
}

pub(super) fn frame_meter(frame: &mut Frame<'_>, meter: &Meter, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(meter.spans(area.width, false))),
        area,
    );
}
