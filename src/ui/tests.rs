use super::disks::disk_meters;
use super::disks::rate_scale;
use super::graphs::draw_disk_graph;
use super::layout::BlockStyle;
use super::layout::disk_layout;
use super::summary::combined_usage;
use super::summary::summary_line;
use super::*;
use crate::config::GraphStyle;
use crate::config::Language;
use crate::config::PlotColor;
use crate::config::Settings;
use crate::config::View;
use crate::i18n::Lang;
use crate::model::Device;
use crate::model::DeviceRates;
use crate::model::DirNode;
use crate::model::Filesystem;
use crate::model::HomeReport;
use crate::model::Snapshot;
use crate::{demo, model::DeviceKind};
use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::{Terminal, backend::TestBackend};
use std::collections::BTreeSet;
use std::collections::VecDeque;

fn app() -> App {
    let mut app = App::new(
        Some("nvme0n1".into()),
        Settings {
            view: View::Processes,
            ..Settings::default()
        },
        true,
    );
    for tick in 0..61 {
        let known = app.take_home_request().known_generation;
        app.update(demo::snapshot(tick, tick as f64, "/home", known));
    }
    app
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let width = usize::from(buffer.area.width);
    buffer
        .content
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>() + "\n")
        .collect()
}

#[test]
fn collector_clock_restart_drops_old_history_and_rate_scale() {
    const MIB: f64 = 1024.0 * 1024.0;
    let mut app = App::new(
        None,
        Settings {
            history_seconds: 10,
            ..Settings::default()
        },
        false,
    );
    let mut old = demo::snapshot(10, 3600.0, "/home", 0);
    for device in &mut old.devices {
        if let Some(rates) = &mut device.rates {
            rates.read_rate = 1024.0 * MIB;
            rates.write_rate = 0.0;
        }
    }
    app.update(old);
    assert_eq!(rate_scale(&app), 1024.0 * MIB);
    app.home_source_changed();
    assert!(app.history.is_empty());
    for tick in 1..=30 {
        let mut snapshot = demo::snapshot(10, f64::from(tick), "/home", 0);
        for device in &mut snapshot.devices {
            if let Some(rates) = &mut device.rates {
                rates.read_rate = MIB;
                rates.write_rate = 0.0;
            }
        }
        app.update(snapshot);
    }
    assert_eq!(rate_scale(&app), MIB);
    assert!(
        app.history
            .values()
            .all(|history| history.len() == 11 && history.front().unwrap().at == 20.0)
    );
    // Also recover when a clock reset arrives without a source notification.
    app.update(demo::snapshot(0, 0.0, "/home", 0));
    assert!(app.history.values().all(|history| history.len() == 1));
}

