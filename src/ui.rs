//! Terminal state and frame composition; focused renderers live in `ui/`.

mod disks;
mod graphs;
mod input;
mod layout;
mod overlays;
mod state;
mod summary;
mod tables;

#[cfg(test)]
mod tests;

pub use crate::config::SortKey as Sort;
pub use layout::{BlockStyle, DiskLayout, disk_layout};

use crate::config::{Field, PlotColor, Settings, View};
use crate::i18n::Lang;
use crate::model::{HomeReport, Snapshot};
use crate::text::{Columns, truncate};
use disks::draw_disks;
use layout::{TABLE_MIN, disk_layout_for_view};
use overlays::{draw_devices, draw_help, draw_setup, draw_sort, draw_views};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, TableState};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use summary::summary_line;
use tables::{draw_filesystems, draw_home, draw_processes};

const KEY: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const UTIL: Color = Color::Cyan;
const SORTS: [Sort; 6] = [
    Sort::Io,
    Sort::Read,
    Sort::Write,
    Sort::Total,
    Sort::Pid,
    Sort::Name,
];
const CATEGORY_COUNT: usize = 5;
const VIEWS: [View; 3] = [View::Filesystems, View::Home, View::Processes];
const RESCAN_STEPS: [u16; 11] = [1, 2, 5, 10, 15, 30, 60, 120, 240, 720, 1440];
/// Smallest graph scale, so idle noise does not fill the whole graph.
const GRAPH_FLOOR: f64 = 64.0 * 1024.0;

fn categories(lang: Lang) -> [&'static str; CATEGORY_COUNT] {
    match lang {
        Lang::En => ["General", "Device", "Chart", "Table", "Home"],
        Lang::De => ["Allgemein", "Gerät", "Diagramm", "Tabelle", "Home"],
    }
}

fn inverse(color: Color) -> Style {
    // ncurses A_STANDOUT: reverse the terminal's own foreground/background.
    Style::default().fg(color).add_modifier(Modifier::REVERSED)
}

fn plot_color(color: PlotColor) -> Color {
    match color {
        PlotColor::Green => Color::Green,
        PlotColor::Yellow => Color::Yellow,
        PlotColor::Cyan => Color::Cyan,
        PlotColor::Red => Color::Red,
        PlotColor::Blue => Color::Blue,
        PlotColor::Magenta => Color::Magenta,
        PlotColor::White => Color::White,
    }
}

impl Sort {
    pub fn next(self) -> Self {
        let index = SORTS.iter().position(|sort| *sort == self).unwrap_or(0);
        SORTS[(index + 1) % SORTS.len()]
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Io => lang.pick("I/O rate", "I/O-Rate"),
            Self::Read => lang.pick("read", "Lesen"),
            Self::Write => lang.pick("write", "Schreiben"),
            Self::Total => lang.pick("total", "gesamt"),
            Self::Pid => "PID",
            Self::Name => lang.pick("command", "Befehl"),
        }
    }
}

fn view_label(view: View, lang: Lang) -> &'static str {
    match view {
        View::Processes => "I/O",
        View::Filesystems => lang.pick("Space", "Speicher"),
        View::Home => "Home",
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    None,
    Help,
    Devices,
    Setup,
    Sort,
    Views,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    DeviceChanged,
    SettingsChanged,
    SaveSettings,
    /// Home root, scan request or rescan: sample now.
    HomeChanged,
}

/// One refresh of one device, kept per device for its history graph.
#[derive(Clone, Debug)]
pub struct Sample {
    pub at: f64,
    pub read: Option<f64>,
    pub write: Option<f64>,
    /// Utilization in percent.
    pub util: Option<f64>,
    /// Fill level of the mounted filesystems in percent.
    pub used: Option<f64>,
}

