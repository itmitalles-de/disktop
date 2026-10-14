//! Capacity aggregation and one-line device summaries.

use super::{App, rd_label, us_label, wr_label};
use crate::config::View;
use crate::i18n::Lang;
use crate::model::Device;
use crate::text::{Columns, format_bytes, format_rate, format_short_count, truncate};
use std::collections::BTreeSet;

pub(super) fn fit_pieces(pieces: Vec<String>, width: u16) -> String {
    let mut text = String::new();
    for piece in pieces {
        if text.is_empty() {
            text = piece;
        } else if text.columns() + 2 + piece.columns() <= usize::from(width) {
            text.push_str("  ");
            text.push_str(&piece);
        }
    }
    truncate(&text, usize::from(width))
}

/// Physical whole disks: the devices that "all" adds.
pub(super) fn physical_disks(app: &App) -> impl Iterator<Item = &Device> {
    app.snapshot
        .devices
        .iter()
        .filter(|device| !device.partition && !device.kind.is_virtual())
}

/// Used and total bytes of the physical disks' filesystems. A filesystem on
/// several disks (LVM, MD, btrfs RAID, ZFS pools) counts once; replies from older
/// helpers without the filesystem list are summed per disk.
pub(super) fn combined_usage(app: &App) -> (u64, u64) {
    let mut seen = BTreeSet::new();
    let (mut used, mut total) = (0u64, 0u64);
    let mut add = |filesystem_used: u64, available: u64| {
        used = used.saturating_add(filesystem_used);
        total = total.saturating_add(filesystem_used.saturating_add(available));
    };
    for usage in physical_disks(app).filter_map(|device| device.usage.as_ref()) {
        let keys = if usage.accounting_keys.is_empty() {
            &usage.block_devices
        } else {
            &usage.accounting_keys
        };
        if keys.is_empty() {
            add(usage.used_bytes, usage.available_bytes);
            continue;
        }
        for name in keys {
            if !seen.insert(name.as_str()) {
                continue;
            }
            if let Some((filesystem_used, available)) = app
                .snapshot
                .filesystems
                .iter()
                .filter(|filesystem| filesystem.accounting_key().as_ref() == Some(name))
                .find_map(|filesystem| {
                    filesystem
                        .accounting_usage()
                        .map(|(_, used, available)| (used, available))
                })
            {
                add(filesystem_used, available);
            }
        }
    }
    (used, total)
}

