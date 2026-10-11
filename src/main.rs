use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use crossterm::event::{self, Event, KeyEventKind};
use dwtop::{
    collector::{Collector, HomeScan, complete_storage, default_device},
    config::{ConfigFile, RootWithForeignSettings, Settings, SortKey, View},
    demo, diskstats,
    helper::{Client, HelperError},
    i18n::Lang,
    model::{HomeErrorKind, HomeReport, HomeRequest, ScanState, Snapshot},
    scanner,
    shutdown::SignalGuard,
    text::{format_bytes, format_count, format_rate, pad, truncate},
    ui::{self, Action, App},
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SortArg {
    Io,
    Read,
    Write,
    Total,
    Pid,
    Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ViewArg {
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
struct Args {
    /// Block device to show, for example nvme0n1 or sda; 'all' adds physical disks
    #[arg(short = 'D', long, value_parser = parse_device)]
    device: Option<String>,
    /// Refresh interval in seconds (0.1 to 60)
    #[arg(short = 'd', long, value_parser = parse_interval)]
    interval: Option<f64>,
    /// Visible history in seconds
    #[arg(long, value_parser = clap::value_parser!(u16).range(10..=600))]
    history: Option<u16>,
    /// Process sort order
    #[arg(short, long, value_enum)]
    sort: Option<SortArg>,
    /// View: storage space (default), home directory growth, or I/O
    #[arg(long, value_enum)]
    view: Option<ViewArg>,
    /// Directory tree for the home view (default /home)
    #[arg(long, value_parser = parse_home_path)]
    home_path: Option<String>,
    /// With --once or --json: wait for a complete home scan and include it
    #[arg(long)]
    scan_home: bool,
    /// Also show loop, ram and zram devices
    #[arg(long)]
    all_devices: bool,
    /// Disable colors (also available in F2 Setup)
    #[arg(long)]
    no_color: bool,
    /// Do not use the installed helper; show only your own processes
    #[arg(long)]
    no_helper: bool,
    /// Print one snapshot after a refresh interval and exit
    #[arg(long)]
    once: bool,
    /// Print one snapshot as JSON and exit
    #[arg(long)]
    json: bool,
    /// Explicit preview with synthetic data, clearly labeled DEMO
    #[arg(long)]
    demo: bool,
}

fn parse_interval(value: &str) -> Result<f64, String> {
    let interval: f64 = value
        .parse()
        .map_err(|_| "expected seconds, for example 0.5".to_string())?;
    if !interval.is_finite() || !(0.1..=60.0).contains(&interval) {
        return Err("interval must be between 0.1 and 60 seconds".into());
    }
    Ok(interval)
}

fn parse_device(value: &str) -> Result<String, String> {
    let name = value.strip_prefix("/dev/").unwrap_or(value);
    if name == "all" || diskstats::valid_name(name) {
        Ok(name.to_owned())
    } else {
        Err("expected a block device name such as nvme0n1, or 'all'".into())
    }
}

fn parse_home_path(value: &str) -> Result<String, String> {
    let trimmed = value.trim_end_matches('/');
    let path = if trimmed.is_empty() { "/" } else { trimmed };
    scanner::validate_root(path)
        .map(|()| path.to_owned())
        .map_err(|error| format!("{error:#}"))
}

enum Source {
    Live(Box<Collector>),
    /// `outdated` is set once a reply lacked fields that newer helpers send.
    /// Home roots the helper may not scan are scanned by `local_home`
    /// without privileges, as without the helper; `local` is true while it
    /// does.
    Helper {
        client: Client,
        sysfs: diskstats::Sysfs,
        outdated: bool,
        local_home: Box<HomeScan>,
        local: bool,
    },
    Demo {
        tick: u64,
        start: Instant,
    },
    #[cfg(test)]
    Failing {
        helper: bool,
    },
}

impl Source {
    fn sample(&mut self, request: &HomeRequest, shutdown: &SignalGuard) -> Result<Snapshot> {
        match self {
            Self::Live(collector) => collector.sample(request),
            Self::Helper {
                client,
                sysfs,
                outdated,
                local_home,
                local,
            } => {
                let mut snapshot = client.sample(request, || shutdown.requested())?;
                // An older helper sends no fill levels; they need no privilege.
                *outdated |= complete_storage(sysfs, &mut snapshot);
                let refused = snapshot
                    .home
                    .as_ref()
                    .is_some_and(|status| status.error_kind == Some(HomeErrorKind::NotAllowed));
                if refused {
                    // Each scanner numbers its trees itself; a switch starts
                    // from zero so the current tree is delivered.
                    let mut own = request.clone();
                    if !*local {
                        own.known_generation = 0;
                    }
                    let (status, tree) = local_home.sample(&own);
                    snapshot.home = status.map(|mut status| {
                        status.helper_refused = true;
                        status
                    });
                    snapshot.home_tree = tree;
                    *local = true;
                } else if *local {
                    *local = false;
                    **local_home = HomeScan::unprivileged();
                    if request.scan && snapshot.home_tree.is_none() {
                        let again = HomeRequest {
                            known_generation: 0,
                            rescan: false,
                            ..request.clone()
                        };
                        let retry = client.sample(&again, || shutdown.requested())?;
                        snapshot.home = retry.home;
                        snapshot.home_tree = retry.home_tree;
                    }
                }
                Ok(snapshot)
            }
            Self::Demo { tick, start } => {
                let snapshot = demo::snapshot(
                    *tick,
                    start.elapsed().as_secs_f64(),
                    &request.root,
                    request.known_generation,
                );
                *tick += 1;
                Ok(snapshot)
            }
            #[cfg(test)]
            Self::Failing { .. } => bail!("injected sample failure"),
        }
    }

    /// A revoked, outdated, stalled or crashed helper must not stop the
    /// monitor; direct, unprivileged collection replaces it.
    fn is_helper(&self) -> bool {
        match self {
            Self::Helper { .. } => true,
            #[cfg(test)]
            Self::Failing { helper } => *helper,
            _ => false,
        }
    }
}

/// Startup notices: English on stderr, the UI language in the terminal.
enum Notice {
    Settings(String),
    UnknownKeys(Vec<String>),
    DeviceUnavailable(String),
    HelperOutdated,
}

impl Notice {
    fn text(&self, lang: Lang) -> String {
        match self {
            Self::Settings(error) => lang.settings_problem(error),
            Self::UnknownKeys(keys) => lang.unknown_keys(keys),
            Self::DeviceUnavailable(name) => lang.device_unavailable(name),
            Self::HelperOutdated => lang.helper_outdated(),
        }
    }
}

/// Apply command-line options and NO_COLOR to a copy of the saved settings.
/// These overrides apply to this run only and are never saved by F12.
fn runtime_settings(args: &Args, saved: &Settings, no_color_env: bool) -> Settings {
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

fn direct_collector() -> Result<Source> {
    Ok(Source::Live(Box::new(Collector::new(false).context(
        "cannot initialize Linux block device counters",
    )?)))
}

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

/// State of the terminal input (standard input) after waiting for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    Ready,
    Idle,
    /// The terminal hung up: its window closed or the PTY master went away.
    HangUp,
}

/// Wait up to `timeout` for terminal input. A hung-up terminal reports
/// POLLHUP/POLLERR and end of file; crossterm 0.29 retries such reads in an
/// endless loop, so it must only read once this check has passed.
fn wait_for_input(timeout: Duration) -> Input {
    let mut descriptor = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid pollfd for the duration of the call.
    let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
    if ready < 0 {
        // EINTR from a signal: the caller checks the shutdown flag.
        return Input::Idle;
    }
    if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        Input::HangUp
    } else if descriptor.revents & libc::POLLIN != 0 {
        Input::Ready
    } else {
        Input::Idle
    }
}

fn terminal_input() -> Input {
    wait_for_input(Duration::ZERO)
}

/// Safety net for the short window between `wait_for_input` and crossterm's
/// read: if the terminal hangs up and the monitor has not finished within two
/// seconds, exit like SIGHUP instead of spinning on end of file. The helper
/// then sees end of file on its private pipe and exits by itself.
fn spawn_hangup_watchdog(shutdown: &SignalGuard) -> Arc<AtomicBool> {
    let finished = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&finished);
    let requested = shutdown.flag();
    let spawned = std::thread::Builder::new()
        .name("dwtop-hangup".into())
        .spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let mut descriptor = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: 0,
                    revents: 0,
                };
                // SAFETY: one valid pollfd for the duration of the call.
                let ready = unsafe { libc::poll(&mut descriptor, 1, 250) };
                let hung_up = ready > 0
                    && descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
                if !hung_up {
                    continue;
                }
                requested.store(true, Ordering::Relaxed);
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                std::process::exit(0);
            }
        });
    if spawned.is_err() {
        // Without the safety net the main loop still detects hangups itself.
        finished.store(true, Ordering::Relaxed);
    }
    finished
}

