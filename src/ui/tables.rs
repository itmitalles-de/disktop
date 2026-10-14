//! Process, filesystem and home-directory tables.

use super::disks::bar;
use super::{App, DIM, HomeEntry, HomeEntryKind, KEY, inverse, plot_color};
use crate::i18n::Lang;
use crate::model::{Filesystem, ScanState, StopReason};
use crate::text::{
    Columns, format_bytes, format_bytes_short, format_count, format_delta, format_duration,
    format_rate, format_short_count, truncate, truncate_left,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};

pub(super) fn number_cell(text: String) -> Cell<'static> {
    Cell::from(Line::from(text).alignment(Alignment::Right))
}

pub(super) fn header_row(headers: Vec<(&str, bool)>) -> Row<'_> {
    Row::new(headers.into_iter().map(|(header, numeric)| {
        if numeric {
            number_cell(header.into())
        } else {
            Cell::from(header)
        }
    }))
    .style(inverse(Color::Green))
}

pub(super) fn empty_message(
    frame: &mut Frame<'_>,
    area: Rect,
    offset: u16,
    message: &str,
    color: Color,
) {
    if area.height > offset {
        frame.render_widget(
            Paragraph::new(message.to_owned()).style(Style::default().fg(color)),
            Rect::new(area.x, area.y + offset, area.width, 1),
        );
    }
}

pub(super) fn draw_processes(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.visible_rows = usize::from(area.height.saturating_sub(1));
    let wide = area.width >= 94;
    let medium = area.width >= 52;
    let lang = app.lang();
    let read_color = Style::default().fg(plot_color(app.settings.read_color));
    let write_color = Style::default().fg(plot_color(app.settings.write_color));
    // The column header carries /s; cells keep the whole unit.
    let rate = |value: Option<f64>| {
        value.map_or_else(
            || "-".to_owned(),
            |value| format_rate(value).trim_end_matches("/s").to_owned(),
        )
    };
    let read_header = lang.pick("READ/s", "LESEN/s");
    let write_header = lang.pick("WRITE/s", "SCHREIB/s");
    let command = lang.pick("COMMAND", "BEFEHL");
    let records = app.process_rows();
    let count = records.len();
    let (headers, widths, rows): (Vec<(&str, bool)>, Vec<Constraint>, Vec<Row<'_>>) = if wide {
        (
            vec![
                ("PID", true),
                (lang.pick("USER", "BENUTZER"), false),
                (read_header, true),
                (write_header, true),
                (lang.pick("CANCEL/s", "VERWORF/s"), true),
                (lang.pick("TOTAL", "GESAMT"), true),
                (command, false),
            ],
            vec![
                Constraint::Length(7),
                Constraint::Length(10),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(11),
                Constraint::Length(11),
                Constraint::Min(8),
            ],
            records
                .into_iter()
                .map(|row| {
                    Row::new(vec![
                        number_cell(row.pid.to_string()),
                        Cell::from(truncate(&row.user, 10)),
                        number_cell(rate(row.read_rate)).style(read_color),
                        number_cell(rate(row.write_rate)).style(write_color),
                        number_cell(rate(row.cancelled_write_rate)),
                        number_cell(format_bytes(
                            row.read_bytes.saturating_add(row.write_bytes) as f64
                        )),
                        Cell::from(row.command.clone()),
                    ])
                })
                .collect(),
        )
    } else if medium {
        let rate_width = if area.width >= 64 { 12 } else { 10 };
        (
            vec![
                ("PID", true),
                (lang.pick("USER", "BENUTZER"), false),
                (read_header, true),
                (write_header, true),
                (command, false),
            ],
            vec![
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Length(rate_width),
                Constraint::Length(rate_width),
                Constraint::Min(6),
            ],
            records
                .into_iter()
                .map(|row| {
                    Row::new(vec![
                        number_cell(row.pid.to_string()),
                        Cell::from(truncate(&row.user, 8)),
                        number_cell(rate(row.read_rate)).style(read_color),
                        number_cell(rate(row.write_rate)).style(write_color),
                        Cell::from(row.command.clone()),
                    ])
                })
                .collect(),
        )
    } else {
        (
            vec![
                ("PID", true),
                (read_header, true),
                (write_header, true),
                (command, false),
            ],
            vec![
                Constraint::Length(7),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Min(5),
            ],
            records
                .into_iter()
                .map(|row| {
                    Row::new(vec![
                        number_cell(row.pid.to_string()),
                        number_cell(rate(row.read_rate)).style(read_color),
                        number_cell(rate(row.write_rate)).style(write_color),
                        Cell::from(row.name.clone()),
                    ])
                })
                .collect(),
        )
    };
    let table = Table::new(rows, widths)
        .header(header_row(headers))
        .column_spacing(1)
        .row_highlight_style(inverse(KEY));
    frame.render_stateful_widget(table, area, &mut app.table);
    if count == 0 {
        let message = if !app.filter.is_empty() {
            lang.pick("No matching processes.", "Keine passenden Prozesse.")
        } else if app.snapshot.processes.is_empty()
            && !app.snapshot.process_access.message.is_empty()
        {
            app.snapshot.process_access.message.as_str()
        } else {
            lang.pick(
                "No process I/O in this interval (F2 Table: show idle rows).",
                "Keine Prozess-I/O in diesem Intervall (F2 Tabelle: inaktive zeigen).",
            )
        };
        empty_message(frame, area, 1, message, DIM);
    }
}