/// The row below the title: details of the focused device, or of all disks.
pub(super) fn summary_line(app: &App, width: u16) -> String {
    let lang = app.lang();
    if app.settings.view != View::Processes {
        let (used, total, partial) = if let Some(name) = &app.device {
            let Some(device) = app.selected_device() else {
                return format!("[{name}] {}", lang.pick("unavailable", "nicht verfügbar"));
            };
            let Some(usage) = device.usage.as_ref() else {
                return lang
                    .pick(
                        "Used -  Free -  (unavailable)",
                        "Belegt -  Frei -  (nicht verfügbar)",
                    )
                    .into();
            };
            if usage.percent().is_none() {
                return if usage.unmeasured > 0 {
                    lang.pick(
                        "Used -  Free -  (unavailable)",
                        "Belegt -  Frei -  (nicht verfügbar)",
                    )
                } else if usage.swap {
                    lang.pick("swap only", "nur Swap")
                } else {
                    lang.pick("not mounted", "nicht eingehängt")
                }
                .into();
            }
            (
                usage.used_bytes,
                usage.used_bytes.saturating_add(usage.available_bytes),
                usage.unmeasured > 0,
            )
        } else {
            let (used, total) = combined_usage(app);
            if total == 0 {
                return lang
                    .pick(
                        "Used -  Free -  (no measured filesystems)",
                        "Belegt -  Frei -  (keine gemessenen Dateisysteme)",
                    )
                    .into();
            }
            let partial = physical_disks(app).any(|device| {
                device
                    .usage
                    .as_ref()
                    .is_none_or(|usage| usage.unmeasured > 0)
            });
            (used, total, partial)
        };
        return fit_pieces(
            vec![
                format!(
                    "{} {}",
                    if partial {
                        lang.pick("Used (partial)", "Belegt (teilweise)")
                    } else {
                        lang.pick("Used", "Belegt")
                    },
                    format_bytes(used as f64)
                ),
                format!(
                    "{} {}",
                    lang.pick("Free", "Frei"),
                    format_bytes(total.saturating_sub(used) as f64)
                ),
                if total > 0 {
                    format!("{:.1}%", used as f64 * 100.0 / total as f64)
                } else {
                    "-".into()
                },
                graph_note(app),
            ],
            width,
        );
    }
    let ms =
        |value: Option<f64>| value.map_or_else(|| "-".to_owned(), |value| format!("{value:.2} ms"));
    let rates = app.selected_rates();
    let mut pieces = Vec::new();
    let Some(name) = &app.device else {
        let disks = physical_disks(app).count();
        pieces.push(match lang {
            Lang::En => format!("[ALL] {disks} disks"),
            Lang::De => format!("[ALLE] {disks} Disks"),
        });
        match &rates {
            Some(rates) => {
                pieces.push(format!(
                    "{} {}",
                    rd_label(lang),
                    format_rate(rates.read_rate)
                ));
                pieces.push(format!(
                    "{} {}",
                    wr_label(lang),
                    format_rate(rates.write_rate)
                ));
                pieces.push(format!(
                    "IOPS {}/{}",
                    format_short_count(rates.read_iops),
                    format_short_count(rates.write_iops)
                ));
                pieces.push(format!(
                    "IO max {:.0}%",
                    rates.utilization_percent.min(100.0)
                ));
            }
            None => pieces.push(
                lang.pick(
                    "no rates in this interval",
                    "keine Raten in diesem Intervall",
                )
                .to_owned(),
            ),
        }
        let (used, total) = combined_usage(app);
        if total > 0 {
            pieces.push(format!(
                "{} {}/{}",
                us_label(lang),
                format_bytes(used as f64),
                format_bytes(total as f64)
            ));
        }
        pieces.push(
            lang.pick(
                "partitions, dm, md not added twice",
                "Partitionen, dm, md nicht doppelt",
            )
            .to_owned(),
        );
        pieces.push(graph_note(app));
        return fit_pieces(pieces, width);
    };
    let Some(device) = app.selected_device() else {
        return truncate(
            &format!(
                "[{name}] {}",
                lang.pick("device unavailable", "Gerät nicht verfügbar")
            ),
            usize::from(width),
        );
    };
    pieces.push(format!("[{name}]"));
    match &rates {
        Some(rates) => {
            pieces.push(format!(
                "IOPS {}/{}",
                format_short_count(rates.read_iops),
                format_short_count(rates.write_iops)
            ));
            pieces.push(format!("await {}", ms(rates.await_ms)));
            pieces.push(format!("qd {:.2}", rates.queue_depth));
            if width >= 100 {
                pieces.push(format!(
                    "r {} w {}",
                    ms(rates.read_await_ms),
                    ms(rates.write_await_ms)
                ));
            }
        }
        None => pieces.push(
            lang.pick(
                "no rates in this interval",
                "keine Raten in diesem Intervall",
            )
            .to_owned(),
        ),
    }
    pieces.push(format!(
        "{} {} {} {} {}",
        lang.pick("total", "gesamt"),
        rd_label(lang),
        format_bytes(device.read_bytes as f64),
        wr_label(lang),
        format_bytes(device.write_bytes as f64)
    ));
    if let Some(sector) = device.hw_sector_size {
        pieces.push(format!("{sector} B {}", lang.pick("sectors", "Sektoren")));
    }
    pieces.push(graph_note(app));
    fit_pieces(pieces, width)
}

pub(super) fn graph_note(app: &App) -> String {
    let lang = app.lang();
    if app.settings.view != View::Processes {
        return match lang {
            Lang::En => format!(
                "used-space history 0-100% over {}s",
                app.settings.history_seconds
            ),
            Lang::De => format!(
                "Belegungsverlauf 0-100% über {}s",
                app.settings.history_seconds
            ),
        };
    }
    match lang {
        Lang::En => format!(
            "graphs: IO / {} 0-100% over {}s",
            us_label(lang),
            app.settings.history_seconds
        ),
        Lang::De => format!(
            "Diagramme: IO / {} 0-100% über {} s",
            us_label(lang),
            app.settings.history_seconds
        ),
    }
}
