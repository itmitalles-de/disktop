//! One-shot sampling and text/JSON output.

use super::cli::Args;
use super::source::{Source, direct_collector};
use super::terminal::wait;
use anyhow::{Result, bail};
use dwtop::config::View;
use dwtop::i18n::Lang;
use dwtop::model::{HomeReport, ScanState, Snapshot};
use dwtop::shutdown::SignalGuard;
use dwtop::text::{format_bytes, format_count, format_rate, pad, truncate};
use dwtop::ui::App;
use std::io;
use std::io::Write;
use std::time::Duration;

pub(super) fn print_once(
    args: &Args,
    source: &mut Source,
    app: &mut App,
    first: Snapshot,
    shutdown: &SignalGuard,
) -> Result<()> {
    let mut tree: Option<HomeReport> = first.home_tree;
    if args.scan_home {
        let mut last_status = first.home;
        // Scans are bounded by entry and time budgets; signals end the wait.
        while tree.is_none() {
            if last_status
                .as_ref()
                .is_some_and(|status| status.state == ScanState::Failed)
            {
                let error = last_status
                    .and_then(|status| status.error)
                    .unwrap_or_else(|| "home scan failed".into());
                bail!("{error}");
            }
            if !wait(Duration::from_millis(250), shutdown) {
                return Ok(());
            }
            let snapshot = match sample_once(source, app, shutdown) {
                Err(_) if shutdown.requested() => return Ok(()),
                result => result?.0,
            };
            tree = snapshot.home_tree;
            last_status = snapshot.home;
        }
    }
    let mut snapshot = loop {
        if !wait(Duration::from_millis(app.settings.interval_ms), shutdown) {
            return Ok(());
        }
        match sample_once(source, app, shutdown) {
            Err(_) if shutdown.requested() => return Ok(()),
            // The replacement collector's rates would cover only the moment
            // since it started; measure a whole interval again.
            Ok((_, true)) => continue,
            result => break result?.0,
        }
    };
    if snapshot.home_tree.is_none() {
        snapshot.home_tree = tree;
    }
    app.update(snapshot.clone());
    let mut stdout = io::stdout().lock();
    let written = if args.json {
        let json = serde_json::to_string_pretty(&snapshot)?;
        writeln!(stdout, "{json}")
    } else {
        print_snapshot(&mut stdout, app, &snapshot)
    };
    ignore_closed_output(written.and_then(|()| stdout.flush()))?;
    Ok(())
}

/// One sample for --once/--json. A failing helper is replaced by direct,
/// unprivileged collection as in the interactive monitor; the flag is true
/// when that happened.
pub(super) fn sample_once(
    source: &mut Source,
    app: &mut App,
    shutdown: &SignalGuard,
) -> Result<(Snapshot, bool)> {
    match source.sample(&app.take_home_request(), shutdown) {
        Ok(snapshot) => Ok((snapshot, false)),
        Err(error) if !shutdown.requested() && source.is_helper() => {
            eprintln!("{}", Lang::En.helper_failed(&format!("{error:#}")));
            *source = direct_collector()?;
            app.home_source_changed();
            Ok((source.sample(&app.take_home_request(), shutdown)?, true))
        }
        Err(error) => Err(error),
    }
}

/// A reader that exits early, as in `dwtop --json | head`, is not an error.
pub(super) fn ignore_closed_output(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

pub(super) fn optional_rate(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), format_rate)
}