/// One row of the home view: a retained directory or a summary.
#[derive(Clone, Debug, PartialEq)]
pub enum HomeEntryKind {
    Directory(u32),
    Files,
    Folded,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HomeEntry {
    pub kind: HomeEntryKind,
    pub name: String,
    pub size: u64,
    pub files: u64,
    pub dirs: u64,
    pub growth_previous: Option<i64>,
    pub growth_start: Option<i64>,
    pub partial: bool,
}

pub struct App {
    pub snapshot: Snapshot,
    /// Focused device; `None` focuses no disk and summarizes all of them.
    pub device: Option<String>,
    /// History per device name, for the overview graphs and meter scales.
    pub history: BTreeMap<String, VecDeque<Sample>>,
    /// Effective runtime settings, including CLI and environment overrides.
    pub settings: Settings,
    /// Settings as stored in the preferences file.
    saved_settings: Settings,
    /// Preferences the user changed in this session; only these are saved.
    touched: BTreeSet<Field>,
    /// Language used when the language preference is Auto.
    pub system_lang: Lang,
    pub auto_device: Option<String>,
    pub notice: Option<String>,
    pub notice_error: bool,
    pub paused: bool,
    pub filter: String,
    pub searching: bool,
    pub overlay: Overlay,
    pub table: TableState,
    picker: usize,
    help_scroll: u16,
    visible_rows: usize,
    pub demo: bool,
    setup_category: usize,
    setup_option: usize,
    setup_focus: bool,
    /// Home path being edited in Setup.
    editing: Option<String>,
    pub home_tree: Option<HomeReport>,
    known_generation: u64,
    /// Drill-down position below the home root, by sanitized names.
    pub home_cwd: Vec<String>,
    /// The home scanner is requested once the home view has been opened.
    pub home_active: bool,
    rescan_pending: bool,
    /// First grid row of the disk overview when not all disks fit.
    disk_scroll: usize,
    /// Layout of the last drawn overview, for tests and scrolling.
    pub disk_layout: Option<DiskLayout>,
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width < 36 || area.height < 16 {
        frame.render_widget(
            Paragraph::new(app.lang().pick(
                "dwtop\nTerminal too small.\nUse at least 36 x 16.\nq / F10 to quit.",
                "dwtop\nTerminal zu klein.\nMindestens 36 x 16.\nq / F10 beendet.",
            ))
            .style(Style::default().fg(if app.settings.color {
                KEY
            } else {
                Color::Reset
            })),
            area,
        );
        return;
    }
    let status = status_line(app);
    let status_rows = u16::from(status.is_some());
    // Title, summary and footer rows; the rest is disks and the lower table.
    let body = area.height - 3 - status_rows;
    let rows = u16::from(app.settings.process_rows);
    let reserved = match app.settings.view {
        View::Processes if rows == 0 => 0,
        // Small terminals keep at most three process rows for the disks.
        View::Processes if body < 20 => 1 + rows.min(3),
        View::Processes => 1 + rows,
        // A short filesystem list must not crowd out fill history.
        View::Filesystems => ((body / 2).max(TABLE_MIN)).min(
            app.filesystem_rows()
                .len()
                .max(1)
                .saturating_add(1)
                .min(u16::MAX as usize) as u16,
        ),
        _ => (body / 2).max(TABLE_MIN),
    }
    .min(body.saturating_sub(2));
    let count = app.overview_devices().len();
    let graphs = app.settings.show_graph
        && (app.settings.show_usage_series
            || (app.settings.view == View::Processes && app.settings.show_io_series));
    let mut layout = if app.settings.view == View::Processes {
        disk_layout(area.width, body - reserved, count, graphs)
    } else {
        disk_layout_for_view(area.width, body - reserved, count, graphs, true)
    };
    if count == 0 {
        layout.visible_rows = 0;
    }
    let disk_height = if count == 0 { 1 } else { layout.height() };
    // Keep the focused disk on screen when not all disks fit.
    let columns = usize::from(layout.columns.max(1));
    let max_scroll = layout.grid_rows.saturating_sub(layout.visible_rows);
    if let Some(index) = app
        .overview_devices()
        .iter()
        .position(|device| Some(device.name.as_str()) == app.device.as_deref())
    {
        let row = index / columns;
        if row < app.disk_scroll {
            app.disk_scroll = row;
        } else if row >= app.disk_scroll + layout.visible_rows {
            app.disk_scroll = row + 1 - layout.visible_rows.max(1);
        }
    }
    app.disk_scroll = app.disk_scroll.min(max_scroll);
    app.disk_layout = Some(layout);
    let lower = body - disk_height;
    let layout_rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(status_rows),
        Constraint::Length(disk_height),
        Constraint::Length(lower),
        Constraint::Length(1),
    ])
    .split(area);
    draw_title(frame, app, layout_rows[0]);
    frame.render_widget(
        // Default foreground keeps the summary readable on light and dark themes.
        Paragraph::new(summary_line(app, area.width)),
        layout_rows[1],
    );
    if let Some((text, color)) = status {
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(color)),
            layout_rows[2],
        );
    }
    draw_disks(frame, app, layout_rows[3], layout);
    if lower > 0 && (app.settings.view != View::Processes || rows > 0) {
        match app.settings.view {
            View::Processes => draw_processes(frame, app, layout_rows[4]),
            View::Filesystems => draw_filesystems(frame, app, layout_rows[4]),
            View::Home => draw_home(frame, app, layout_rows[4]),
        }
    } else {
        app.visible_rows = 0;
    }
    draw_footer(frame, app, layout_rows[5]);
    match app.overlay {
        Overlay::Help => draw_help(frame, app, area),
        Overlay::Devices => draw_devices(frame, app, area),
        Overlay::Setup => draw_setup(frame, app, area),
        Overlay::Sort => draw_sort(frame, app, area),
        Overlay::Views => draw_views(frame, app, area),
        Overlay::None => {}
    }
    if !app.settings.color {
        for cell in &mut frame.buffer_mut().content {
            cell.set_fg(Color::Reset).set_bg(Color::Reset);
        }
    }
}

