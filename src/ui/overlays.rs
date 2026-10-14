//! Setup, help and device/view/sort dialogs.

use super::summary::{combined_usage, physical_disks};
use super::{App, KEY, SORTS, VIEWS, categories, draw_key_bar, inverse, us_label, view_label};
use crate::config::{GraphStyle, Language, PlotColor, View};
use crate::diskstats;
use crate::i18n::Lang;
use crate::model::{Device, DeviceRates};
use crate::text::{Columns, format_bytes, format_duration, format_rate, truncate_left};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap};

pub(super) fn plot_color_label(color: PlotColor, lang: Lang) -> &'static str {
    match (color, lang) {
        (_, Lang::En) | (PlotColor::Cyan | PlotColor::Magenta, _) => color.label(),
        (PlotColor::Green, Lang::De) => "Grün",
        (PlotColor::Yellow, Lang::De) => "Gelb",
        (PlotColor::Red, Lang::De) => "Rot",
        (PlotColor::Blue, Lang::De) => "Blau",
        (PlotColor::White, Lang::De) => "Weiß",
    }
}

pub(super) fn setup_options(app: &App) -> Vec<String> {
    let lang = app.lang();
    let check = |enabled| if enabled { "[*]" } else { "[ ]" };
    match app.setup_category {
        0 => vec![
            format!(
                "{} {}",
                check(app.settings.color),
                lang.pick("Color", "Farbe")
            ),
            format!(
                "[{:.1}s] {}",
                app.settings.interval_ms as f64 / 1000.0,
                lang.pick("Update interval", "Aktualisierung")
            ),
            format!(
                "[{}] {}",
                match app.settings.language {
                    Language::Auto => "Auto",
                    Language::En => "English",
                    Language::De => "Deutsch",
                },
                lang.pick("Language", "Sprache")
            ),
        ],
        1 => {
            let mut rows = vec![
                format!(
                    "{} {}",
                    check(app.settings.show_virtual),
                    lang.pick("Show loop, ram, zram", "loop, ram, zram zeigen")
                ),
                format!(
                    "{} {}",
                    check(app.settings.show_stacked),
                    lang.pick("Show stacked devices", "Gestapelte Geräte zeigen")
                ),
                format!(
                    "{} Auto ({})",
                    check(app.settings.device.is_none()),
                    app.auto_device
                        .as_deref()
                        .unwrap_or(lang.pick("all", "alle"))
                ),
                format!(
                    "{} {}",
                    check(app.settings.device.as_deref() == Some("all")),
                    lang.pick("No focus (all disks)", "Kein Fokus (alle Disks)")
                ),
            ];
            rows.extend(app.visible_devices().into_iter().map(|device| {
                format!(
                    "{} {}{} ({})",
                    check(app.settings.device.as_deref() == Some(&device.name)),
                    if device.partition { "  " } else { "" },
                    device.name,
                    device.kind.label()
                )
            }));
            rows
        }
        2 => vec![
            format!(
                "{} {}",
                check(app.settings.show_graph),
                lang.pick("Show graph", "Diagramm zeigen")
            ),
            format!(
                "[{}s] {}",
                app.settings.history_seconds,
                lang.pick("History", "Verlauf")
            ),
            format!(
                "[{}] {}",
                match (app.settings.graph_style, lang) {
                    (GraphStyle::Steps, Lang::De) => "Stufen",
                    (style, _) => style.label(),
                },
                lang.pick("Drawing", "Zeichnung")
            ),
            format!(
                "[{}] {}",
                plot_color_label(app.settings.read_color, lang),
                lang.pick("Read color", "Lesefarbe")
            ),
            format!(
                "[{}] {}",
                plot_color_label(app.settings.write_color, lang),
                lang.pick("Write color", "Schreibfarbe")
            ),
            format!(
                "{} {}",
                check(app.settings.show_io_series),
                lang.pick("Graph: IO utilization", "Diagramm: IO-Auslastung")
            ),
            format!(
                "{} {}",
                check(app.settings.show_usage_series),
                lang.pick("Graph: fill level (US)", "Diagramm: Füllstand (BL)")
            ),
            format!(
                "[{}] {}",
                plot_color_label(app.settings.usage_color, lang),
                lang.pick("Fill level color", "Füllstandsfarbe")
            ),
        ],
        3 => vec![
            format!(
                "[{}] {}",
                view_label(app.settings.view, lang),
                lang.pick("View", "Ansicht")
            ),
            format!(
                "[{}] {}",
                app.settings.sort.label(lang),
                lang.pick("Sort by", "Sortierung")
            ),
            format!(
                "{} {}",
                check(app.settings.show_idle),
                lang.pick("Show idle processes", "Inaktive Prozesse zeigen")
            ),
            format!(
                "[{}] {}",
                app.settings.process_rows,
                lang.pick("Process rows", "Prozesszeilen")
            ),
        ],
        _ => vec![
            match &app.editing {
                Some(buffer) => format!("[{buffer}_] {}", lang.pick("Home path", "Home-Pfad")),
                None => format!(
                    "[{}] {}",
                    app.settings.home_path,
                    lang.pick("Home path", "Home-Pfad")
                ),
            },
            format!(
                "[{}] {}",
                format_duration(u64::from(app.settings.rescan_minutes) * 60),
                lang.pick("Rescan interval", "Neuer Scan alle")
            ),
            format!(
                "[{}] {}",
                if app.settings.apparent_size {
                    lang.pick("Apparent", "Scheinbar")
                } else {
                    lang.pick("Allocated", "Belegt")
                },
                lang.pick("Sizes", "Größen")
            ),
            lang.pick("Rescan now", "Jetzt neu scannen").to_owned(),
        ],
    }
}

