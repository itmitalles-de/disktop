//! Terminal polling, refresh, restoration and preference saving.

use super::source::{Source, direct_collector};
use anyhow::{Context, Result};
use crossterm::event;
use crossterm::event::{Event, KeyEventKind};
use dwtop::config::{ConfigFile, RootWithForeignSettings};
use dwtop::shutdown::SignalGuard;
use dwtop::ui;
use dwtop::ui::{Action, App};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// State of the terminal input (standard input) after waiting for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Input {
    Ready,
    Idle,
    /// The terminal hung up: its window closed or the PTY master went away.
    HangUp,
}

/// Wait up to `timeout` for terminal input. A hung-up terminal reports
/// POLLHUP/POLLERR and end of file; crossterm 0.29 retries such reads in an
/// endless loop, so it must only read once this check has passed.
pub(super) fn wait_for_input(timeout: Duration) -> Input {
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

pub(super) fn terminal_input() -> Input {
    wait_for_input(Duration::ZERO)
}

/// Safety net for the short window between `wait_for_input` and crossterm's
/// read: if the terminal hangs up and the monitor has not finished within two
/// seconds, exit like SIGHUP instead of spinning on end of file. The helper
/// then sees end of file on its private pipe and exits by itself.
pub(super) fn spawn_hangup_watchdog(shutdown: &SignalGuard) -> Arc<AtomicBool> {
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
pub(super) fn wait(duration: Duration, shutdown: &SignalGuard) -> bool {
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

/// Take one sample. Failures become a visible notice instead of ending the
/// monitor; a failed helper is replaced by `fallback`.
pub(super) fn refresh(
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

pub(super) fn run(
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

pub(super) fn save_settings(app: &mut App, config: Option<&ConfigFile>) {
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
