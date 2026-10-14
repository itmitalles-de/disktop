use super::cli::{Args, parse_device, parse_home_path, parse_interval, runtime_settings};
use super::output::{ignore_closed_output, print_snapshot, sample_once};
use super::source::{HomeGenerations, Source};
use super::terminal::refresh;
use anyhow::bail;
use clap::Parser;
use crossterm::event::KeyCode;
use dwtop::config::{Settings, SortKey, View};
use dwtop::demo;
use dwtop::model::Snapshot;
use dwtop::shutdown::SignalGuard;
use dwtop::ui::App;
use std::io;
use std::time::Instant;

#[test]
fn scanner_generations_survive_async_switches_and_unaccepted_trees() {
    let mut generations = HomeGenerations::default();
    let mut app = App::new(None, Settings::default(), false);
    app.update(demo::snapshot(160, 160.0, "/home", 0));
    let acknowledged = app.take_home_request().known_generation;
    assert!(acknowledged > 1);
    generations.switch(acknowledged);
    // No tree during the first asynchronous local request. The next
    // request still acknowledges zero in that scanner, not the old helper.
    assert_eq!(generations.native(acknowledged), 0);
    assert_eq!(generations.native(acknowledged), 0);
    let mut local = demo::snapshot(0, 161.0, "/home", 0);
    generations.publish(&mut local);
    let published = local.home_tree.as_ref().unwrap().generation;
    assert!(published > acknowledged);
    app.paused = true;
    app.update(local.clone());
    assert_eq!(app.take_home_request().known_generation, acknowledged);
    assert_eq!(generations.native(acknowledged), 0);
    app.paused = false;
    app.update(local);
    assert_eq!(
        generations.native(app.take_home_request().known_generation),
        1
    );
    // Reverse switch to a helper that has just started its first scan.
    generations.switch(published);
    assert_eq!(generations.native(published), 0);
    let mut helper = demo::snapshot(0, 162.0, "/srv/new-root", 0);
    generations.publish(&mut helper);
    app.update(helper);
    assert_eq!(
        generations.native(app.take_home_request().known_generation),
        1
    );
}

#[test]
fn argument_parsers_reject_invalid_values() {
    for invalid in ["0", "NaN", "inf", "-1", "61", "nope"] {
        assert!(parse_interval(invalid).is_err());
    }
    assert_eq!(parse_interval("0.5").unwrap(), 0.5);
    assert_eq!(parse_device("/dev/nvme0n1").unwrap(), "nvme0n1");
    assert_eq!(parse_device("all").unwrap(), "all");
    assert!(parse_device("../sda").is_err());
    assert_eq!(parse_home_path("/home/").unwrap(), "/home");
    assert_eq!(parse_home_path("/").unwrap(), "/");
    assert!(parse_home_path("home").is_err());
    assert!(parse_home_path("/home/../etc").is_err());
}

#[test]
fn command_line_overrides_and_no_color_are_runtime_only() {
    let saved = Settings {
        device: Some("sda".into()),
        ..Settings::default()
    };
    let args = Args::parse_from([
        "dwtop",
        "--interval",
        "0.1",
        "--history",
        "10",
        "--device",
        "nvme0n1",
        "--sort",
        "write",
        "--view",
        "home",
        "--home-path",
        "/srv/",
        "--all-devices",
    ]);
    let runtime = runtime_settings(&args, &saved, true);
    assert!(!runtime.color && runtime.show_virtual);
    assert_eq!(runtime.interval_ms, 100);
    assert_eq!(runtime.history_seconds, 10);
    assert_eq!(runtime.device.as_deref(), Some("nvme0n1"));
    assert_eq!(runtime.sort, SortKey::Write);
    assert_eq!(runtime.view, View::Home);
    assert_eq!(runtime.home_path, "/srv");
    let mut app = App::with_saved(Some("nvme0n1".into()), runtime, saved.clone(), true);
    assert_eq!(app.settings_to_save(), saved);
    assert!(!app.settings_dirty());
    // Changing a preference in the monitor persists only that preference.
    app.handle_key(KeyCode::Char('i').into());
    app.handle_key(KeyCode::Char('s').into());
    let expected = Settings {
        view: View::Processes,
        sort: SortKey::Total,
        ..saved.clone()
    };
    assert_eq!(app.settings_to_save(), expected);
    assert!(app.settings_dirty());
    app.settings_saved("saved".into());
    assert!(!app.settings_dirty());
    assert!(app.settings_to_save().color, "NO_COLOR must not be saved");
    assert_eq!(app.settings_to_save().history_seconds, 60);
    assert_eq!(app.settings_to_save().home_path, "/home");
    assert_eq!(app.settings_to_save().device.as_deref(), Some("sda"));
}