#[test]
fn missing_measurements_leave_visible_gaps_in_both_graph_styles() {
    for style in [GraphStyle::Steps, GraphStyle::Braille] {
        let mut app = App::new(
            None,
            Settings {
                graph_style: style,
                history_seconds: 10,
                ..Settings::default()
            },
            false,
        );
        let history: VecDeque<_> = (0..=9)
            .map(|tick| Sample {
                at: f64::from(tick),
                read: None,
                write: None,
                util: Some(50.0),
                used: match tick {
                    0 => Some(90.0),
                    9 => Some(10.0),
                    _ => None,
                },
            })
            .collect();
        app.history.insert("disk".into(), history);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| draw_disk_graph(frame, &app, "disk", Rect::new(0, 0, 80, 12)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let usage = plot_color(app.settings.usage_color);
        assert!(
            (10..80)
                .any(|x| (0..12)
                    .any(|y| buffer[(x, y)].fg == usage && buffer[(x, y)].symbol() != " ")),
            "{style:?}: valid endpoints missing"
        );
        assert!(
            (30..55)
                .all(|x| (0..12)
                    .all(|y| buffer[(x, y)].fg != usage || buffer[(x, y)].symbol() == " ")),
            "{style:?}: interpolated missing fill level"
        );
        // A gap in fill level must not break independently measured I/O.
        app.settings.view = View::Processes;
        terminal
            .draw(|frame| draw_disk_graph(frame, &app, "disk", Rect::new(0, 0, 80, 12)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            (30..55)
                .all(|x| (0..12)
                    .any(|y| buffer[(x, y)].fg == UTIL && buffer[(x, y)].symbol() != " ")),
            "{style:?}: measured I/O lost"
        );
    }
}

#[test]
fn apparent_home_sizes_label_growth_as_allocated_in_both_languages() {
    for language in [crate::config::Language::En, crate::config::Language::De] {
        for width in [60, 120] {
            let mut app = app();
            app.settings.language = language;
            app.settings.view = View::Home;
            app.settings.apparent_size = true;
            let before: Vec<_> = app
                .home_entries()
                .iter()
                .map(|entry| entry.growth_previous)
                .collect();
            let mut terminal = Terminal::new(TestBackend::new(width, 36)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let text = screen(&terminal);
            let label = if language == crate::config::Language::En {
                "+PREV alloc"
            } else {
                "+LETZT bel"
            };
            assert!(text.contains(label), "{language:?}/{width}: {text}");
            app.settings.apparent_size = false;
            assert_eq!(
                before,
                app.home_entries()
                    .iter()
                    .map(|entry| entry.growth_previous)
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn space_is_default_and_io_is_an_explicit_reversible_view() {
    for (width, height) in [(36, 16), (40, 24), (80, 24), (120, 36), (200, 60)] {
        for language in [Language::En, Language::De] {
            let mut app = App::new(
                Some("nvme0n1".into()),
                Settings {
                    language,
                    ..Settings::default()
                },
                true,
            );
            app.update(demo::snapshot(1, 1.0, "/home", 0));
            assert_eq!(app.settings.view, View::Filesystems);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let text = screen(&terminal);
            for forbidden in [
                "RD[", "WR[", "IO[", "LE[", "SC[", "READ/s", "WRITE/s", "IOPS", "COMMAND", "BEFEHL",
            ] {
                assert!(
                    !text.contains(forbidden),
                    "{forbidden} on default screen {width}x{height}: {text}"
                );
            }
            assert!(text.contains("/home"), "{text}");
            assert!(text.contains(app.lang().pick("FREE", "FREI")), "{text}");
            assert!(text.contains("F4"), "{text}");
            app.handle_key(KeyCode::F(4).into());
            assert_eq!(app.overlay, Overlay::Views);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            assert!(screen(&terminal).contains("3 I/O"));
            app.handle_key(KeyCode::Down.into());
            app.handle_key(KeyCode::Down.into());
            app.handle_key(KeyCode::Enter.into());
            assert_eq!(app.settings.view, View::Processes);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            assert!(screen(&terminal).contains(app.lang().pick("READ/s", "LESEN/s")));
            app.handle_key(KeyCode::Esc.into());
            assert_eq!(app.settings.view, View::Filesystems);
            app.handle_key(KeyCode::Char('i').into());
            assert_eq!(app.settings.view, View::Processes);
            app.handle_key(KeyCode::Char('1').into());
            assert_eq!(app.settings.view, View::Filesystems);
        }
    }
}

#[test]
fn space_capacity_never_substitutes_all_for_an_unavailable_focused_disk() {
    let mut app = app();
    app.settings.view = View::Filesystems;
    app.snapshot
        .devices
        .iter_mut()
        .find(|device| device.name == "nvme0n1")
        .unwrap()
        .usage = None;
    assert!(summary_line(&app, 120).contains("unavailable"));
    app.device = Some("sdb".into());
    assert_eq!(summary_line(&app, 120), "not mounted");
    app.device = Some("nvme1n1".into());
    app.snapshot
        .devices
        .iter_mut()
        .find(|device| device.name == "nvme1n1")
        .unwrap()
        .usage
        .as_mut()
        .unwrap()
        .unmeasured = 1;
    assert!(summary_line(&app, 120).contains("partial"));
    app.device = None;
    assert!(summary_line(&app, 120).contains("partial"));
}

const SIZES: [(u16, u16); 8] = [
    (36, 16),
    (40, 24),
    (52, 18),
    (60, 24),
    (80, 24),
    (120, 36),
    (160, 48),
    (200, 60),
];
const DISK_COUNTS: [usize; 5] = [1, 2, 4, 8, 16];

/// An app whose overview has `count` disks with varied demo load and fill
/// levels, after a full history of samples.
fn app_with_disks(count: usize) -> App {
    let names: Vec<String> = (0..count)
        .map(|index| format!("sd{}", char::from(b'a' + index as u8)))
        .collect();
    let mut app = App::new(Some(names[0].clone()), Settings::default(), true);
    for tick in 0..61 {
        let known = app.take_home_request().known_generation;
        let mut snapshot = demo::snapshot(tick, tick as f64, "/home", known);
        let templates: Vec<Device> = snapshot
            .devices
            .iter()
            .filter(|device| !device.partition && !device.kind.hidden_by_default())
            .cloned()
            .collect();
        snapshot.devices = names
            .iter()
            .enumerate()
            .map(|(index, name)| Device {
                name: name.clone(),
                ..templates[index % templates.len()].clone()
            })
            .collect();
        app.update(snapshot);
    }
    app
}

/// Disk names that start a label row or a one-row disk on screen.
fn shown_disks(text: &str, names: &[String]) -> usize {
    names
        .iter()
        .filter(|name| {
            text.lines().any(|line| {
                line.split_whitespace().any(|word| word == name.as_str())
                    || line.contains(&format!(" {name} ["))
                    || line.starts_with(&format!("{name} "))
            })
        })
        .count()
}

#[test]
fn short_space_table_leaves_room_for_both_disk_histories() {
    for language in [Language::En, Language::De] {
        let mut app = app_with_disks(2);
        app.settings.view = View::Filesystems;
        app.settings.language = language;
        app.snapshot.filesystems.truncate(4);
        assert_eq!(app.filesystem_rows().len(), 4);
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        // Real regression: two capacity blocks, both with visible history,
        // and the complete short table above the footer at 60x24.
        assert_eq!(text.matches("100%").count(), 2, "{text}");
        assert_eq!(text.matches("  0%").count(), 2, "{text}");
        for filesystem in app.filesystem_rows() {
            assert!(text.contains(&filesystem.mount_point), "{text}");
        }
        let lines: Vec<_> = text.lines().collect();
        assert!(!lines[22].trim().is_empty(), "unused table tail\n{text}");
        assert!(lines[23].contains("F10"), "{text}");
        assert!(
            terminal.backend().buffer().content.iter().any(|cell| {
                cell.fg == plot_color(app.settings.usage_color) && cell.symbol() == "─"
            }),
            "fill history not drawn\n{text}"
        );

        // Explicit graph preferences remain authoritative. Each compact
        // capacity block follows immediately after the preceding Free row.
        app.settings.show_graph = false;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(!text.contains("100%"), "{text}");
        let lines: Vec<_> = text.lines().collect();
        assert!(
            lines[4].starts_with(app.lang().pick("Free", "Frei")),
            "{text}"
        );
        assert!(lines[5].starts_with("sdb "), "{text}");
    }
}

#[test]
fn space_table_keeps_scroll_room_and_empty_filter_message() {
    let mut app = app_with_disks(2);
    app.settings.view = View::Filesystems;
    let template = app.snapshot.filesystems[0].clone();
    app.snapshot.filesystems = (0..30)
        .map(|index| crate::model::Filesystem {
            mount_point: format!("/mount/{index:02}"),
            ..template.clone()
        })
        .collect();
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(app.visible_rows >= 9);
    for _ in 0..30 {
        app.handle_key(KeyCode::Down.into());
    }
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(screen(&terminal).contains("/mount/29"));
    app.filter = "no such mount".into();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("No matching filesystems."), "{text}");
    assert_eq!(text.matches("100%").count(), 2, "{text}");
    assert!(text.lines().last().unwrap().contains("F10"));
}

#[test]
fn layouts_fit_narrow_and_wide_terminals_in_both_languages_and_views() {
    for (language, title, quit, done, save, read, more) in [
        (
            Language::En,
            "Focus ",
            "F10Quit",
            "Done",
            "Save",
            "READ/s",
            "more disks",
        ),
        (
            Language::De,
            "Fokus ",
            "F10Ende",
            "Fertig",
            "Speich",
            "LESEN/s",
            "weitere Disks",
        ),
    ] {
        for (width, height) in SIZES {
            let mut app = app();
            app.settings.language = language;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let at = format!("{language:?} at {width}x{height}");
            for (view, marker) in [
                (View::Processes, read),
                (View::Filesystems, "/home"),
                (View::Home, "tim/"),
            ] {
                app.settings.view = view;
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                let text = screen(&terminal);
                assert!(text.contains(title), "title missing {at} {view:?}\n{text}");
                assert!(
                    text.contains(marker),
                    "{marker} missing {at} {view:?}\n{text}"
                );
                let footer = text.lines().last().unwrap();
                // Every required function key must be complete on the bottom row.
                for key in ["F1", "F2", "F4", quit, "F12"] {
                    assert!(footer.contains(key), "{key} missing {at}: {footer:?}");
                }
                assert!(footer.contains(save), "save label cut {at}: {footer:?}");
            }
            for overlay in [
                Overlay::Help,
                Overlay::Devices,
                Overlay::Setup,
                Overlay::Sort,
            ] {
                app.overlay = overlay;
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            }
            app.overlay = Overlay::Setup;
            for (category, name) in categories(app.lang()).iter().enumerate() {
                app.setup_category = category;
                app.setup_focus = true;
                app.setup_option = app.setup_len() - 1;
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                let text = screen(&terminal);
                assert!(
                    text.contains(&format!("{name} >")),
                    "category {name} cut {at}\n{text}"
                );
                let keys = text.lines().last().unwrap();
                assert!(keys.contains(done), "setup exit missing {at}: {keys:?}");
                assert!(keys.contains(save), "setup save missing {at}: {keys:?}");
            }
            // Every disk is either on screen or counted in "+N more".
            for count in DISK_COUNTS {
                let mut app = app_with_disks(count);
                app.settings.language = language;
                let names: Vec<String> = app
                    .overview_devices()
                    .iter()
                    .map(|device| device.name.clone())
                    .collect();
                assert_eq!(names.len(), count);
                for view in [View::Processes, View::Filesystems, View::Home] {
                    app.settings.view = view;
                    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                    let text = screen(&terminal);
                    let at = format!("{at} with {count} disks in {view:?}");
                    let layout = app.disk_layout.unwrap();
                    assert!(layout.visible_rows >= 1, "no disk visible {at}\n{text}");
                    assert!(text.contains(&format!("Disks {count} ")), "{at}\n{text}");
                    let shown = shown_disks(&text, &names);
                    if layout.overflow() {
                        let hidden = count - shown;
                        assert!(hidden > 0, "{at}\n{text}");
                        assert!(
                            text.contains(&format!("+{hidden} {more}")),
                            "hidden disks not counted {at}\n{text}"
                        );
                    } else {
                        assert_eq!(shown, count, "disk missing {at}\n{text}");
                    }
                    // I/O meters belong only to the dedicated I/O view.
                    if view == View::Processes {
                        for label in [rd_label(app.lang()), wr_label(app.lang()), "IO"] {
                            let short = &label[..1];
                            let meters = text.matches(&format!("{label}[")).count()
                                + text.matches(&format!("{short}[")).count();
                            assert!(meters >= shown, "{label} meters {at}\n{text}");
                        }
                    } else {
                        assert!(
                            !text.contains("RD[") && !text.contains("WR[") && !text.contains("IO["),
                            "{text}"
                        );
                    }
                    if view == View::Processes {
                        assert!(text.contains(read), "process strip {at}\n{text}");
                    }
                    let footer = text.lines().last().unwrap();
                    assert!(footer.contains(quit), "footer {at}: {footer:?}");
                }
            }
        }
    }
}

#[test]
fn overview_degrades_from_graphs_to_rows_and_scrolls_with_focus() {
    let layout = |width, height, count| disk_layout(width, height, count, true);
    // Wide terminals: graphs beside the meters, several columns.
    let wide = layout(200, 51, 8);
    assert!(matches!(wide.style, BlockStyle::Side(_)), "{wide:?}");
    assert!(wide.columns >= 3 && !wide.overflow(), "{wide:?}");
    assert!(layout(200, 51, 16).style.has_graph());
    assert_eq!(layout(120, 27, 2).columns, 2);
    assert!(matches!(layout(120, 27, 4).style, BlockStyle::Side(_)));
    // One disk gets one block, never an empty second column.
    assert_eq!(layout(200, 51, 1).columns, 1);
    // Narrow terminals put the graph below the meters.
    assert!(matches!(layout(40, 18, 1).style, BlockStyle::Below(_)));
    // Shorter graphs, then meters only, then one row per disk.
    let heights: Vec<u16> = (4..=16)
        .rev()
        .map(|graph| layout(80, 2 * (5 + graph), 2).style.height())
        .collect();
    assert!(
        heights.windows(2).all(|pair| pair[0] >= pair[1]),
        "{heights:?}"
    );
    assert_eq!(layout(80, 10, 4).style, BlockStyle::Meters);
    assert_eq!(layout(40, 12, 4).style, BlockStyle::Pair);
    assert_eq!(layout(40, 8, 4).style, BlockStyle::Row);
    // Rows use a second column before anything is hidden.
    let rows = layout(80, 15, 16);
    assert_eq!((rows.style, rows.columns), (BlockStyle::Row, 2));
    assert!(!rows.overflow());
    // Four disks on a wide terminal form a 2 x 2 grid, not 3 + 1.
    assert_eq!(layout(200, 51, 4).columns, 2);
    let scrolled = layout(36, 9, 16);
    assert!(
        scrolled.overflow() && scrolled.visible_rows == 8,
        "{scrolled:?}"
    );
    assert_eq!(scrolled.height(), 9);
    assert!(!disk_layout(120, 27, 4, false).style.has_graph());

    // At 36x16 sixteen disks scroll; the focused disk is kept on screen.
    let mut app = app_with_disks(16);
    let mut terminal = Terminal::new(TestBackend::new(36, 16)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("more disks ↓"), "{text}");
    assert!(!text.contains("↑"), "{text}");
    for _ in 0..15 {
        app.handle_key(KeyCode::Tab.into());
    }
    assert_eq!(app.device.as_deref(), Some("sdp"));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.lines().any(|line| line.starts_with("sdp ")), "{text}");
    assert!(text.contains("↑"), "{text}");
    // The focused disk is shown in reverse video.
    let buffer = terminal.backend().buffer();
    let row = (0..16)
        .find(|y| buffer[(0, *y)].symbol() == "s" && buffer[(2, *y)].symbol() == "p")
        .unwrap();
    assert!(buffer[(0, row)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn hidepid_warning_does_not_require_a_hidden_process_count() {
    let mut app = App::new(
        None,
        Settings {
            view: View::Processes,
            ..Settings::default()
        },
        false,
    );
    for privileged in [false, true] {
        app.snapshot.process_access.privileged = privileged;
        app.snapshot.process_access.unreadable = 0;
        app.snapshot.process_access.message = if privileged {
            "Process I/O readable; other users' command lines hidden by /proc hidepid"
        } else {
            "Other users' processes hidden by /proc hidepid; total hidden count unavailable"
        }
        .into();
        app.settings.language = Language::En;
        assert_eq!(
            status_line(&app),
            Some((
                "hidepid: process details restricted (F1)".into(),
                Color::Yellow
            ))
        );
        app.settings.language = Language::De;
        assert_eq!(
            status_line(&app).unwrap().0,
            "hidepid: Prozessdetails beschränkt (F1)"
        );
    }
    app.snapshot.process_access.message = "All processes readable (helper or root)".into();
    assert!(status_line(&app).is_none());
}

#[test]
fn zfs_summary_counts_one_pool_across_datasets_and_disks() {
    use crate::model::{DeviceUsage, ZfsPool};

    let pool = ZfsPool {
        name: "tank".into(),
        total_bytes: Some(1000),
        used_bytes: Some(300),
        available_bytes: Some(700),
        error: None,
    };
    let filesystems: Vec<_> = ["tank", "tank/home", "tank/data"]
        .into_iter()
        .map(|source| Filesystem {
            source: source.into(),
            fs_type: "zfs".into(),
            // Dataset statvfs values deliberately differ from pool use.
            total_bytes: Some(800),
            used_bytes: Some(100),
            available_bytes: Some(700),
            member_devices: vec!["sda".into(), "sdb".into()],
            zfs: Some(pool.clone()),
            ..Default::default()
        })
        .collect();
    let devices = ["sda", "sdb"]
        .into_iter()
        .map(|name| Device {
            name: name.into(),
            usage: Some(DeviceUsage::from_filesystems(
                &filesystems,
                &BTreeSet::from([name.into()]),
                false,
            )),
            ..Default::default()
        })
        .collect();
    let mut app = App::new(None, Settings::default(), false);
    app.update(Snapshot {
        devices,
        filesystems,
        ..Default::default()
    });
    assert_eq!(combined_usage(&app), (300, 1000));
    assert!(status_line(&app).is_none());

    for filesystem in &mut app.snapshot.filesystems {
        let pool = filesystem.zfs.as_mut().unwrap();
        pool.used_bytes = None;
        pool.error = Some("membership unavailable".into());
    }
    // Do not substitute or sum the still-readable dataset values.
    assert_eq!(combined_usage(&app), (0, 0));
    let (warning, color) = status_line(&app).unwrap();
    assert_eq!(warning, "ZFS pool data unavailable: tank");
    assert_eq!(color, Color::Yellow);
    app.settings.language = Language::De;
    assert_eq!(
        status_line(&app).unwrap().0,
        "ZFS-Pooldaten nicht verfügbar: tank"
    );
}

#[test]
fn read_and_write_meters_share_one_scale_across_disks() {
    const MIB: f64 = 1024.0 * 1024.0;
    let mut app = App::new(
        None,
        Settings {
            view: View::Processes,
            ..Settings::default()
        },
        true,
    );
    let mut snapshot = demo::snapshot(0, 0.0, "/home", 0);
    snapshot
        .devices
        .retain(|device| device.name == "sda" || device.name == "sdb");
    for device in &mut snapshot.devices {
        let read = if device.name == "sda" {
            MIB
        } else {
            40.0 * MIB
        };
        device.rates = Some(DeviceRates {
            read_rate: read,
            write_rate: read / 2.0,
            ..DeviceRates::default()
        });
    }
    for tick in 0..5 {
        snapshot.elapsed = f64::from(tick);
        app.update(snapshot.clone());
    }
    let scale = rate_scale(&app);
    assert_eq!(scale, 40.0 * MIB, "the busiest disk sets the scale for all");
    let sda = app
        .snapshot
        .devices
        .iter()
        .find(|d| d.name == "sda")
        .unwrap();
    let sdb = app
        .snapshot
        .devices
        .iter()
        .find(|d| d.name == "sdb")
        .unwrap();
    let [sda_read, sda_write, ..] = disk_meters(&app, sda, scale);
    let [sdb_read, sdb_write, ..] = disk_meters(&app, sdb, scale);
    assert_eq!((sdb_read.ratio, sdb_write.ratio), (1.0, 0.5));
    assert_eq!((sda_read.ratio, sda_write.ratio), (1.0 / 40.0, 0.5 / 40.0));
    // Rendered: sdb's read bar is far longer than sda's.
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    let bars: Vec<usize> = text
        .lines()
        .flat_map(|line| line.split("RD[").skip(1))
        .map(|meter| meter.chars().take_while(|c| *c == '|').count())
        .collect();
    assert_eq!(bars.len(), 2, "{text}");
    assert!(bars[0] <= 1 && bars[1] >= 12, "{bars:?}\n{text}");
    // An idle overview keeps the floor instead of dividing by zero.
    let mut idle = App::new(None, Settings::default(), true);
    idle.update(demo::snapshot(0, 0.0, "/home", 0));
    assert!(rate_scale(&idle) >= GRAPH_FLOOR);
}

#[test]
fn disk_blocks_show_fill_level_and_its_graph() {
    let mut app = app();
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    // Demo: nvme0n1 78%, sda 65%, sdb not mounted.
    assert!(text.contains("78% 1.5 TiB/1.9 TiB]"), "{text}");
    assert!(text.contains("not mounted]"), "{text}");
    for label in ["100%", "0%", " IO ", " US "] {
        assert!(text.contains(label), "{label} missing\n{text}");
    }
    // Utilization and fill level share one 0-100% scale: a full disk
    // draws its fill line on the top graph row, an idle one at the bottom.
    let mut snapshot = app.snapshot.clone();
    for device in &mut snapshot.devices {
        if device.name == "sdb" {
            device.usage = Some(crate::model::DeviceUsage {
                filesystems: 1,
                total_bytes: 100,
                used_bytes: 95,
                available_bytes: 0,
                ..Default::default()
            });
        }
    }
    for tick in 0..30 {
        snapshot.elapsed = 61.0 + f64::from(tick);
        app.update(snapshot.clone());
    }
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("100% 95 B/95 B]"), "{text}");
    let buffer = terminal.backend().buffer();
    let usage = plot_color(app.settings.usage_color);
    let (x, y) = (0..36)
        .flat_map(|y| (0..120).map(move |x| (x, y)))
        .find(|(x, y)| buffer[(*x, *y)].symbol() == "s" && buffer[(x + 2, *y)].symbol() == "b")
        .unwrap();
    // The US meter of a disk at 90% or more turns red.
    let meter = (x..120)
        .map(|x| &buffer[(x, y + 4)])
        .find(|cell| cell.symbol() == "|")
        .unwrap();
    assert_eq!(meter.fg, Color::Red);
    let top =
        (x..120).any(|x| buffer[(x, y + 1)].fg == usage && buffer[(x, y + 1)].symbol() == "─");
    assert!(top, "full disk is not at 100%\n{text}");
    // Both series can be hidden separately; without either, no graph.
    app.settings.show_usage_series = false;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        !buffer
            .content
            .iter()
            .any(|cell| cell.fg == usage && cell.symbol() == "─")
    );
    assert!(app.disk_layout.unwrap().style.has_graph());
    app.settings.show_io_series = false;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(!app.disk_layout.unwrap().style.has_graph());
    // Braille drawing uses the same scale and series.
    app.settings.show_io_series = true;
    app.settings.graph_style = GraphStyle::Braille;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.fg == UTIL
                && ('\u{2801}'..='\u{28ff}').contains(&cell.symbol().chars().next().unwrap()))
    );
}

#[test]
fn stacked_devices_on_one_shown_disk_are_folded_into_it() {
    let mut app = app();
    let names = |app: &App| -> Vec<String> {
        app.overview_devices()
            .iter()
            .map(|device| device.name.clone())
            .collect()
    };
    // Demo dm-0 is LUKS on nvme1n1 and holds /data, counted on nvme1n1.
    assert_eq!(names(&app), ["nvme0n1", "nvme1n1", "sda", "sdb"]);
    let nvme1 = app.snapshot.devices.iter().find(|d| d.name == "nvme1n1");
    assert_eq!(nvme1.unwrap().usage.as_ref().unwrap().filesystems, 1);
    app.settings.show_stacked = true;
    assert_eq!(names(&app), ["nvme0n1", "nvme1n1", "dm-0", "sda", "sdb"]);
    app.settings.show_stacked = false;
    // A focused stacked device stays visible.
    app.device = Some("dm-0".into());
    assert!(names(&app).contains(&"dm-0".to_owned()));
    app.device = None;
    // RAID or LVM across several disks, and stacks on hidden devices, stay.
    let mut snapshot = app.snapshot.clone();
    snapshot.devices.push(Device {
        name: "md0".into(),
        kind: DeviceKind::Md,
        backing_disks: vec!["sda".into(), "sdb".into()],
        ..Device::default()
    });
    snapshot.devices.push(Device {
        name: "dm-1".into(),
        kind: DeviceKind::Dm,
        backing_disks: vec!["loop0".into()],
        ..Device::default()
    });
    app.update(snapshot);
    let shown = names(&app);
    assert!(shown.contains(&"md0".to_owned()) && shown.contains(&"dm-1".to_owned()));
    assert!(!shown.contains(&"dm-0".to_owned()));
    // Setup > Device > Show stacked devices toggles and is saved.
    app.overlay = Overlay::Setup;
    app.setup_category = 1;
    app.setup_focus = true;
    app.setup_option = 1;
    app.handle_key(KeyCode::Enter.into());
    assert!(app.settings.show_stacked && app.settings_to_save().show_stacked);
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(screen(&terminal).contains("[*] Show stacked devices"));
    app.settings.language = Language::De;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(screen(&terminal).contains("Gestapelte Geräte zeigen"));
}

#[test]
fn process_rows_are_configurable_and_zero_hides_the_strip() {
    let mut app = app();
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    app.settings.process_rows = 0;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(!screen(&terminal).contains("COMMAND"));
    app.settings.process_rows = 2;
    app.settings.show_idle = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("COMMAND"), "{text}");
    assert!(app.visible_rows >= 2, "{}", app.visible_rows);
    // Setup > Table > Process rows cycles 0..=20.
    app.overlay = Overlay::Setup;
    app.setup_category = 3;
    app.setup_focus = true;
    app.setup_option = 3;
    app.settings.process_rows = 20;
    assert_eq!(
        app.handle_key(KeyCode::Char('+').into()),
        Action::SettingsChanged
    );
    assert_eq!(app.settings.process_rows, 0);
    app.handle_key(KeyCode::Char('-').into());
    assert_eq!(app.settings.process_rows, 20);
    assert_eq!(app.settings_to_save().process_rows, 20);
    // Chart > IO and fill-level series and the fill color.
    app.setup_category = 2;
    for (option, check) in [(5, 0), (6, 1)] {
        app.setup_option = option;
        app.handle_key(KeyCode::Enter.into());
        let settings = &app.settings;
        assert!(![settings.show_io_series, settings.show_usage_series][check]);
    }
    app.setup_option = 7;
    app.handle_key(KeyCode::Enter.into());
    assert_eq!(app.settings.usage_color, PlotColor::White);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(screen(&terminal).contains("Fill level color"));
}

#[test]
fn german_text_covers_monitor_overlays_and_language_setup_row() {
    let mut app = app();
    app.system_lang = Lang::De;
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    for expected in [
        "Fokus ",
        "BL[",
        "nicht eingehängt",
        "BENUTZER",
        "BEFEHL",
        "Hilfe",
        "Speichern",
        "gesamt",
        "I/O",
    ] {
        assert!(text.contains(expected), "{expected} missing: {text}");
    }
    for (overlay, expected) in [
        (
            Overlay::Help,
            "Aktuelle Einstellungen für den nächsten Start",
        ),
        (Overlay::Sort, "Prozesse sortieren"),
        (Overlay::Devices, "kein Fokus"),
        (Overlay::Setup, "Optionen: Allgemein"),
    ] {
        app.overlay = overlay;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains(expected), "{expected} missing");
    }
    // The third General row switches the language at runtime.
    app.overlay = Overlay::None;
    app.handle_key(KeyCode::F(2).into());
    app.handle_key(KeyCode::Right.into());
    for _ in 0..2 {
        app.handle_key(KeyCode::Down.into());
    }
    assert_eq!(
        app.handle_key(KeyCode::Enter.into()),
        Action::SettingsChanged
    );
    assert_eq!(app.settings.language, Language::En);
    assert_eq!(app.lang(), Lang::En);
    assert_eq!(app.settings_to_save().language, Language::En);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(screen(&terminal).contains("[English] Language"));
}

#[test]
fn sort_search_pause_and_idle_filter() {
    let mut app = app();
    app.handle_key(KeyCode::F(3).into());
    for c in "restic".chars() {
        app.handle_key(KeyCode::Char(c).into());
    }
    app.handle_key(KeyCode::Enter.into());
    assert_eq!(app.process_rows().len(), 1);
    assert_eq!(app.process_rows()[0].name, "restic");
    app.handle_key(KeyCode::Esc.into());
    // systemd has no I/O and idle rows are hidden by default.
    assert_eq!(app.process_rows().len(), 6);
    app.settings.show_idle = true;
    assert_eq!(app.process_rows().len(), 7);
    app.settings.sort = Sort::Pid;
    assert_eq!(app.process_rows()[0].pid, 1);
    app.settings.sort = Sort::Io;
    assert_eq!(app.process_rows()[0].name, "cargo");
    app.handle_key(KeyCode::Char(' ').into());
    app.update(demo::snapshot(100, 100.0, "/home", 99));
    assert_eq!(app.snapshot.elapsed, 60.0);
    app.handle_key(KeyCode::Char(' ').into());
    app.update(demo::snapshot(101, 101.0, "/home", 99));
    assert_eq!(app.snapshot.elapsed, 101.0);
    assert!(
        app.history
            .values()
            .flatten()
            .all(|sample| sample.at >= 41.0)
    );
}

#[test]
fn focus_keeps_history_and_tab_cycles_whole_disks() {
    let mut app = app();
    assert_eq!(app.handle_key(KeyCode::Tab.into()), Action::DeviceChanged);
    assert_eq!(
        app.device.as_deref(),
        Some("nvme1n1"),
        "partitions are skipped"
    );
    assert_eq!(app.history["nvme0n1"].len(), 61, "history is kept per disk");
    for _ in 0..3 {
        app.handle_key(KeyCode::Tab.into());
    }
    assert_eq!(
        app.device, None,
        "loop0 is hidden, so the cycle returns to all"
    );
    app.handle_key(KeyCode::BackTab.into());
    assert_eq!(app.device.as_deref(), Some("sdb"));
    // A focused partition joins the overview after its disk.
    app.device = Some("nvme0n1p2".into());
    let names: Vec<&str> = app
        .overview_devices()
        .iter()
        .map(|device| device.name.as_str())
        .collect();
    assert_eq!(names, ["nvme0n1", "nvme0n1p2", "nvme1n1", "sda", "sdb"]);
    app.device = Some("sdb".into());
    app.settings.show_virtual = true;
    app.handle_key(KeyCode::Tab.into());
    assert_eq!(app.device.as_deref(), Some("loop0"));
    assert_eq!(app.settings_to_save().device.as_deref(), Some("loop0"));
    // The aggregate uses physical whole disks only.
    app.handle_key(KeyCode::Tab.into());
    let all = app.selected_rates().unwrap();
    let disks: f64 = app
        .snapshot
        .devices
        .iter()
        .filter(|device| matches!(device.name.as_str(), "nvme0n1" | "nvme1n1" | "sda" | "sdb"))
        .map(|device| device.rates.as_ref().unwrap().read_rate)
        .sum();
    assert!((all.read_rate - disks).abs() < 1e-6);
}

#[test]
fn home_view_drills_down_and_back_and_shows_summaries() {
    let mut app = app();
    assert_eq!(
        app.handle_key(KeyCode::Char('2').into()),
        Action::HomeChanged
    );
    assert!(app.home_active && app.take_home_request().scan);
    let entries = app.home_entries();
    assert_eq!(entries[0].name, "tim");
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind == HomeEntryKind::Files)
    );
    let shared = entries.iter().find(|entry| entry.name == "shared").unwrap();
    assert!(shared.partial);
    app.handle_key(KeyCode::Enter.into());
    assert_eq!(app.home_cwd, ["tim"]);
    let inside = app.home_entries();
    assert_eq!(inside[0].name, "Videos");
    assert!(
        inside
            .iter()
            .any(|entry| entry.kind == HomeEntryKind::Folded)
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("/home/tim"), "{text}");
    assert!(text.contains("[41 smaller dirs]"), "{text}");
    assert!(text.contains("+1.6 GiB"), "{text}");
    // Leaf directories without retained content do not open.
    app.handle_key(KeyCode::Backspace.into());
    assert!(app.home_cwd.is_empty());
    assert_eq!(app.table.selected(), Some(0), "selection returns to tim");
    app.handle_key(KeyCode::Char('r').into());
    assert!(app.take_home_request().rescan);
    assert!(!app.take_home_request().rescan, "rescan is sent once");
    app.handle_key(KeyCode::Char('a').into());
    assert!(app.settings.apparent_size);
    // A tree for a different root resets the drill-down position.
    app.home_cwd = vec!["tim".into()];
    let mut snapshot = demo::snapshot(200, 200.0, "/srv/home", 0);
    snapshot.home_tree = Some(demo::home_tree("/srv/home", 9));
    app.update(snapshot);
    assert!(app.home_cwd.is_empty());
}