pub(super) fn setup_description(app: &App) -> &'static str {
    let lang = app.lang();
    match (app.setup_category, app.setup_option) {
        (0, 0) => lang.pick(
            "Use the terminal's own ANSI palette. Selection stays visible in monochrome.",
            "Nutzt die ANSI-Palette des Terminals. Die Auswahl bleibt auch einfarbig sichtbar.",
        ),
        (0, 1) => lang.pick(
            "Refresh the display every 0.1 to 60 seconds. +/- changes by 0.1s.",
            "Aktualisiert alle 0,1 bis 60 Sekunden. +/- ändert um 0,1 s.",
        ),
        (0, _) => lang.pick(
            "Auto follows the system language (LANG). English or German can be fixed here.",
            "Auto folgt der Systemsprache (LANG). Englisch oder Deutsch lässt sich festlegen.",
        ),
        (1, 1) => lang.pick(
            "Show dm/md devices (LUKS, LVM, RAID) on a single shown disk as own blocks; that disk already counts them.",
            "Zeigt dm/md-Geräte (LUKS, LVM, RAID) auf nur einer Disk als eigene Blöcke; die Disk zählt sie schon.",
        ),
        (1, 0) => lang.pick(
            "Loop, ram and zram devices (snaps, swap) are hidden unless enabled here.",
            "Loop-, RAM- und zram-Geräte (Snaps, Swap) bleiben ohne diese Option verborgen.",
        ),
        (1, _) => lang.pick(
            "Enter focuses a device in the overview. Auto follows the disk of the home path.",
            "Enter fokussiert ein Gerät der Übersicht. Auto folgt der Disk des Home-Pfads.",
        ),
        (2, 0) => lang.pick(
            "Hide the per-disk graphs to fit more disks and processes.",
            "Blendet die Diagramme je Disk aus, damit mehr Disks und Prozesse passen.",
        ),
        (2, 1) => lang.pick(
            "Show 10 to 600 seconds of history. +/- changes by 10 seconds.",
            "Zeigt 10 bis 600 Sekunden Verlauf. +/- ändert um 10 Sekunden.",
        ),
        (2, 2) => lang.pick(
            "Steps uses nvtop-style terminal lines. Braille draws a finer curve.",
            "Stufen nutzt Terminal-Linien wie nvtop. Braille zeichnet eine feinere Kurve.",
        ),
        (2, 5) => lang.pick(
            "Utilization over time in each disk graph, on the shared 0-100% scale.",
            "Auslastung im Verlauf jedes Disk-Diagramms, auf der gemeinsamen 0-100-%-Skala.",
        ),
        (2, 6) => lang.pick(
            "Fill level of the mounted filesystems over time, on the same 0-100% scale.",
            "Füllstand der eingehängten Dateisysteme im Verlauf, auf derselben 0-100-%-Skala.",
        ),
        (2, _) => lang.pick(
            "Cycle through the terminal's standard colors with Enter or +/-.",
            "Wechselt mit Enter oder +/- durch die Standardfarben des Terminals.",
        ),
        (3, 0) => lang.pick(
            "Space, home directory growth or I/O. 1/2/3 select, F4 opens the view menu.",
            "Speicher, Home-Wachstum oder I/O. 1/2/3 wählen, F4 öffnet das Ansichtsmenü.",
        ),
        (3, 1) => lang.pick(
            "Choose the process sort order. F6 opens the same choices in the monitor.",
            "Wählt die Sortierung der Prozesse. F6 bietet dieselbe Auswahl im Monitor.",
        ),
        (3, 2) => lang.pick(
            "Include processes without storage I/O in the current interval.",
            "Zeigt auch Prozesse ohne Speicher-I/O im aktuellen Intervall.",
        ),
        (3, _) => lang.pick(
            "Process rows below the disks, 0 to 20 (0 hides them). Rows the disks leave free are added.",
            "Prozesszeilen unter den Disks, 0 bis 20 (0 blendet aus). Freie Zeilen kommen hinzu.",
        ),
        (_, 0) => lang.pick(
            "Enter edits the absolute path to scan; Enter applies, Esc cancels.",
            "Enter bearbeitet den absoluten Scan-Pfad; Enter übernimmt, Esc bricht ab.",
        ),
        (_, 1) => lang.pick(
            "Time between background scans, from 1 minute to 24 hours.",
            "Zeit zwischen Hintergrund-Scans, von 1 Minute bis 24 Stunden.",
        ),
        (_, 2) => lang.pick(
            "Allocated counts disk blocks (du); apparent counts file lengths (du -b). Growth always counts allocated blocks.",
            "Belegt zählt Plattenblöcke (du); scheinbar zählt Dateilängen (du -b). Wachstum zählt immer belegte Blöcke.",
        ),
        _ => lang.pick(
            "Start a new scan now. The previous result stays visible meanwhile.",
            "Startet sofort einen neuen Scan. Das letzte Ergebnis bleibt sichtbar.",
        ),
    }
}