fn draw_title(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lang = app.lang();
    let count = app.overview_devices().len();
    let mut spans = vec![
        Span::styled("Disks ", Style::default().fg(Color::Green)),
        Span::raw(count.to_string()),
        Span::styled(
            lang.pick("  Focus ", "  Fokus "),
            Style::default().fg(Color::Green),
        ),
        Span::raw("["),
        Span::raw(truncate(
            app.device.as_deref().unwrap_or(lang.pick("all", "alle")),
            20,
        )),
        Span::raw("]"),
    ];
    if app.demo {
        spans.push(Span::styled("  DEMO", Style::default().fg(Color::Magenta)));
    }
    if app.paused {
        spans.push(Span::styled(
            lang.pick("  PAUSED", "  PAUSIERT"),
            Style::default().fg(Color::Yellow),
        ));
    }
    let used: usize = spans.iter().map(|span| span.content.columns()).sum();
    // View tabs on the right; narrow terminals show only the current view.
    let views = VIEWS;
    let tabs_width: usize = views
        .iter()
        .map(|view| view_label(*view, lang).columns() + 3)
        .sum();
    let mut right = Vec::new();
    if usize::from(area.width) >= used + tabs_width + 2 {
        for (index, view) in views.iter().enumerate() {
            let label = format!("{}{}", index + 1, view_label(*view, lang));
            right.push(Span::raw(" "));
            if *view == app.settings.view {
                right.push(Span::styled(label, inverse(KEY)));
            } else {
                right.push(Span::raw(label));
            }
        }
    } else {
        let label = view_label(app.settings.view, lang);
        if usize::from(area.width) >= used + label.columns() + 2 {
            right.push(Span::styled(label.to_owned(), inverse(KEY)));
        }
    }
    let right_width: u16 = right.iter().map(|span| span.content.columns() as u16).sum();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    if right_width > 0 && right_width < area.width {
        frame.render_widget(
            Paragraph::new(Line::from(right)),
            Rect::new(area.right() - right_width, area.y, right_width, 1),
        );
    }
}

fn rd_label(lang: Lang) -> &'static str {
    lang.pick("RD", "LE")
}

fn wr_label(lang: Lang) -> &'static str {
    lang.pick("WR", "SC")
}

fn us_label(lang: Lang) -> &'static str {
    lang.pick("US", "BL")
}