#[test]
fn home_rescan_keeps_surviving_prefix_and_enters_siblings() {
    for folded in [false, true] {
        let mut app = app();
        app.handle_key(KeyCode::Char('2').into());
        let node = |name: &str, children: Vec<u32>| DirNode {
            name: name.into(),
            children,
            allocated: 4096,
            ..DirNode::default()
        };
        app.update(Snapshot {
            home_tree: Some(HomeReport {
                root: "/home".into(),
                nodes: vec![
                    node("/home", vec![1]),
                    node("parent", vec![2]),
                    node("gone", vec![3]),
                    node("old-child", vec![]),
                ],
                ..HomeReport::default()
            }),
            ..Snapshot::default()
        });
        app.handle_key(KeyCode::Enter.into());
        app.handle_key(KeyCode::Enter.into());
        assert_eq!(app.home_cwd, ["parent", "gone"]);
        app.filter = "old".into();
        app.table.select(Some(99));
        let mut parent = node("parent", vec![2]);
        if folded {
            // The missing directory may still exist on disk but have
            // been folded out of the bounded retained tree.
            parent.other.dirs = 1;
        }
        app.update(Snapshot {
            home_tree: Some(HomeReport {
                root: "/home".into(),
                generation: 2,
                nodes: vec![
                    node("/home", vec![1]),
                    parent,
                    node("survivor", vec![3]),
                    node("new-child", vec![]),
                ],
                ..HomeReport::default()
            }),
            ..Snapshot::default()
        });
        assert_eq!(app.home_cwd, ["parent"]);
        assert!(app.filter.is_empty());
        assert_eq!(app.table.selected(), Some(0));
        app.handle_key(KeyCode::Enter.into());
        assert_eq!(app.home_cwd, ["parent", "survivor"]);
        assert_eq!(app.home_entries()[0].name, "new-child");
        app.handle_key(KeyCode::Backspace.into());
        assert_eq!(app.home_cwd, ["parent"]);
        assert_eq!(app.table.selected(), Some(0));
    }
}