#[test]
fn closed_output_pipes_are_a_clean_exit_and_once_output_is_labeled() {
    let closed = io::Error::from(io::ErrorKind::BrokenPipe);
    assert!(ignore_closed_output(Err(closed)).is_ok());
    let other = io::Error::from(io::ErrorKind::PermissionDenied);
    assert!(ignore_closed_output(Err(other)).is_err());
    let mut app = App::new(Some("nvme0n1".into()), Settings::default(), true);
    let snapshot = demo::snapshot(1, 1.0, "/home", 0);
    app.update(snapshot.clone());
    let mut output = Vec::new();
    print_snapshot(&mut output, &app, &snapshot).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.starts_with("dwtop DEMO  nvme0n1 (NVMe"), "{text}");
    assert!(text.contains("*/home"), "{text}");
    assert!(
        !text.contains("READ/s") && !text.contains("IOPS") && !text.contains("COMMAND"),
        "{text}"
    );
    // Every disk of the overview is listed with rates and fill level.
    for disk in ["nvme0n1 ", "nvme1n1 ", "sda ", "sdb "] {
        assert!(
            text.lines().any(|line| line.starts_with(disk)),
            "{disk} missing: {text}"
        );
    }
    assert!(text.contains("not mounted"), "{text}");
    assert!(
        !text.contains("\nloop0 "),
        "hidden devices stay hidden: {text}"
    );
    assert!(text.contains("HOME /home"), "{text}");
}

#[test]
fn once_reports_unavailable_zfs_once_per_pool() {
    let pool = dwtop::model::ZfsPool {
        name: "tank".into(),
        error: Some("ZFS pool discovery pending".into()),
        ..Default::default()
    };
    let snapshot = Snapshot {
        filesystems: ["tank", "tank/home"]
            .into_iter()
            .map(|source| dwtop::model::Filesystem {
                source: source.into(),
                fs_type: "zfs".into(),
                zfs: Some(pool.clone()),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let mut app = App::new(None, Settings::default(), false);
    app.update(snapshot.clone());
    let mut output = Vec::new();
    print_snapshot(&mut output, &app, &snapshot).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert_eq!(output.matches("WARNING ZFS pool tank:").count(), 1);
    assert!(output.contains("ZFS pool discovery pending"));
}

#[test]
fn runtime_sample_errors_are_notices_and_helpers_fall_back() {
    let shutdown = SignalGuard::new().unwrap();
    let mut app = App::new(Some("nvme0n1".into()), Settings::default(), true);
    let mut source = Source::Failing { helper: false };
    refresh(&mut source, &mut app, &shutdown, || {
        panic!("only a failed helper may be replaced")
    });
    assert!(app.notice_error);
    assert!(
        app.notice
            .as_deref()
            .unwrap()
            .contains("injected sample failure")
    );
    assert!(matches!(source, Source::Failing { .. }));

    let mut app = App::new(Some("nvme0n1".into()), Settings::default(), true);
    let mut source = Source::Failing { helper: true };
    refresh(&mut source, &mut app, &shutdown, || {
        Ok(Source::Demo {
            tick: 0,
            start: Instant::now(),
        })
    });
    assert!(matches!(source, Source::Demo { .. }));
    assert!(app.notice_error);
    assert!(app.notice.as_deref().unwrap().starts_with("Helper failed"));
    assert!(
        !app.history.is_empty() && app.history.values().all(|samples| samples.len() == 1),
        "fallback samples immediately"
    );

    let mut app = App::new(Some("nvme0n1".into()), Settings::default(), true);
    let mut source = Source::Failing { helper: true };
    refresh(&mut source, &mut app, &shutdown, || bail!("no counters"));
    assert!(app.notice.as_deref().unwrap().contains("no counters"));

    // --once/--json replace a failed helper the same way.
    let mut app = App::new(None, Settings::default(), false);
    let mut source = Source::Failing { helper: true };
    let (snapshot, replaced) = sample_once(&mut source, &mut app, &shutdown).unwrap();
    assert!(replaced && !snapshot.demo);
    assert!(matches!(source, Source::Live(_)));
    let mut source = Source::Failing { helper: false };
    assert!(sample_once(&mut source, &mut app, &shutdown).is_err());
}