pub(super) fn use_color(percent: f64) -> Color {
    if percent >= 90.0 {
        Color::Red
    } else if percent >= 75.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

pub(super) fn optional_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format_bytes(value as f64))
}

pub(super) fn draw_filesystems(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.visible_rows = usize::from(area.height.saturating_sub(1));
    let wide = area.width >= 94;
    let medium = area.width >= 52;
    let lang = app.lang();
    let records = app.filesystem_rows();
    let count = records.len();
    let mount_header = lang.pick("MOUNT", "EINHÄNGEPUNKT");
    let free_header = lang.pick("FREE", "FREI");
    let size_header = lang.pick("USED", "BELEGT");
    let percent =
        |value: Option<f64>| value.map_or_else(|| "-".to_owned(), |value| format!("{value:.0}%"));
    let mount = |row: &Filesystem| {
        format!(
            "{}{}{}",
            if row.contains_home { "*" } else { " " },
            row.mount_point,
            if row.not_responding {
                format!("  {}", lang.not_responding())
            } else {
                String::new()
            }
        )
    };
    let style = |row: &Filesystem| {
        if row.not_responding {
            Style::default().fg(Color::Yellow)
        } else if row.contains_home {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    };
    let (headers, widths, rows): (Vec<(&str, bool)>, Vec<Constraint>, Vec<Row<'_>>) =
        if wide {
            (
                vec![
                    (mount_header, false),
                    (lang.pick("DEVICE", "GERÄT"), false),
                    (lang.pick("TYPE", "TYP"), false),
                    (lang.pick("USE", "BELEGT"), false),
                    (size_header, true),
                    (free_header, true),
                    ("INODES", true),
                ],
                vec![
                    Constraint::Min(12),
                    Constraint::Length(14),
                    Constraint::Length(6),
                    Constraint::Length(17),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(6),
                ],
                records
                    .into_iter()
                    .map(|row| {
                        let used = row.use_percent();
                        let mut usage = bar(
                            used.unwrap_or(0.0) / 100.0,
                            10,
                            use_color(used.unwrap_or(0.0)),
                        );
                        usage.push(Span::raw(format!("{:>5}", percent(used))));
                        Row::new(vec![
                            Cell::from(mount(row)),
                            Cell::from(truncate(
                                row.block_device.as_deref().unwrap_or(&row.source),
                                14,
                            )),
                            Cell::from(truncate(&row.fs_type, 6)),
                            Cell::from(Line::from(usage)),
                            number_cell(optional_bytes(row.used_bytes)),
                            number_cell(optional_bytes(row.available_bytes)),
                            number_cell(percent(row.inode_percent())),
                        ])
                        .style(style(row))
                    })
                    .collect(),
            )
        } else if medium {
            (
                vec![
                    (mount_header, false),
                    (lang.pick("USE", "BEL."), true),
                    (size_header, true),
                    (free_header, true),
                    (lang.pick("TYPE", "TYP"), false),
                ],
                vec![
                    Constraint::Min(10),
                    Constraint::Length(5),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(6),
                ],
                records
                    .into_iter()
                    .map(|row| {
                        let used = row.use_percent();
                        Row::new(vec![
                            Cell::from(mount(row)),
                            number_cell(percent(used))
                                .style(Style::default().fg(use_color(used.unwrap_or(0.0)))),
                            number_cell(optional_bytes(row.used_bytes)),
                            number_cell(optional_bytes(row.available_bytes)),
                            Cell::from(truncate(&row.fs_type, 6)),
                        ])
                        .style(style(row))
                    })
                    .collect(),
            )
        } else {
            (
                vec![
                    (mount_header, false),
                    (lang.pick("USE", "BEL."), true),
                    (size_header, true),
                    (free_header, true),
                ],
                vec![
                    Constraint::Min(10),
                    Constraint::Length(5),
                    Constraint::Length(8),
                    Constraint::Length(8),
                ],
                records
                    .into_iter()
                    .map(|row| {
                        let used = row.use_percent();
                        Row::new(vec![
                            Cell::from(mount(row)),
                            number_cell(percent(used))
                                .style(Style::default().fg(use_color(used.unwrap_or(0.0)))),
                            number_cell(row.used_bytes.map_or_else(
                                || "-".into(),
                                |value| format_bytes_short(value as f64),
                            )),
                            number_cell(row.available_bytes.map_or_else(
                                || "-".into(),
                                |value| format_bytes_short(value as f64),
                            )),
                        ])
                        .style(style(row))
                    })
                    .collect(),
            )
        };
    let table = Table::new(rows, widths)
        .header(header_row(headers))
        .column_spacing(1)
        .row_highlight_style(inverse(KEY));
    frame.render_stateful_widget(table, area, &mut app.table);
    if count == 0 {
        let message = if app.filter.is_empty() {
            lang.pick(
                "No storage filesystems mounted.",
                "Keine Speicher-Dateisysteme eingehängt.",
            )
        } else {
            lang.pick("No matching filesystems.", "Keine passenden Dateisysteme.")
        };
        empty_message(frame, area, 1, message, DIM);
    }
}

/// One line describing the home scan: position, totals and scan state.
pub(super) fn home_header(app: &App, width: u16) -> (String, Color) {
    let lang = app.lang();
    let status = app.snapshot.home.as_ref();
    let root = status.map_or(app.settings.home_path.as_str(), |status| {
        status.root.as_str()
    });
    let mut path = root.to_owned();
    let mut text = String::new();
    let mut color = Color::Reset;
    if let Some((node, cwd)) = app.home_node() {
        for name in cwd {
            if !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(name);
        }
        let size = if app.settings.apparent_size {
            node.apparent
        } else {
            node.allocated
        };
        text.push_str(&format!("  {}", format_bytes(size as f64)));
        if app.settings.apparent_size {
            text.push_str(lang.pick(" apparent", " scheinbar"));
        }
        if width >= 60 {
            text.push_str(&format!(
                "  {} {}",
                format_count(node.files),
                lang.pick("files", "Dateien")
            ));
        }
        if node.partial() {
            text.push_str(lang.pick("  PARTIAL", "  UNVOLLSTÄNDIG"));
            color = Color::Yellow;
        }
    }
    if let Some(tree) = &app.home_tree {
        if let Some(reason) = tree.stopped {
            text.push_str(match (reason, lang) {
                (StopReason::EntryBudget, Lang::En) => "  entry limit reached",
                (StopReason::EntryBudget, Lang::De) => "  Eintragslimit erreicht",
                (StopReason::TimeBudget, Lang::En) => "  time limit reached",
                (StopReason::TimeBudget, Lang::De) => "  Zeitlimit erreicht",
                (StopReason::Cancelled, Lang::En) => "  cancelled",
                (StopReason::Cancelled, Lang::De) => "  abgebrochen",
            });
            color = Color::Yellow;
        }
        if tree.hardlink_limit_reached {
            text.push_str(lang.pick("  hard links may count twice", "  Hardlinks evtl. doppelt"));
        }
    }
    match status {
        Some(status) if status.state == ScanState::Scanning => {
            text.push_str(&match lang {
                Lang::En => format!(
                    "  scanning {} entries {}",
                    format_short_count(status.progress_entries as f64),
                    format_duration(status.progress_seconds as u64)
                ),
                Lang::De => format!(
                    "  scanne {} Einträge {}",
                    format_short_count(status.progress_entries as f64),
                    format_duration(status.progress_seconds as u64)
                ),
            });
            color = KEY;
        }
        Some(status) if status.state == ScanState::Failed => {
            text.push_str("  ");
            match (&status.error, status.error_kind) {
                (Some(error), Some(kind)) => {
                    text.push_str(&lang.home_error(kind, &status.root, error));
                }
                (Some(error), None) => text.push_str(error),
                (None, _) => text.push_str(lang.pick("scan failed", "Scan fehlgeschlagen")),
            }
            color = Color::Red;
        }
        Some(status) if status.state == ScanState::Done && width >= 80 => {
            if let Some(tree) = &app.home_tree {
                text.push_str(&match lang {
                    Lang::En => format!("  scan {}", format_duration(tree.duration_ms / 1000)),
                    Lang::De => format!("  Scan {}", format_duration(tree.duration_ms / 1000)),
                });
            }
            if let Some(next) = status.next_scan_in_seconds {
                text.push_str(&match lang {
                    Lang::En => format!(", next in {}", format_duration(next)),
                    Lang::De => format!(", nächster in {}", format_duration(next)),
                });
            }
        }
        _ => {}
    }
    if status.is_some_and(|status| status.helper_refused) {
        text.push_str("  ");
        text.push_str(&lang.helper_refused());
        if color == Color::Reset {
            color = Color::Yellow;
        }
    }
    if width >= 100 {
        text.push_str(lang.pick("  r rescan", "  r neu scannen"));
    }
    let path_width = usize::from(width).saturating_sub(text.columns()).max(8);
    (format!("{}{text}", truncate_left(&path, path_width)), color)
}

pub(super) fn draw_home(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let lang = app.lang();
    let (header, color) = home_header(app, area.width);
    frame.render_widget(
        Paragraph::new(truncate(&header, usize::from(area.width)))
            .style(Style::default().fg(color)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    app.visible_rows = usize::from(area.height.saturating_sub(1));
    let wide = area.width >= 94;
    let medium = area.width >= 60;
    let entries = app.home_entries();
    let parent_size = app
        .home_node()
        .map(|(node, _)| {
            if app.settings.apparent_size {
                node.apparent
            } else {
                node.allocated
            }
        })
        .unwrap_or(0)
        .max(1);
    let name = |entry: &HomeEntry| match entry.kind {
        HomeEntryKind::Directory(_) => {
            format!("{}/{}", entry.name, if entry.partial { " !" } else { "" })
        }
        HomeEntryKind::Files => match lang {
            Lang::En => format!("[{} files here]", format_count(entry.files)),
            Lang::De => format!("[{} Dateien hier]", format_count(entry.files)),
        },
        HomeEntryKind::Folded => match lang {
            Lang::En => format!("[{} smaller dirs]", format_count(entry.dirs)),
            Lang::De => format!("[{} kleinere Verz.]", format_count(entry.dirs)),
        },
    };
    let style = |entry: &HomeEntry| {
        if entry.partial {
            Style::default().fg(Color::Yellow)
        } else if matches!(entry.kind, HomeEntryKind::Directory(_)) {
            Style::default()
        } else {
            // Summary rows cannot be opened.
            Style::default().fg(DIM)
        }
    };
    let delta = |value: Option<i64>| value.map_or_else(|| "-".to_owned(), format_delta);
    let share = |entry: &HomeEntry, width: usize| {
        let ratio = entry.size as f64 / parent_size as f64;
        let mut spans = bar(ratio, width, Color::Green);
        spans.push(Span::raw(format!("{:>5.1}%", (ratio * 100.0).min(100.0))));
        Line::from(spans)
    };
    let name_header = lang.pick("NAME", "NAME");
    let size_header = if app.settings.apparent_size {
        lang.pick("APPARENT", "SCHEINBAR")
    } else {
        lang.pick("SIZE", "GRÖSSE")
    };
    let files_header = lang.pick("FILES", "DATEIEN");
    let previous_header = if app.settings.apparent_size {
        lang.pick("+PREV alloc", "+LETZT bel")
    } else {
        lang.pick("+PREV", "+LETZTER")
    };
    let start_header = if app.settings.apparent_size {
        lang.pick("+START blk", "+START bel")
    } else {
        "+START"
    };
    let (headers, widths, rows): (Vec<(&str, bool)>, Vec<Constraint>, Vec<Row<'_>>) = if wide {
        (
            vec![
                (name_header, false),
                (size_header, true),
                (lang.pick("SHARE", "ANTEIL"), false),
                (files_header, true),
                (previous_header, true),
                (start_header, true),
            ],
            vec![
                Constraint::Min(16),
                Constraint::Length(10),
                Constraint::Length(20),
                Constraint::Length(11),
                Constraint::Length(11),
                Constraint::Length(11),
            ],
            entries
                .iter()
                .map(|entry| {
                    Row::new(vec![
                        Cell::from(name(entry)),
                        number_cell(format_bytes(entry.size as f64)),
                        Cell::from(share(entry, 12)),
                        number_cell(format_count(entry.files)),
                        number_cell(delta(entry.growth_previous)),
                        number_cell(delta(entry.growth_start)),
                    ])
                    .style(style(entry))
                })
                .collect(),
        )
    } else if medium {
        (
            vec![
                (name_header, false),
                (size_header, true),
                (lang.pick("SHARE", "ANTEIL"), false),
                (previous_header, true),
            ],
            vec![
                Constraint::Min(12),
                Constraint::Length(10),
                Constraint::Length(14),
                Constraint::Length(11),
            ],
            entries
                .iter()
                .map(|entry| {
                    Row::new(vec![
                        Cell::from(name(entry)),
                        number_cell(format_bytes(entry.size as f64)),
                        Cell::from(share(entry, 6)),
                        number_cell(delta(entry.growth_previous)),
                    ])
                    .style(style(entry))
                })
                .collect(),
        )
    } else {
        (
            vec![(size_header, true), ("%", true), (name_header, false)],
            vec![
                Constraint::Length(10),
                Constraint::Length(4),
                Constraint::Min(8),
            ],
            entries
                .iter()
                .map(|entry| {
                    Row::new(vec![
                        number_cell(format_bytes(entry.size as f64)),
                        number_cell(format!(
                            "{:.0}",
                            (entry.size as f64 / parent_size as f64 * 100.0).min(100.0)
                        )),
                        Cell::from(name(entry)),
                    ])
                    .style(style(entry))
                })
                .collect(),
        )
    };
    let count = rows.len();
    let table = Table::new(rows, widths)
        .header(header_row(headers))
        .column_spacing(1)
        .row_highlight_style(inverse(KEY));
    frame.render_stateful_widget(table, area, &mut app.table);
    if count == 0 {
        let state = app.snapshot.home.as_ref().map(|status| status.state);
        let message = if !app.filter.is_empty() {
            lang.pick("No matching directories.", "Keine passenden Verzeichnisse.")
        } else if app.home_tree.is_some() {
            lang.pick("Empty directory.", "Leeres Verzeichnis.")
        } else if state == Some(ScanState::Scanning) {
            lang.pick(
                "Scanning in the background with idle I/O priority...",
                "Scan läuft im Hintergrund mit Leerlauf-I/O-Priorität...",
            )
        } else if state == Some(ScanState::Failed) {
            lang.pick(
                "Scan failed; F2 Home changes the path.",
                "Scan fehlgeschlagen; F2 Home ändert den Pfad.",
            )
        } else {
            lang.pick(
                "Waiting for the first scan...",
                "Warte auf den ersten Scan...",
            )
        };
        empty_message(frame, area, 1, message, DIM);
    }
}