pub(super) fn print_snapshot(
    out: &mut impl Write,
    app: &App,
    snapshot: &Snapshot,
) -> io::Result<()> {
    let device = app.selected_device();
    let title = match (&app.device, device) {
        (None, _) => "all physical disks".to_owned(),
        (Some(name), Some(device)) => {
            let identity = [device.model.as_deref(), device.label.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "{name} ({}{}{})",
                device.kind.label(),
                if identity.is_empty() { "" } else { " " },
                identity
            )
        }
        (Some(name), None) => format!("{name} (unavailable)"),
    };
    writeln!(
        out,
        "dwtop{}  {title}",
        if snapshot.demo { " DEMO" } else { "" }
    )?;
    if !snapshot.truncated.is_empty() {
        writeln!(out, "WARNING {}", Lang::En.truncated(&snapshot.truncated))?;
    }
    let mut warned_pools = std::collections::BTreeSet::new();
    for pool in snapshot
        .filesystems
        .iter()
        .filter_map(|row| row.zfs.as_ref())
    {
        if let Some(error) = &pool.error
            && warned_pools.insert(&pool.name)
        {
            writeln!(
                out,
                "WARNING ZFS pool {}: {}",
                dwtop::text::sanitize(&pool.name),
                dwtop::text::sanitize(error)
            )?;
        }
    }
    if app.settings.view == View::Processes {
        match app.selected_rates() {
            Some(rates) => writeln!(
                out,
                "READ {}  WRITE {}  IOPS {:.0}/{:.0}  UTIL {:.1}%  AWAIT {}",
                format_rate(rates.read_rate),
                format_rate(rates.write_rate),
                rates.read_iops,
                rates.write_iops,
                rates.utilization_percent,
                rates
                    .await_ms
                    .map_or_else(|| "-".to_owned(), |value| format!("{value:.2} ms"))
            )?,
            None => writeln!(out, "READ -  WRITE -  (no rates for this interval)")?,
        }
        // Every disk of the overview, as the terminal UI shows them.
        writeln!(out)?;
        writeln!(
            out,
            "{:<12}  {:<5}  {:>12}  {:>12}  {:>6}  {:>9}  {:>4}  {:<21}  MOUNTS",
            "DISK", "TYPE", "READ/s", "WRITE/s", "UTIL", "AWAIT", "USE", "USED / SIZE"
        )?;
        for device in app.overview_devices() {
            let rates = device.rates.as_ref();
            let usage = device.usage.as_ref();
            let (percent, used) = match usage {
                Some(usage) => match usage.percent() {
                    Some(percent) => (
                        format!("{percent:.0}%"),
                        format!(
                            "{} / {}",
                            format_bytes(usage.used_bytes as f64),
                            format_bytes(
                                usage.used_bytes.saturating_add(usage.available_bytes) as f64
                            )
                        ),
                    ),
                    None if usage.filesystems == 0 && usage.unmeasured == 0 => (
                        "-".into(),
                        if usage.swap {
                            "swap only"
                        } else {
                            "not mounted"
                        }
                        .into(),
                    ),
                    None => ("-".into(), "-".into()),
                },
                None => ("-".into(), "-".into()),
            };
            writeln!(
                out,
                "{:<12}  {:<5}  {:>12}  {:>12}  {:>6}  {:>9}  {:>4}  {:<21}  {}",
                truncate(&device.name, 12),
                device.kind.label(),
                optional_rate(rates.map(|rates| rates.read_rate)),
                optional_rate(rates.map(|rates| rates.write_rate)),
                rates.map_or_else(
                    || "-".to_owned(),
                    |rates| format!("{:.1}%", rates.utilization_percent)
                ),
                rates
                    .and_then(|rates| rates.await_ms)
                    .map_or_else(|| "-".to_owned(), |value| format!("{value:.2} ms")),
                percent,
                used,
                truncate(&device.mounts.join(" "), 40)
            )?;
        }
        writeln!(out)?;
        let access = &snapshot.process_access;
        writeln!(
            out,
            "Processes: {} readable, {} not readable; {}",
            access.readable, access.unreadable, access.message
        )?;
        writeln!(
            out,
            "{:>8}  {:<12}  {:>12}  {:>12}  COMMAND",
            "PID", "USER", "READ/s", "WRITE/s"
        )?;
        for row in app.process_rows().into_iter().take(15) {
            writeln!(
                out,
                "{:>8}  {}  {:>12}  {:>12}  {}",
                row.pid,
                pad(&row.user, 12),
                optional_rate(row.read_rate),
                optional_rate(row.write_rate),
                truncate(&row.command, 60)
            )?;
        }
    } else {
        writeln!(out, "SPACE  Used / Free / Use%")?;
        writeln!(out)?;
        writeln!(
            out,
            "{:<12}  {:<5}  {:>12}  {:>12}  {:>6}  MOUNTS",
            "DISK", "TYPE", "USED", "FREE", "USE%"
        )?;
        for device in app.overview_devices() {
            let usage = device.usage.as_ref();
            let measured = usage.filter(|usage| usage.percent().is_some());
            let note = usage
                .filter(|usage| usage.filesystems == 0 && usage.unmeasured == 0)
                .map_or("", |usage| {
                    if usage.swap {
                        "swap only"
                    } else {
                        "not mounted"
                    }
                });
            writeln!(
                out,
                "{:<12}  {:<5}  {:>12}  {:>12}  {:>6}  {} {}",
                truncate(&device.name, 12),
                device.kind.label(),
                measured.map_or_else(|| "-".into(), |usage| format_bytes(usage.used_bytes as f64)),
                measured.map_or_else(
                    || "-".into(),
                    |usage| format_bytes(usage.available_bytes as f64)
                ),
                measured
                    .and_then(|usage| usage.percent())
                    .map_or_else(|| "-".into(), |value| format!("{value:.0}%")),
                truncate(&device.mounts.join(" "), 40),
                if usage.is_some_and(|usage| usage.unmeasured > 0) {
                    "PARTIAL"
                } else {
                    note
                }
            )?;
        }
    }
    writeln!(out)?;
    writeln!(
        out,
        "{:<24}  {:<8}  {:>5}  {:>10}  {:>10}  DEVICE",
        "MOUNT", "TYPE", "USE%", "USED", "FREE"
    )?;
    for fs in app.filesystem_rows() {
        writeln!(
            out,
            "{}{}  {:<8.8}  {:>5}  {:>10}  {:>10}  {}{}",
            if fs.contains_home { "*" } else { " " },
            pad(&fs.mount_point, 23),
            fs.fs_type,
            fs.use_percent()
                .map_or_else(|| "-".to_owned(), |value| format!("{value:.0}%")),
            fs.used_bytes
                .map_or_else(|| "-".to_owned(), |value| format_bytes(value as f64)),
            fs.available_bytes
                .map_or_else(|| "-".to_owned(), |value| format_bytes(value as f64)),
            fs.block_device.as_deref().unwrap_or(&fs.source),
            if fs.not_responding {
                "  NOT RESPONDING"
            } else {
                ""
            }
        )?;
    }
    if let Some(tree) = &snapshot.home_tree
        && let Some(root) = tree.root_node()
    {
        writeln!(out)?;
        writeln!(
            out,
            "HOME {}  {}  {} files{}  scan {:.1}s",
            tree.root,
            format_bytes(root.allocated as f64),
            format_count(root.files),
            if root.partial() { "  PARTIAL" } else { "" },
            tree.duration_ms as f64 / 1000.0
        )?;
        for child in root
            .children
            .iter()
            .filter_map(|index| tree.node(*index))
            .take(15)
        {
            writeln!(
                out,
                "{:>10}  {}{}",
                format_bytes(child.allocated as f64),
                child.name,
                if child.partial() { "  (partial)" } else { "" }
            )?;
        }
    }
    Ok(())
}
