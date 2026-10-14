//! CLI parsing and runtime overrides for saved preferences.

use anyhow::Result;
use clap::{Parser, ValueEnum};
use dwtop::config::{Settings, SortKey, View};
use dwtop::{diskstats, scanner};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(super) enum SortArg {
    Io,
    Read,
    Write,
    Total,
    Pid,
    Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(super) enum ViewArg {
    #[value(name = "io", alias = "processes")]
    Processes,
    #[value(alias = "space")]
    Filesystems,
    Home,
}

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Linux storage-space monitor with used-space history, directory growth and a separate I/O view"
)]
pub(super) struct Args {
    /// Block device to show, for example nvme0n1 or sda; 'all' adds physical disks
    #[arg(short = 'D', long, value_parser = parse_device)]
    pub(super) device: Option<String>,
    /// Refresh interval in seconds (0.1 to 60)
    #[arg(short = 'd', long, value_parser = parse_interval)]
    pub(super) interval: Option<f64>,
    /// Visible history in seconds
    #[arg(long, value_parser = clap::value_parser!(u16).range(10..=600))]
    pub(super) history: Option<u16>,
    /// Process sort order
    #[arg(short, long, value_enum)]
    pub(super) sort: Option<SortArg>,
    /// View: storage space (default), home directory growth, or I/O
    #[arg(long, value_enum)]
    pub(super) view: Option<ViewArg>,
    /// Directory tree for the home view (default /home)
    #[arg(long, value_parser = parse_home_path)]
    pub(super) home_path: Option<String>,
    /// With --once or --json: wait for a complete home scan and include it
    #[arg(long)]
    pub(super) scan_home: bool,
    /// Also show loop, ram and zram devices
    #[arg(long)]
    pub(super) all_devices: bool,
    /// Disable colors (also available in F2 Setup)
    #[arg(long)]
    pub(super) no_color: bool,
    /// Do not use the installed helper; show only your own processes
    #[arg(long)]
    pub(super) no_helper: bool,
    /// Print one snapshot after a refresh interval and exit
    #[arg(long)]
    pub(super) once: bool,
    /// Print one snapshot as JSON and exit
    #[arg(long)]
    pub(super) json: bool,
    /// Explicit preview with synthetic data, clearly labeled DEMO
    #[arg(long)]
    pub(super) demo: bool,
}

pub(super) fn parse_interval(value: &str) -> Result<f64, String> {
    let interval: f64 = value
        .parse()
        .map_err(|_| "expected seconds, for example 0.5".to_string())?;
    if !interval.is_finite() || !(0.1..=60.0).contains(&interval) {
        return Err("interval must be between 0.1 and 60 seconds".into());
    }
    Ok(interval)
}

pub(super) fn parse_device(value: &str) -> Result<String, String> {
    let name = value.strip_prefix("/dev/").unwrap_or(value);
    if name == "all" || diskstats::valid_name(name) {
        Ok(name.to_owned())
    } else {
        Err("expected a block device name such as nvme0n1, or 'all'".into())
    }
}

pub(super) fn parse_home_path(value: &str) -> Result<String, String> {
    let trimmed = value.trim_end_matches('/');
    let path = if trimmed.is_empty() { "/" } else { trimmed };
    scanner::validate_root(path)
        .map(|()| path.to_owned())
        .map_err(|error| format!("{error:#}"))
}

/// Apply command-line options and NO_COLOR to a copy of the saved settings.
/// These overrides apply to this run only and are never saved by F12.
pub(super) fn runtime_settings(args: &Args, saved: &Settings, no_color_env: bool) -> Settings {
    let mut settings = saved.clone();
    if let Some(interval) = args.interval {
        settings.interval_ms = (interval * 1000.0).round() as u64;
    }
    if let Some(history) = args.history {
        settings.history_seconds = history;
    }
    if let Some(sort) = args.sort {
        settings.sort = match sort {
            SortArg::Io => SortKey::Io,
            SortArg::Read => SortKey::Read,
            SortArg::Write => SortKey::Write,
            SortArg::Total => SortKey::Total,
            SortArg::Pid => SortKey::Pid,
            SortArg::Command => SortKey::Name,
        };
    }
    if let Some(view) = args.view {
        settings.view = match view {
            ViewArg::Processes => View::Processes,
            ViewArg::Filesystems => View::Filesystems,
            ViewArg::Home => View::Home,
        };
    }
    if let Some(path) = &args.home_path {
        settings.home_path = path.clone();
    }
    if args.all_devices {
        settings.show_virtual = true;
    }
    if args.no_color || no_color_env {
        settings.color = false;
    }
    if let Some(device) = &args.device {
        settings.device = Some(device.clone());
    }
    settings
}
