//! History segmentation and Steps/Braille rendering.

use super::{App, DIM, Sample, UTIL, plot_color, us_label};
use crate::config::{GraphStyle, View};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType};
use ratatui::{Frame, symbols};
use std::collections::VecDeque;

/// Reads one series from a history sample.
type SampleValue = fn(&Sample) -> Option<f64>;

/// Per-disk history: utilization and fill level on one 0-100% scale.
pub(super) fn draw_disk_graph(frame: &mut Frame<'_>, app: &App, name: &str, area: Rect) {
    if area.width < 12 || area.height < 3 {
        return;
    }
    let lang = app.lang();
    let axis = Style::default().fg(DIM);
    let label_width = 4;
    let buffer = frame.buffer_mut();
    buffer.set_stringn(area.x, area.y, "100%", 4, axis);
    buffer.set_stringn(area.x, area.bottom() - 1, "  0%", 4, axis);
    if area.height >= 7 {
        buffer.set_stringn(area.x, area.y + (area.height - 1) / 2, " 50%", 4, axis);
    }
    let usage_color = plot_color(app.settings.usage_color);
    // The legend sits in the axis column, so it never hides the curves.
    let mut legend_row = 1;
    for (enabled, label, color) in [
        (
            app.settings.view == View::Processes && app.settings.show_io_series,
            "IO",
            UTIL,
        ),
        (app.settings.show_usage_series, us_label(lang), usage_color),
    ] {
        if enabled && legend_row < area.height - 1 {
            buffer.set_stringn(
                area.x + 1,
                area.y + legend_row,
                label,
                2,
                Style::default().fg(color),
            );
            legend_row += 1;
        }
    }
    // A thin axis line separates the labels from the plot.
    for y in area.y..area.bottom() {
        buffer[(area.x + label_width, y)]
            .set_symbol("│")
            .set_style(axis);
    }
    let plot = Rect {
        x: area.x + label_width + 1,
        width: area.width - label_width - 1,
        ..area
    };
    let Some(history) = app.device_history(name) else {
        return;
    };
    let window = app.history_seconds();
    let now = history.back().map_or(0.0, |sample| sample.at);
    // Fill level first, so utilization stays visible where both meet.
    let series: Vec<(Color, SampleValue)> = [
        (
            app.settings.show_usage_series,
            usage_color,
            (|sample: &Sample| sample.used) as SampleValue,
        ),
        (
            app.settings.view == View::Processes && app.settings.show_io_series,
            UTIL,
            |sample: &Sample| sample.util,
        ),
    ]
    .into_iter()
    .filter(|(enabled, _, _)| *enabled)
    .map(|(_, color, value)| (color, value))
    .collect();
    let data: Vec<(Color, Vec<(f64, f64)>)> = series
        .iter()
        .flat_map(|(color, value)| {
            history_segments(history, *value)
                .into_iter()
                .map(|points| (*color, points))
        })
        .collect();
    if app.settings.graph_style == GraphStyle::Steps {
        for (color, points) in &data {
            let points = points.iter().filter_map(|(at, value)| {
                let progress = (at - now + window) / window;
                (0.0..=1.0)
                    .contains(&progress)
                    .then_some((progress, value / 100.0))
            });
            draw_steps(frame.buffer_mut(), plot, points, *color);
        }
        return;
    }
    let relative: Vec<Vec<(f64, f64)>> = data
        .iter()
        .map(|(_, points)| {
            points
                .iter()
                .map(|(at, value)| (at - now, *value))
                .collect()
        })
        .collect();
    let datasets = data
        .iter()
        .zip(&relative)
        .map(|((color, _), points)| {
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(*color))
                .data(points)
        })
        .collect();
    let chart = Chart::new(datasets)
        .x_axis(Axis::default().bounds([-window, 0.0]))
        .y_axis(Axis::default().bounds([0.0, 100.0]));
    frame.render_widget(chart, plot);
}

/// Missing measurements separate curves; they are never interpolated.
pub(super) fn history_segments(
    history: &VecDeque<Sample>,
    value: SampleValue,
) -> Vec<Vec<(f64, f64)>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for sample in history {
        match value(sample) {
            Some(value) => current.push((sample.at, value.clamp(0.0, 100.0))),
            None if !current.is_empty() => segments.push(std::mem::take(&mut current)),
            None => {}
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// nvtop-style stepped line from (progress 0..1, value 0..1) points, drawn
/// with terminal line characters rather than colored dots.
pub(super) fn draw_steps(
    buffer: &mut Buffer,
    plot: Rect,
    points: impl Iterator<Item = (f64, f64)>,
    color: Color,
) {
    if plot.width == 0 || plot.height == 0 {
        return;
    }
    let mut columns = vec![None; usize::from(plot.width)];
    for (progress, value) in points {
        let x = (progress * f64::from(plot.width - 1)).round() as usize;
        let y = ((1.0 - value.clamp(0.0, 1.0)) * f64::from(plot.height - 1)).round() as u16;
        let last = columns.len() - 1;
        columns[x.min(last)] = Some(y);
    }
    let mut paths = vec![0u8; usize::from(plot.width) * usize::from(plot.height)];
    let index = |x: u16, y: u16| usize::from(y) * usize::from(plot.width) + usize::from(x);
    let mut previous = None;
    for (x, y) in columns
        .into_iter()
        .enumerate()
        .filter_map(|(x, y)| y.map(|y| (x as u16, y)))
    {
        if let Some((px, py)) = previous {
            for column in px..x {
                paths[index(column, py)] |= 2;
                paths[index(column + 1, py)] |= 8;
            }
            for row in py.min(y)..py.max(y) {
                paths[index(x, row)] |= 4;
                paths[index(x, row + 1)] |= 1;
            }
        } else {
            paths[index(x, y)] = 10;
        }
        previous = Some((x, y));
    }
    for y in 0..plot.height {
        for x in 0..plot.width {
            let symbol = match paths[index(x, y)] {
                0 => continue,
                1 | 4 | 5 => "│",
                2 | 8 | 10 => "─",
                3 => "└",
                6 => "┌",
                9 => "┘",
                12 => "┐",
                7 => "├",
                11 => "┴",
                13 => "┤",
                14 => "┬",
                _ => "┼",
            };
            buffer[(plot.x + x, plot.y + y)]
                .set_symbol(symbol)
                .set_style(Style::default().fg(color));
        }
    }
}