#[test]
fn home_navigation_uses_identity_and_displays_names() {
    let mut app = app();
    app.handle_key(KeyCode::Char('2').into());
    let node = |name: &str, key: &str, children: Vec<u32>, allocated| DirNode {
        name: name.into(),
        key: Some(key.into()),
        children,
        allocated,
        ..DirNode::default()
    };
    app.update(Snapshot {
        home_tree: Some(HomeReport {
            root: "/home".into(),
            nodes: vec![
                node("/home", "", vec![1, 2], 30),
                node("same~", "01", vec![3], 20),
                node("same~", "02", vec![4], 10),
                node("first-child", "03", vec![], 20),
                node("second-child", "04", vec![], 10),
            ],
            ..HomeReport::default()
        }),
        ..Snapshot::default()
    });
    app.table.select(Some(1));
    app.handle_key(KeyCode::Enter.into());
    assert_eq!(app.home_cwd, ["02"]);
    assert_eq!(app.home_entries()[0].name, "second-child");
    assert_eq!(app.home_node().unwrap().1, ["same~"]);
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("/home/same~"), "{text}");
    assert!(!text.contains("/home/02"), "{text}");
    // Retained indices and display order may change between scans, but
    // the open directory and its parent selection keep their identity.
    let mut tree = app.home_tree.clone().unwrap();
    tree.generation += 1;
    tree.nodes[0].children.reverse();
    tree.nodes[1].allocated = 5;
    tree.nodes.swap(1, 2);
    app.update(Snapshot {
        home_tree: Some(tree),
        ..Snapshot::default()
    });
    assert_eq!(app.home_entries()[0].name, "second-child");
    app.handle_key(KeyCode::Backspace.into());
    assert!(app.home_cwd.is_empty());
    assert_eq!(app.table.selected(), Some(0));
}