pub(super) fn draw_setup(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lang = app.lang();
    let names = categories(lang);
    frame.render_widget(Clear, area);
    let vertical = Layout::vertical([
        Constraint::Min(6),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    // Room for the longest category and the focus marker " >".
    let category_width = names
        .iter()
        .map(|name| name.columns() as u16 + 2)
        .max()
        .unwrap_or(0)
        .max(12);
    let panes = Layout::horizontal([
        Constraint::Length(category_width),
        Constraint::Length(1),
        Constraint::Min(20),
    ])
    .split(vertical[0]);
    frame.render_widget(
        Paragraph::new("Setup").style(inverse(Color::Green)),
        Rect::new(panes[0].x, panes[0].y, panes[0].width, 1),
    );
    for (index, name) in names.iter().enumerate() {
        let current = app.setup_category == index;
        let style = if current && !app.setup_focus {
            inverse(KEY)
        } else if current {
            Style::default().fg(KEY).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let text = if current && app.setup_focus {
            format!("{name} >")
        } else {
            (*name).into()
        };
        frame.render_widget(
            Paragraph::new(text).style(style),
            Rect::new(panes[0].x, panes[0].y + index as u16 + 1, panes[0].width, 1),
        );
    }
    let options = setup_options(app);
    let name = names[app.setup_category];
    let header = match lang {
        Lang::En => format!("{name} Options"),
        Lang::De => format!("Optionen: {name}"),
    };
    // Narrow terminals show just the category rather than a cut-off header.
    let header = if header.columns() > usize::from(panes[2].width) {
        name.to_owned()
    } else {
        header
    };
    let option_width = usize::from(panes[2].width);
    let mut state =
        TableState::default().with_selected(app.setup_focus.then_some(app.setup_option));
    let table = Table::new(
        options
            .into_iter()
            .map(|row| Row::new([truncate_left(&row, option_width)])),
        [Constraint::Min(1)],
    )
    .header(Row::new([header]).style(inverse(Color::Green)))
    .row_highlight_style(inverse(KEY));
    frame.render_stateful_widget(table, panes[2], &mut state);
    frame.render_widget(
        // Default foreground keeps the description readable on light themes.
        Paragraph::new(setup_description(app)).wrap(Wrap { trim: true }),
        vertical[1],
    );
    let message = app.notice.clone().unwrap_or_else(|| {
        if app.editing.is_some() {
            lang.pick(
                "Editing: Enter applies, Esc cancels",
                "Bearbeiten: Enter übernimmt, Esc bricht ab",
            )
            .into()
        } else if app.settings_dirty() {
            lang.pick(
                "Unsaved changes - F12 saves for next start",
                "Ungespeichert - F12 speichert für den nächsten Start",
            )
            .into()
        } else {
            lang.pick(
                "Arrows navigate | Enter / +/- change",
                "Pfeile wählen | Enter / +/- ändern",
            )
            .into()
        }
    });
    frame.render_widget(
        Paragraph::new(message).style(Style::default().fg(
            if app.notice_error && app.notice.is_some() {
                Color::Red
            } else {
                Color::Yellow
            },
        )),
        vertical[2],
    );
    draw_key_bar(
        frame,
        vertical[3],
        &[
            ("Tab", lang.pick("Panel", "Feld")),
            ("Ent", lang.pick("Change", "Ändern")),
            ("F10", lang.pick("Done", "Fertig")),
            (
                "F12",
                if area.width >= 40 {
                    lang.pick("Save", "Speichern")
                } else {
                    lang.pick("Save", "Speich")
                },
            ),
        ],
    );
}

pub(super) fn draw_sort(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let area = popup(area, 32, 10);
    frame.render_widget(Clear, area);
    let lang = app.lang();
    let mut state = TableState::default().with_selected(app.picker.min(SORTS.len() - 1));
    let rows = SORTS.iter().map(|sort| Row::new([sort.label(lang)]));
    let table = Table::new(rows, [Constraint::Min(1)])
        .header(
            Row::new([lang.pick("Sort processes by", "Prozesse sortieren")])
                .style(inverse(Color::Green)),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(lang.pick(" Enter select / Esc close ", " Enter wählt / Esc schließt ")),
        )
        .row_highlight_style(inverse(KEY));
    frame.render_stateful_widget(table, area, &mut state);
}

pub(super) fn draw_views(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let area = popup(area, 46, 7);
    frame.render_widget(Clear, area);
    let lang = app.lang();
    let descriptions = match lang {
        Lang::En => [
            "1 Space: used / free / history",
            "2 Home: directory sizes / growth",
            "3 I/O: activity / processes",
        ],
        Lang::De => [
            "1 Speicher: belegt / frei / Verlauf",
            "2 Home: Ordnergrößen / Wachstum",
            "3 I/O: Aktivität / Prozesse",
        ],
    };
    let mut state = TableState::default().with_selected(app.picker.min(VIEWS.len() - 1));
    let table = Table::new(
        descriptions.into_iter().map(|text| Row::new([text])),
        [Constraint::Min(1)],
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(lang.pick(" Views · Enter / Esc ", " Ansichten · Enter / Esc ")),
    )
    .row_highlight_style(inverse(KEY));
    frame.render_stateful_widget(table, area, &mut state);
}

pub(super) fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

const HELP_EN: &[&str] = &[
    "F1 / ?         Help     Esc closes",
    "F2             Setup: General, Device, Chart, Table, Home",
    "F3 / /         Search the current table",
    "F4 / v         View menu: 1 Space, 2 Home, 3 I/O",
    "F5 / d         Focus a device; Tab / Shift-Tab cycle disks",
    "F6             Choose process sorting; s cycles it",
    "F9 / Space     Pause / resume display",
    "F12            Save current settings for next start",
    "Up / Down      Select; PgUp / PgDn scroll",
    "Home view      Enter/Right open, Backspace/Left up,",
    "               r rescan, a allocated/apparent size",
    "q / F10 / Ctrl-C Quit",
    "",
    "Space starts with used/free capacity and used-space history.",
    "2 opens directory sizes/growth; 3 or i opens I/O.",
    "Esc returns from I/O to Space.",
    "In I/O, every disk has its own block:",
    "RD read and WR write (green / yellow), all disks on one",
    "scale: the highest rate of any shown disk in the visible",
    "history, so bars compare across disks; IO utilization;",
    "US fill level of the filesystems mounted from the disk,",
    "its partitions and dm/md devices (red from 90%). Not",
    "mounted disks say so; swap-only disks show swap.",
    "The graph shows IO (cyan) and US over the history on",
    "one 0-100% scale; F2 Chart hides either series.",
    "dm/md devices on one shown disk (LUKS, LVM) are part of",
    "that disk; F2 Device shows them as own blocks. RAID and",
    "LVM across several disks always get their own block.",
    "Small terminals use shorter graphs, then meters only,",
    "then one row per disk; '+N more' rows scroll with Tab.",
    "The focused disk is shown inverted; the row below the",
    "title lists its IOPS, await, queue depth and totals.",
    "Device rates come from /proc/diskstats (512-byte sectors).",
    "Utilization is time with I/O in flight; await is average",
    "request time; qd is the average queue depth.",
    "Without focus, all adds physical whole disks; partitions,",
    "dm and md would count the same I/O twice.",
    "Process READ/WRITE are bytes a process caused to be read",
    "from or dirtied for storage, across all devices. They",
    "cannot be split per device and exclude cache hits.",
    "Writes count when data is dirtied, not at writeback.",
    "CANCEL: dirty data truncated before writeback; charged",
    "to the truncating process, so it is not subtracted.",
    "Processes that exit between refreshes are not listed;",
    "their I/O remains in the device rates.",
    "F2 Table sets the process rows (0-20) below the disks.",
    "Without the helper only your own processes are shown.",
    "Enable all once: ./scripts/setup-helper.sh",
    "* marks the filesystem that contains the home path.",
    "Home: one filesystem, no symlinks, hard links once.",
    "Sizes are allocated blocks (du) unless apparent is set.",
    "Growth always measures allocated disk blocks.",
    "! marks directories that were partly unreadable.",
    "Command-line options and NO_COLOR apply only to this run.",
    "",
];

const HELP_DE: &[&str] = &[
    "F1 / ?         Hilfe    Esc schließt",
    "F2             Setup: Allgemein, Gerät, Diagramm, Tabelle, Home",
    "F3 / /         Aktuelle Tabelle durchsuchen",
    "F4 / v         Ansichtsmenü: 1 Speicher, 2 Home, 3 I/O",
    "F5 / d         Gerät fokussieren; Tab / Umschalt-Tab wechselt",
    "F6             Prozess-Sortierung wählen; s wechselt",
    "F9 / Leertaste Anzeige anhalten / fortsetzen",
    "F12            Aktuelle Einstellungen für den nächsten Start speichern",
    "Hoch / Runter  Auswählen; Bild hoch / runter blättert",
    "Home-Ansicht   Enter/Rechts öffnet, Rücktaste/Links hoch,",
    "               r neu scannen, a belegt/scheinbar",
    "q / F10 / Strg-C Beenden",
    "",
    "Speicher zeigt Belegung, freien Platz und Belegungsverlauf.",
    "2 öffnet Ordner/Wachstum; 3 oder i öffnet I/O.",
    "Esc wechselt von I/O zurück zu Speicher.",
    "In I/O hat jede Disk einen eigenen Block wie die CPU-Anzeigen",
    "von htop: LE Lesen und SC Schreiben (grün / gelb), alle",
    "Disks auf einer Skala: höchste Rate aller gezeigten Disks",
    "im sichtbaren Verlauf, Balken sind vergleichbar;",
    "IO Auslastung; BL Füllstand der Dateisysteme der Disk,",
    "ihrer Partitionen und dm/md-Geräte (rot ab 90 %).",
    "Nicht eingehängte Disks sind markiert, reine Swap-Disks",
    "zeigen Swap. Das Diagramm zeigt IO (cyan) und BL im",
    "Verlauf auf einer 0-100-%-Skala; F2 Diagramm blendet",
    "jede Reihe aus. dm/md-Geräte auf nur einer Disk (LUKS,",
    "LVM) gehören zu dieser Disk; F2 Gerät zeigt sie einzeln.",
    "RAID und LVM über mehrere Disks haben immer einen Block.",
    "Kleine Terminals kürzen erst die",
    "Diagramme, zeigen dann nur Balken, dann eine Zeile je",
    "Disk; '+N weitere' blättert mit Tab.",
    "Die fokussierte Disk ist invertiert; die Zeile unter dem",
    "Titel zeigt ihre IOPS, await, Warteschlange und Summen.",
    "Geräteraten stammen aus /proc/diskstats (512-Byte-Sektoren).",
    "Auslastung ist die Zeit mit laufender I/O; await ist die",
    "mittlere Anfragezeit; qd die mittlere Warteschlangentiefe.",
    "Ohne Fokus summiert alle physische Disks; Partitionen,",
    "dm und md würden dieselbe I/O doppelt zählen.",
    "Prozess-LESEN/SCHREIB sind Bytes, die ein Prozess vom",
    "Speicher lesen ließ oder dafür schrieb, über alle Geräte.",
    "Sie lassen sich nicht pro Gerät aufteilen; Cache-Treffer",
    "zählen nicht. Schreiben zählt beim Ändern im Cache.",
    "VERWORF: vor dem Rückschreiben abgeschnittene Daten; sie",
    "zählen beim abschneidenden Prozess und werden nicht",
    "abgezogen. Zwischen zwei Messungen beendete Prozesse",
    "fehlen; ihre I/O bleibt in den Geräteraten.",
    "F2 Tabelle legt die Prozesszeilen (0-20) unter den Disks fest.",
    "Ohne Helper erscheinen nur eigene Prozesse.",
    "Alle einmalig aktivieren: ./scripts/setup-helper.sh",
    "* markiert das Dateisystem mit dem Home-Pfad.",
    "Home: ein Dateisystem, keine Symlinks, Hardlinks einmal.",
    "Größen sind belegte Blöcke (du), außer scheinbar ist aktiv.",
    "Wachstum misst immer belegte Plattenblöcke.",
    "! markiert teilweise unlesbare Verzeichnisse.",
    "Kommandozeilenoptionen und NO_COLOR gelten nur für diesen Lauf.",
    "",
];

pub(super) fn draw_help(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let area = popup(area, 78, 26);
    frame.render_widget(Clear, area);
    let lang = app.lang();
    let lines = match lang {
        Lang::En => HELP_EN,
        Lang::De => HELP_DE,
    };
    let mut text: Vec<Line<'_>> = lines.iter().map(|line| Line::from(*line)).collect();
    text.push(Line::from(app.snapshot.process_access.message.as_str()));
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((app.help_scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(KEY))
                    .title(lang.pick(
                        " Help  Up/Down scroll / Esc close ",
                        " Hilfe  Hoch/Runter blättert / Esc schließt ",
                    )),
            ),
        area,
    );
}

pub(super) fn draw_devices(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let devices = app.visible_devices();
    let area = popup(area, 76, (devices.len() as u16 + 5).min(26));
    frame.render_widget(Clear, area);
    let lang = app.lang();
    let rate = |rates: Option<&DeviceRates>| {
        rates.map_or_else(
            || "-".to_owned(),
            |rates| {
                format!(
                    "{} {}",
                    format_rate(rates.read_rate),
                    format_rate(rates.write_rate)
                )
            },
        )
    };
    let used = |device: &Device| {
        device.usage.as_ref().map_or_else(
            || "-".to_owned(),
            |usage| match usage.percent() {
                Some(percent) => format!("{percent:.0}%"),
                None if usage.filesystems == 0 && usage.unmeasured == 0 => {
                    lang.pick("no fs", "kein FS").to_owned()
                }
                None => "-".to_owned(),
            },
        )
    };
    let total = diskstats::aggregate(&app.snapshot.devices);
    let mut rows = vec![Row::new(vec![
        lang.pick("all", "alle").to_string(),
        lang.pick("no focus", "kein Fokus").to_string(),
        String::new(),
        if app.settings.view == View::Processes {
            rate(total.as_ref())
        } else {
            let (used, total) = combined_usage(app);
            if total == 0 {
                "-".into()
            } else {
                format!(
                    "{}{}",
                    format_bytes(total.saturating_sub(used) as f64),
                    if physical_disks(app).any(|device| device
                        .usage
                        .as_ref()
                        .is_none_or(|usage| usage.unmeasured > 0))
                    {
                        "!"
                    } else {
                        ""
                    }
                )
            }
        },
    ])];
    rows.extend(devices.iter().map(|device| {
        Row::new(vec![
            format!("{}{}", if device.partition { " " } else { "" }, device.name),
            format!(
                "{} {}",
                device.kind.label(),
                device
                    .size_bytes
                    .map(|size| format_bytes(size as f64))
                    .unwrap_or_default()
            ),
            used(device),
            if app.settings.view == View::Processes {
                rate(device.rates.as_ref())
            } else {
                device
                    .usage
                    .as_ref()
                    .filter(|usage| usage.percent().is_some())
                    .map_or_else(
                        || "-".into(),
                        |usage| {
                            format!(
                                "{}{}",
                                format_bytes(usage.available_bytes as f64),
                                if usage.unmeasured > 0 { "!" } else { "" }
                            )
                        },
                    )
            },
        ])
    }));
    let mut state = TableState::default().with_selected(app.picker);
    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(15),
            Constraint::Length(7),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new([
            lang.pick("DEVICE", "GERÄT"),
            lang.pick("TYPE / SIZE", "TYP / GRÖSSE"),
            us_label(lang),
            if app.settings.view == View::Processes {
                lang.pick("READ WRITE /s", "LESEN SCHREIB /s")
            } else {
                lang.pick("FREE", "FREI")
            },
        ])
        .style(inverse(Color::Green)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(lang.pick(
                " Focus  Enter select / Esc close ",
                " Fokus  Enter wählt / Esc schließt ",
            ))
            .border_style(Style::default().fg(KEY)),
    )
    .row_highlight_style(inverse(KEY));
    frame.render_stateful_widget(table, area, &mut state);
}