/// Wait without blocking termination signals.
fn wait(duration: Duration, shutdown: &SignalGuard) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if shutdown.requested() {
            return false;
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        );
    }
    !shutdown.requested()
}

fn print_once(
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
fn sample_once(
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
fn ignore_closed_output(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

/// Take one sample. Failures become a visible notice instead of ending the
/// monitor; a failed helper is replaced by `fallback`.
fn refresh(
    source: &mut Source,
    app: &mut App,
    shutdown: &SignalGuard,
    fallback: impl FnOnce() -> Result<Source>,
) {
    let error = match source.sample(&app.take_home_request(), shutdown) {
        Ok(snapshot) => {
            app.update(snapshot);
            return;
        }
        Err(_) if shutdown.requested() => return,
        Err(error) => error,
    };
    let lang = app.lang();
    if !source.is_helper() {
        app.show_error(lang.sample_failed(&lang.error_text(&error)));
        return;
    }
    match fallback() {
        Ok(replacement) => {
            // Dropping the client also stops and reaps the failed helper.
            *source = replacement;
            app.home_source_changed();
            match source.sample(&app.take_home_request(), shutdown) {
                Ok(snapshot) => app.update(snapshot),
                Err(_) if shutdown.requested() => return,
                Err(retry) => {
                    app.show_error(lang.sample_failed(&lang.error_text(&retry)));
                    return;
                }
            }
            app.show_error(lang.helper_failed(&lang.error_text(&error)));
        }
        Err(fallback_error) => app.show_error(
            lang.sample_failed(&format!("{}; {fallback_error:#}", lang.error_text(&error))),
        ),
    }
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    source: &mut Source,
    app: &mut App,
    config: Option<&ConfigFile>,
    shutdown: &SignalGuard,
) -> Result<()> {
    let mut next_sample = Instant::now() + Duration::from_millis(app.settings.interval_ms);
    'running: loop {
        if shutdown.requested() {
            break;
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        // Poll frequently for external termination without repainting between
        // events or samples. Even a 60-second interval exits promptly.
        let event_ready = loop {
            if shutdown.requested() {
                break 'running;
            }
            let remaining = next_sample.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break false;
            }
            if wait_for_input(remaining.min(Duration::from_millis(100))) == Input::HangUp {
                // A closed terminal window ends the monitor like SIGHUP.
                shutdown.request();
                break 'running;
            }
            // Keys, and resizes from crossterm's own signal pipe; the input
            // was checked above, so this read cannot spin on end of file.
            if event::poll(Duration::ZERO)? {
                break true;
            }
        };
        if event_ready {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match app.handle_key(key) {
                    Action::Quit => break,
                    Action::DeviceChanged => {}
                    Action::HomeChanged => {
                        refresh(source, app, shutdown, direct_collector);
                        next_sample =
                            Instant::now() + Duration::from_millis(app.settings.interval_ms);
                    }
                    Action::SettingsChanged => {
                        next_sample = next_sample
                            .min(Instant::now() + Duration::from_millis(app.settings.interval_ms));
                    }
                    Action::SaveSettings => save_settings(app, config),
                    Action::None => {}
                },
                _ => {}
            }
        }
        if Instant::now() >= next_sample {
            refresh(source, app, shutdown, direct_collector);
            next_sample = Instant::now() + Duration::from_millis(app.settings.interval_ms);
        }
    }
    Ok(())
}

fn save_settings(app: &mut App, config: Option<&ConfigFile>) {
    let lang = app.lang();
    let settings = app.settings_to_save();
    match config
        .context("settings path unavailable")
        .and_then(|config| config.save(&settings).map(|()| config.path()))
    {
        Ok(path) => app.settings_saved(lang.saved(&path.display().to_string())),
        Err(error) if error.downcast_ref::<RootWithForeignSettings>().is_some() => {
            app.show_error(lang.root_with_foreign_settings())
        }
        Err(error) => app.show_error(lang.save_failed(&format!("{error:#}"))),
    }
}

fn optional_rate(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), format_rate)
}

fn print_snapshot(out: &mut impl Write, app: &App, snapshot: &Snapshot) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

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
}