/// Join pieces with two spaces while they fit; the first piece is always kept.
fn status_line(app: &App) -> Option<(String, Color)> {
    let lang = app.lang();
    if let Some(notice) = &app.notice {
        return Some((
            notice.clone(),
            if app.notice_error { Color::Red } else { KEY },
        ));
    }
    if app.searching {
        return Some((
            match lang {
                Lang::En => format!("Search: {}_  Enter apply / Esc clear", app.filter),
                Lang::De => format!("Suche: {}_  Enter übernimmt / Esc leert", app.filter),
            },
            KEY,
        ));
    }
    if !app.filter.is_empty() {
        return Some((
            format!(
                "Filter: {}  {}",
                app.filter,
                lang.pick("Esc clears", "Esc leert")
            ),
            KEY,
        ));
    }
    if !app.snapshot.truncated.is_empty() {
        return Some((lang.truncated(&app.snapshot.truncated), Color::Yellow));
    }
    let unavailable_pools: BTreeSet<_> = app
        .snapshot
        .filesystems
        .iter()
        .filter_map(|filesystem| filesystem.zfs.as_ref())
        .filter(|pool| pool.error.is_some())
        .map(|pool| crate::text::sanitize(&pool.name))
        .collect();
    if !unavailable_pools.is_empty() {
        return Some((
            format!(
                "{}: {}",
                lang.pick("ZFS pool data unavailable", "ZFS-Pooldaten nicht verfügbar"),
                unavailable_pools.into_iter().collect::<Vec<_>>().join(", ")
            ),
            Color::Yellow,
        ));
    }
    if app.demo || app.settings.view != View::Processes {
        return None; // DEMO is labeled in the title; other views show their own state.
    }
    let access = &app.snapshot.process_access;
    if access.message.contains("/proc hidepid") {
        return Some((
            lang.pick(
                "hidepid: process details restricted (F1)",
                "hidepid: Prozessdetails beschränkt (F1)",
            )
            .to_owned(),
            Color::Yellow,
        ));
    }
    if !access.privileged && access.unreadable > 0 {
        return Some((
            match lang {
                Lang::En => format!(
                    "{} processes of other users hidden | all: scripts/setup-helper.sh",
                    access.unreadable
                ),
                Lang::De => format!(
                    "{} Prozesse anderer Benutzer verborgen | alle: scripts/setup-helper.sh",
                    access.unreadable
                ),
            },
            Color::Yellow,
        ));
    }
    None
}

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lang = app.lang();
    // German labels are abbreviated like Midnight Commander's on narrow
    // terminals, so every required key fits at 36 columns.
    let wide = area.width >= 60;
    let mut keys = vec![
        ("F1", lang.pick("Help", "Hilfe")),
        ("F2", if area.width < 40 { "Set" } else { "Setup" }),
    ];
    if area.width >= 66 {
        keys.push(("F3", lang.pick("Search", "Suche")));
    }
    keys.push(("F4", lang.pick("View", "Sicht")));
    if area.width >= 80 {
        keys.push(("F5", lang.pick("Focus", "Fokus")));
    }
    if app.settings.view == View::Processes && area.width >= 44 {
        keys.push((
            "F6",
            if wide {
                lang.pick("Sort", "Sortieren")
            } else {
                lang.pick("Sort", "Sort")
            },
        ));
    }
    if area.width >= 94 {
        keys.push((
            "F9",
            if app.paused {
                lang.pick("Resume", "Weiter")
            } else {
                lang.pick("Pause", "Pause")
            },
        ));
    }
    keys.push(("F10", lang.pick("Quit", "Ende")));
    keys.push((
        "F12",
        if wide {
            lang.pick("SaveConfig", "Speichern")
        } else {
            lang.pick("Save", "Speich")
        },
    ));
    draw_key_bar(frame, area, &keys);
}

fn draw_key_bar(frame: &mut Frame<'_>, area: Rect, keys: &[(&str, &str)]) {
    let used: usize = keys
        .iter()
        .map(|(key, label)| key.columns() + label.columns())
        .sum();
    let padding = usize::from(area.width).saturating_sub(used);
    let mut spans = Vec::new();
    for (index, (key, label)) in keys.iter().enumerate() {
        let spaces = padding / keys.len() + usize::from(index < padding % keys.len());
        spans.push(Span::raw((*key).to_owned()));
        spans.push(Span::styled(
            format!("{label}{}", " ".repeat(spaces)),
            inverse(KEY),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