#[test]
fn home_path_is_edited_in_setup_and_validated() {
    let mut app = app();
    app.handle_key(KeyCode::F(2).into());
    for _ in 0..4 {
        app.handle_key(KeyCode::Down.into());
    }
    app.handle_key(KeyCode::Right.into());
    assert_eq!(app.handle_key(KeyCode::Enter.into()), Action::None);
    for _ in 0.."/home".len() {
        app.handle_key(KeyCode::Backspace.into());
    }
    for c in "srv".chars() {
        app.handle_key(KeyCode::Char(c).into());
    }
    // Relative paths are refused and editing continues.
    assert_eq!(app.handle_key(KeyCode::Enter.into()), Action::None);
    assert!(app.notice_error);
    app.handle_key(KeyCode::Home.into());
    assert_eq!(
        app.editing.as_deref(),
        Some("srv"),
        "F-keys and Home do not leak"
    );
    app.editing = Some("/srv".into());
    assert_eq!(app.handle_key(KeyCode::Enter.into()), Action::HomeChanged);
    assert_eq!(app.settings.home_path, "/srv");
    assert_eq!(app.settings_to_save().home_path, "/srv");
    assert_eq!(app.take_home_request().root, "/srv");
    // Rescan interval steps and returns a resample action.
    app.handle_key(KeyCode::Down.into());
    assert_eq!(
        app.handle_key(KeyCode::Char('+').into()),
        Action::HomeChanged
    );
    assert_eq!(app.settings.rescan_minutes, 10);
    app.handle_key(KeyCode::Char('-').into());
    app.handle_key(KeyCode::Char('-').into());
    assert_eq!(app.settings.rescan_minutes, 2);
}

