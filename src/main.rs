//! Command startup and shutdown; focused command responsibilities live in main/.

#[path = "main/cli.rs"]
mod cli;
#[path = "main/output.rs"]
mod output;
#[path = "main/source.rs"]
mod source;
#[path = "main/terminal.rs"]
mod terminal;

#[cfg(test)]
#[path = "main/tests.rs"]
mod tests;

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{Args, runtime_settings};
use dwtop::collector::{HomeScan, default_device};
use dwtop::config::{ConfigFile, Settings};
use dwtop::diskstats;
use dwtop::helper::{Client, HelperError};
use dwtop::i18n::Lang;
use dwtop::shutdown::SignalGuard;
use dwtop::ui::App;
use output::print_once;
use source::{HomeGenerations, Notice, Source, direct_collector};
use std::io;
use std::io::{IsTerminal, Write};
use std::sync::atomic::Ordering;
use std::time::Instant;
use terminal::{Input, run, spawn_hangup_watchdog, terminal_input};

fn main() -> Result<()> {
    let args = Args::parse();
    let shutdown = SignalGuard::new().context("registering shutdown handlers")?;
    let mut notices = Vec::new();
    let config = match ConfigFile::discover() {
        Ok(config) => Some(config),
        Err(error) => {
            notices.push(Notice::Settings(format!("{error:#}")));
            None
        }
    };
    let saved = match config
        .as_ref()
        .map(ConfigFile::load_with_unknown_keys)
        .transpose()
    {
        Ok(loaded) => {
            let loaded = loaded.unwrap_or_default();
            if !loaded.unknown_keys.is_empty() {
                notices.push(Notice::UnknownKeys(loaded.unknown_keys));
            }
            loaded.settings
        }
        Err(error) => {
            notices.push(Notice::Settings(format!("{error:#}")));
            Settings::default()
        }
    };
    let no_color_env = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    let settings = runtime_settings(&args, &saved, no_color_env);
    settings.validate()?;
    for notice in &notices {
        eprintln!("{}", notice.text(Lang::En));
    }
    if !cfg!(target_os = "linux") && !args.demo {
        bail!("live monitoring requires Linux");
    }
    let mut helper_error = None;
    let mut source = if args.demo {
        Source::Demo {
            tick: 0,
            start: Instant::now(),
        }
    } else if !args.no_helper && unsafe { libc::geteuid() } != 0 {
        match Client::start() {
            Ok(client) => Source::Helper {
                client,
                sysfs: diskstats::Sysfs::default(),
                outdated: false,
                local_home: Box::new(HomeScan::unprivileged()),
                local: false,
                generations: HomeGenerations::default(),
            },
            Err(error) => {
                helper_error = Some(error);
                direct_collector()?
            }
        }
    } else {
        direct_collector()?
    };
    let automatic_device = if args.demo {
        Some("nvme0n1".into())
    } else {
        default_device(&settings.home_path)
    };
    let device = match settings.device.as_deref() {
        Some("all") => None,
        Some(name) => Some(name.to_string()),
        None => automatic_device.clone(),
    };
    let mut app = App::with_saved(device, settings.clone(), saved, args.demo);
    app.auto_device = automatic_device.clone();
    if args.scan_home {
        app.home_active = true;
    }
    let first = match source.sample(&app.take_home_request(), &shutdown) {
        Ok(snapshot) => snapshot,
        Err(_) if shutdown.requested() => return Ok(()),
        Err(error) if source.is_helper() => {
            helper_error = Some(error);
            source = direct_collector()?;
            source.sample(&app.take_home_request(), &shutdown)?
        }
        Err(error) => return Err(error),
    };
    if let Some(error) = &helper_error
        && error.downcast_ref::<HelperError>() != Some(&HelperError::NotInstalled)
    {
        eprintln!("Helper: {error:#}. Run scripts/setup-helper.sh once; see README.");
    }
    if matches!(source, Source::Helper { outdated: true, .. }) {
        let notice = Notice::HelperOutdated;
        eprintln!("{}", notice.text(Lang::En));
        notices.push(notice);
    }
    if let Some(name) = app.device.clone()
        && !first.devices.iter().any(|device| device.name == name)
    {
        if args.device.is_some() {
            bail!(
                "device {name:?} does not exist; available: all, {}",
                first
                    .devices
                    .iter()
                    .map(|device| device.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let notice = Notice::DeviceUnavailable(name);
        eprintln!("{}", notice.text(Lang::En));
        notices.push(notice);
        // Only this run falls back; F12 keeps the saved choice unless changed.
        app.settings.device = None;
        app.device = automatic_device.clone();
    }
    if args.once || args.json {
        return print_once(&args, &mut source, &mut app, first, &shutdown);
    }
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        bail!("interactive mode needs a terminal; use --once or --json for non-interactive output");
    }
    app.system_lang = Lang::system();
    if !notices.is_empty() {
        let lang = app.lang();
        app.show_error(
            notices
                .iter()
                .map(|notice| notice.text(lang))
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    app.update(first);
    // Color policy is applied to the whole buffer by the UI, so changing it in
    // Setup can also reset previously colored cells and override NO_COLOR.
    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let finished = spawn_hangup_watchdog(&shutdown);
    let result = run(
        &mut terminal,
        &mut source,
        &mut app,
        config.as_ref(),
        &shutdown,
    );
    // A failed write to a terminal that hung up is a hangup, not an error.
    if result.is_err() && terminal_input() == Input::HangUp {
        shutdown.request();
    }
    // ratatui::restore and Terminal's drop report failures with eprintln!,
    // which panics (and then aborts in the panic hook) when stderr is the
    // hung-up terminal. There is nothing left to restore on it.
    if shutdown.requested() && terminal_input() == Input::HangUp {
        std::mem::forget(terminal);
    } else {
        drop(terminal);
    }
    if let Err(error) = ratatui::try_restore()
        && !shutdown.requested()
    {
        let _ = writeln!(io::stderr(), "Failed to restore terminal: {error}");
    }
    // The helper is stopped and reaped before the watchdog may give up.
    drop(source);
    finished.store(true, Ordering::Relaxed);
    if shutdown.requested() { Ok(()) } else { result }
}