#[test]
fn unavailable_rates_and_hidden_processes_are_visible() {
    let mut app = app();
    app.demo = false;
    for device in &mut app.snapshot.devices {
        device.rates = None;
    }
    app.snapshot.processes[0].read_rate = None;
    app.snapshot.processes[0].write_rate = None;
    app.snapshot.process_access.privileged = false;
    app.snapshot.process_access.unreadable = 42;
    // Unknown rates sort last; enough rows keep them on screen.
    app.settings.process_rows = 20;
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(
        text.contains("42 processes of other users hidden"),
        "{text}"
    );
    let read_line = text
        .lines()
        .find(|line| line.starts_with("RD["))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(read_line.contains(" -]"), "{read_line}");
    assert!(text.contains("no rates in this interval"), "{text}");
    let cargo = text
        .lines()
        .find(|line| line.contains("cargo build"))
        .unwrap();
    assert!(cargo.contains(" - "), "{cargo}");
    assert!(
        app.process_rows().iter().any(|row| row.name == "cargo"),
        "unknown is not idle"
    );
}

#[test]
fn plain_text_uses_the_terminal_default_foreground() {
    let mut app = app();
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    for view in [View::Processes, View::Filesystems, View::Home] {
        app.settings.view = view;
        for overlay in [
            Overlay::None,
            Overlay::Setup,
            Overlay::Help,
            Overlay::Devices,
        ] {
            app.overlay = overlay;
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| cell.fg != Color::White && cell.bg != Color::White),
                "white text is unreadable on light terminal themes"
            );
        }
    }
    app.overlay = Overlay::None;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let summary = (0..10).map(|x| &buffer[(x, 1)]).collect::<Vec<_>>();
    assert_eq!(summary[0].symbol(), "U");
    assert!(summary.iter().all(|cell| cell.fg == Color::Reset));
}

#[test]
fn setup_changes_apply_without_quitting_or_implicitly_saving() {
    let mut app = app();
    assert_eq!(app.handle_key(KeyCode::F(2).into()), Action::None);
    assert_eq!(app.overlay, Overlay::Setup);
    app.handle_key(KeyCode::Right.into());
    app.handle_key(KeyCode::Down.into());
    assert_eq!(
        app.handle_key(KeyCode::Char('+').into()),
        Action::SettingsChanged
    );
    assert_eq!(app.settings.interval_ms, 1100);
    assert!(app.settings_dirty());
    assert_eq!(app.handle_key(KeyCode::F(10).into()), Action::None);
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.handle_key(KeyCode::F(12).into()), Action::SaveSettings);
    app.settings_saved("Saved test preferences".into());
    assert!(!app.settings_dirty());
    app.handle_key(KeyCode::F(6).into());
    app.handle_key(KeyCode::Down.into());
    app.handle_key(KeyCode::Enter.into());
    assert_eq!(app.settings.sort, Sort::Read);
    assert_eq!(app.overlay, Overlay::None);
    // Views are saved preferences, too.
    app.handle_key(KeyCode::Char('1').into());
    assert_eq!(app.settings_to_save().view, View::Filesystems);
    assert_eq!(app.handle_key(KeyCode::F(10).into()), Action::Quit);
}

#[test]
fn monochrome_covers_small_windows_all_views_and_overlays() {
    let mut app = app();
    app.settings.color = false;
    for (width, height) in [(20, 10), (36, 16), (52, 18), (120, 36)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        for view in [View::Processes, View::Filesystems, View::Home] {
            app.settings.view = view;
            for overlay in [
                Overlay::None,
                Overlay::Help,
                Overlay::Setup,
                Overlay::Sort,
                Overlay::Devices,
            ] {
                app.overlay = overlay;
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                assert!(
                    terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset),
                    "colored cell in {overlay:?} at {width}x{height}"
                );
            }
        }
    }
}

#[test]
fn hostile_names_never_reach_the_terminal_raw() {
    let mut app = app();
    app.settings.view = View::Home;
    let mut tree = demo::home_tree("/home", 50);
    // The collector sanitizes; the renderer must still not panic on width.
    tree.nodes[1].name = crate::text::sanitize_bytes(b"evil\x1b]0;pwned\x07\xff");
    app.home_tree = Some(tree);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("evil\\x1b]0;pwned\\x07\\xff/"), "{text}");
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn graph_scale_is_labeled_and_hiding_graphs_frees_rows() {
    let mut wide = app();
    wide.device = None;
    let mut terminal = Terminal::new(TestBackend::new(200, 60)).unwrap();
    terminal.draw(|frame| draw(frame, &mut wide)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("graphs: IO / US 0-100% over 60s"), "{text}");
    assert!(text.contains("[ALL] 4 disks  RD "), "{text}");
    let mut app = app();
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = screen(&terminal);
    assert!(text.contains("100%"), "{text}");
    let with_graph = app.visible_rows;
    app.settings.show_graph = false;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(app.visible_rows > with_graph);
}
