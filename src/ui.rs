//! A compact nvtop-style terminal interface. No data is invented by the renderer.

use std::collections::{BTreeSet, VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Cell, Chart, Clear, Dataset, GraphType, Paragraph, Row, Table,
        TableState, Wrap,
    },
};

pub use crate::config::SortKey as Sort;
use crate::{
    config::{Field, GraphStyle, Language, PlotColor, Settings, View},
    diskstats,
    i18n::Lang,
    model::{
        Device, DeviceRates, DirNode, Filesystem, Folded, HomeReport, HomeRequest, ProcessRow,
        ScanState, Snapshot, StopReason,
    },
    scanner,
    text::{
        format_bytes, format_count, format_delta, format_duration, format_rate, format_short_count,
        truncate, truncate_left,
    },
};

const KEY: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const UTIL: Color = Color::Cyan;
const SORTS: [Sort; 6] = [
    Sort::Io,
    Sort::Read,
    Sort::Write,
    Sort::Total,
    Sort::Pid,
    Sort::Name,
];
const CATEGORY_COUNT: usize = 5;
const RESCAN_STEPS: [u16; 11] = [1, 2, 5, 10, 15, 30, 60, 120, 240, 720, 1440];
/// Smallest graph scale, so idle noise does not fill the whole graph.
const GRAPH_FLOOR: f64 = 64.0 * 1024.0;

fn categories(lang: Lang) -> [&'static str; CATEGORY_COUNT] {
    match lang {
        Lang::En => ["General", "Device", "Chart", "Table", "Home"],
        Lang::De => ["Allgemein", "Gerät", "Diagramm", "Tabelle", "Home"],
    }
}

fn inverse(color: Color) -> Style {
    // ncurses A_STANDOUT: reverse the terminal's own foreground/background.
    Style::default().fg(color).add_modifier(Modifier::REVERSED)
}

fn plot_color(color: PlotColor) -> Color {
    match color {
        PlotColor::Green => Color::Green,
        PlotColor::Yellow => Color::Yellow,
        PlotColor::Cyan => Color::Cyan,
        PlotColor::Red => Color::Red,
        PlotColor::Blue => Color::Blue,
        PlotColor::Magenta => Color::Magenta,
        PlotColor::White => Color::White,
    }
}

impl Sort {
    pub fn next(self) -> Self {
        let index = SORTS.iter().position(|sort| *sort == self).unwrap_or(0);
        SORTS[(index + 1) % SORTS.len()]
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Io => lang.pick("I/O rate", "I/O-Rate"),
            Self::Read => lang.pick("read", "Lesen"),
            Self::Write => lang.pick("write", "Schreiben"),
            Self::Total => lang.pick("total", "gesamt"),
            Self::Pid => "PID",
            Self::Name => lang.pick("command", "Befehl"),
        }
    }
}

fn view_label(view: View, lang: Lang) -> &'static str {
    match view {
        View::Processes => lang.pick("Processes", "Prozesse"),
        View::Filesystems => lang.pick("Filesystems", "Dateisysteme"),
        View::Home => "Home",
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    None,
    Help,
    Devices,
    Setup,
    Sort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    DeviceChanged,
    SettingsChanged,
    SaveSettings,
    /// Home root, scan request or rescan: sample now.
    HomeChanged,
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub at: f64,
    pub read: Option<f64>,
    pub write: Option<f64>,
}

/// One row of the home view: a retained directory or a summary.
#[derive(Clone, Debug, PartialEq)]
pub enum HomeEntryKind {
    Directory(u32),
    Files,
    Folded,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HomeEntry {
    pub kind: HomeEntryKind,
    pub name: String,
    pub size: u64,
    pub files: u64,
    pub dirs: u64,
    pub growth_previous: Option<i64>,
    pub growth_start: Option<i64>,
    pub partial: bool,
}

pub struct App {
    pub snapshot: Snapshot,
    /// Selected device; `None` aggregates physical whole disks.
    pub device: Option<String>,
    pub history: VecDeque<Sample>,
    /// Effective runtime settings, including CLI and environment overrides.
    pub settings: Settings,
    /// Settings as stored in the preferences file.
    saved_settings: Settings,
    /// Preferences the user changed in this session; only these are saved.
    touched: BTreeSet<Field>,
    /// Language used when the language preference is Auto.
    pub system_lang: Lang,
    pub auto_device: Option<String>,
    pub notice: Option<String>,
    pub notice_error: bool,
    pub paused: bool,
    pub filter: String,
    pub searching: bool,
    pub overlay: Overlay,
    pub table: TableState,
    picker: usize,
    help_scroll: u16,
    visible_rows: usize,
    pub demo: bool,
    setup_category: usize,
    setup_option: usize,
    setup_focus: bool,
    /// Home path being edited in Setup.
    editing: Option<String>,
    pub home_tree: Option<HomeReport>,
    known_generation: u64,
    /// Drill-down position below the home root, by sanitized names.
    pub home_cwd: Vec<String>,
    /// The home scanner is requested once the home view has been opened.
    pub home_active: bool,
    rescan_pending: bool,
}

impl App {
    pub fn new(device: Option<String>, settings: Settings, demo: bool) -> Self {
        Self::with_saved(device, settings.clone(), settings, demo)
    }

    /// `settings` may contain temporary overrides; `saved` is the file content.
    /// Saving writes `saved` plus the preferences the user changed at runtime.
    pub fn with_saved(
        device: Option<String>,
        settings: Settings,
        saved: Settings,
        demo: bool,
    ) -> Self {
        let home_active = settings.view == View::Home;
        Self {
            snapshot: Snapshot::default(),
            auto_device: device.clone(),
            device,
            history: VecDeque::new(),
            saved_settings: saved,
            touched: BTreeSet::new(),
            system_lang: Lang::En,
            settings,
            notice: None,
            notice_error: false,
            paused: false,
            filter: String::new(),
            searching: false,
            overlay: Overlay::None,
            table: TableState::default().with_selected(0),
            picker: 0,
            help_scroll: 0,
            visible_rows: 10,
            demo,
            setup_category: 0,
            setup_option: 0,
            setup_focus: false,
            editing: None,
            home_tree: None,
            known_generation: 0,
            home_cwd: Vec::new(),
            home_active,
            rescan_pending: false,
        }
    }

    pub fn history_seconds(&self) -> f64 {
        f64::from(self.settings.history_seconds)
    }

    pub fn lang(&self) -> Lang {
        Lang::resolve(self.settings.language, self.system_lang)
    }

    /// Saved settings plus only the preferences changed in Setup or by keys.
    /// CLI options, NO_COLOR and automatic fallbacks stay temporary.
    pub fn settings_to_save(&self) -> Settings {
        let mut settings = self.saved_settings.clone();
        for field in &self.touched {
            settings.copy_field(&self.settings, *field);
        }
        settings
    }

    pub fn settings_dirty(&self) -> bool {
        self.settings_to_save() != self.saved_settings
    }

    /// Record a successful save of `settings_to_save()`.
    pub fn settings_saved(&mut self, message: String) {
        self.saved_settings = self.settings_to_save();
        self.touched.clear();
        self.notice = Some(message);
        self.notice_error = false;
    }

    pub fn show_error(&mut self, message: String) {
        self.notice = Some(message);
        self.notice_error = true;
    }

    /// The next request for the collector; consumes a pending rescan.
    pub fn take_home_request(&mut self) -> HomeRequest {
        HomeRequest {
            root: self.settings.home_path.clone(),
            scan: self.home_active,
            interval_minutes: self.settings.rescan_minutes,
            rescan: std::mem::take(&mut self.rescan_pending),
            known_generation: self.known_generation,
        }
    }

    /// A new data source (helper fallback) numbers its scans from scratch.
    pub fn home_source_changed(&mut self) {
        self.known_generation = 0;
    }

    pub fn selected_device(&self) -> Option<&Device> {
        let name = self.device.as_deref()?;
        self.snapshot
            .devices
            .iter()
            .find(|device| device.name == name)
    }

    /// Rates of the selected device, or of all physical disks.
    pub fn selected_rates(&self) -> Option<DeviceRates> {
        match &self.device {
            Some(_) => self
                .selected_device()
                .and_then(|device| device.rates.clone()),
            None => diskstats::aggregate(&self.snapshot.devices),
        }
    }

    pub fn device_visible(&self, device: &Device) -> bool {
        self.settings.show_virtual
            || !device.kind.hidden_by_default()
            || self.device.as_deref() == Some(device.name.as_str())
    }

    pub fn visible_devices(&self) -> Vec<&Device> {
        self.snapshot
            .devices
            .iter()
            .filter(|device| self.device_visible(device))
            .collect()
    }

    pub fn update(&mut self, mut snapshot: Snapshot) {
        if self.paused {
            // An unaccepted tree is resent later: the known generation stays.
            return;
        }
        if let Some(tree) = snapshot.home_tree.take() {
            if self
                .home_tree
                .as_ref()
                .is_none_or(|old| old.root != tree.root)
            {
                self.home_cwd.clear();
            }
            self.known_generation = tree.generation;
            self.home_tree = Some(tree);
        }
        if let Some(status) = &snapshot.home
            && self
                .home_tree
                .as_ref()
                .is_some_and(|tree| tree.root != status.root)
        {
            // The root changed; the old tree no longer describes it.
            self.home_tree = None;
            self.home_cwd.clear();
        }
        let at = snapshot.elapsed;
        self.snapshot = snapshot;
        let rates = self.selected_rates();
        self.history.push_back(Sample {
            at,
            read: rates.as_ref().map(|rates| rates.read_rate),
            write: rates.as_ref().map(|rates| rates.write_rate),
        });
        while self
            .history
            .front()
            .is_some_and(|sample| at - sample.at > self.history_seconds())
        {
            self.history.pop_front();
        }
        // Also cap storage if the collector's clock does not advance.
        while self.history.len() > 6001 {
            self.history.pop_front();
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let count = self.row_count();
        self.table
            .select((count > 0).then(|| self.table.selected().unwrap_or(0).min(count - 1)));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        let before = self.settings.clone();
        let action = self.handle_key_inner(key);
        self.touched.extend(self.settings.differing(&before));
        action
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if self.editing.is_some() {
            return self.edit_key(key);
        }
        if key.code == KeyCode::F(12) {
            return Action::SaveSettings;
        }
        self.notice = None;
        if self.overlay == Overlay::Setup {
            return self.setup_key(key);
        }
        if key.code == KeyCode::F(10) {
            return Action::Quit;
        }
        if self.overlay != Overlay::None {
            return self.overlay_key(key);
        }
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.filter.clear();
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && self.filter.len() < 256 =>
                {
                    // Bound input even when pasted by a terminal.
                    self.filter.push(c);
                }
                _ => {}
            }
            self.table.select(Some(0));
            return Action::None;
        }
        let home = self.settings.view == View::Home;
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::F(1) | KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                self.help_scroll = 0;
            }
            KeyCode::F(2) => {
                self.overlay = Overlay::Setup;
                self.setup_focus = false;
            }
            KeyCode::F(3) | KeyCode::Char('/') => self.searching = true,
            KeyCode::F(4) | KeyCode::Char('v') => return self.set_view(self.settings.view.next(1)),
            KeyCode::Char('1') => return self.set_view(View::Processes),
            KeyCode::Char('2') => return self.set_view(View::Filesystems),
            KeyCode::Char('3') => return self.set_view(View::Home),
            KeyCode::F(5) | KeyCode::Char('d') => {
                self.overlay = Overlay::Devices;
                let visible = self.visible_devices();
                self.picker = visible
                    .iter()
                    .position(|device| Some(device.name.as_str()) == self.device.as_deref())
                    .map_or(0, |index| index + 1);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let mut names: Vec<Option<String>> = vec![None];
                names.extend(
                    self.visible_devices()
                        .into_iter()
                        .filter(|device| !device.partition)
                        .map(|device| Some(device.name.clone())),
                );
                let current = names
                    .iter()
                    .position(|name| *name == self.device)
                    .unwrap_or(0);
                let next = if key.code == KeyCode::BackTab {
                    (current + names.len() - 1) % names.len()
                } else {
                    (current + 1) % names.len()
                };
                self.set_device(names[next].clone());
                return Action::DeviceChanged;
            }
            KeyCode::F(6) => {
                self.overlay = Overlay::Sort;
                self.picker = SORTS
                    .iter()
                    .position(|sort| *sort == self.settings.sort)
                    .unwrap_or(0);
            }
            KeyCode::Char('s') => {
                self.settings.sort = self.settings.sort.next();
                self.table.select(Some(0));
            }
            KeyCode::F(9) | KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Esc if home && self.filter.is_empty() => self.home_up(),
            KeyCode::Esc => self.filter.clear(),
            KeyCode::Enter | KeyCode::Right if home => self.home_enter(),
            KeyCode::Backspace | KeyCode::Left if home => self.home_up(),
            KeyCode::Char('r') if home => {
                self.home_active = true;
                self.rescan_pending = true;
                return Action::HomeChanged;
            }
            KeyCode::Char('a') if home => {
                self.settings.apparent_size = !self.settings.apparent_size;
                self.table.select(Some(0));
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(self.visible_rows as isize),
            KeyCode::PageUp => self.move_selection(-(self.visible_rows as isize)),
            KeyCode::Home => self.table.select((self.row_count() > 0).then_some(0)),
            KeyCode::End => self.table.select(self.row_count().checked_sub(1)),
            _ => {}
        }
        Action::None
    }

    fn set_view(&mut self, view: View) -> Action {
        self.settings.view = view;
        self.filter.clear();
        self.table.select(Some(0));
        if view == View::Home && !self.home_active {
            self.home_active = true;
            return Action::HomeChanged;
        }
        Action::None
    }

    fn home_enter(&mut self) {
        let entries = self.home_entries();
        let Some(entry) = self.table.selected().and_then(|index| entries.get(index)) else {
            return;
        };
        if let HomeEntryKind::Directory(index) = entry.kind
            && self
                .home_tree
                .as_ref()
                .and_then(|tree| tree.node(index))
                .is_some_and(|node| {
                    !node.children.is_empty() || node.own_files.files > 0 || node.other.dirs > 0
                })
        {
            self.home_cwd.push(entry.name.clone());
            self.filter.clear();
            self.table.select(Some(0));
        }
    }

    fn home_up(&mut self) {
        if let Some(name) = self.home_cwd.pop() {
            self.filter.clear();
            let entries = self.home_entries();
            let index = entries
                .iter()
                .position(|entry| entry.name == name)
                .unwrap_or(0);
            self.table.select(Some(index));
        }
    }

    fn overlay_key(&mut self, key: KeyEvent) -> Action {
        let device_count = self.visible_devices().len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(1) | KeyCode::Char('?') => {
                self.overlay = Overlay::None
            }
            KeyCode::Up | KeyCode::Char('k') if self.overlay == Overlay::Help => {
                self.help_scroll = self.help_scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') if self.overlay == Overlay::Help => {
                self.help_scroll = self.help_scroll.saturating_add(1).min(80)
            }
            KeyCode::PageUp if self.overlay == Overlay::Help => {
                self.help_scroll = self.help_scroll.saturating_sub(8)
            }
            KeyCode::PageDown if self.overlay == Overlay::Help => {
                self.help_scroll = self.help_scroll.saturating_add(8).min(80)
            }
            KeyCode::Up | KeyCode::Char('k') if self.overlay == Overlay::Devices => {
                self.picker = self.picker.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') if self.overlay == Overlay::Devices => {
                self.picker = (self.picker + 1).min(device_count)
            }
            KeyCode::Home if self.overlay == Overlay::Devices => self.picker = 0,
            KeyCode::End if self.overlay == Overlay::Devices => self.picker = device_count,
            KeyCode::Enter if self.overlay == Overlay::Devices => {
                let device = self.picker.checked_sub(1).and_then(|index| {
                    self.visible_devices()
                        .get(index)
                        .map(|device| device.name.clone())
                });
                self.set_device(device);
                self.overlay = Overlay::None;
                return Action::DeviceChanged;
            }
            KeyCode::Up | KeyCode::Char('k') if self.overlay == Overlay::Sort => {
                self.picker = self.picker.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if self.overlay == Overlay::Sort => {
                self.picker = (self.picker + 1).min(SORTS.len() - 1);
            }
            KeyCode::Enter if self.overlay == Overlay::Sort => {
                self.settings.sort = SORTS[self.picker.min(SORTS.len() - 1)];
                self.table.select(Some(0));
                self.overlay = Overlay::None;
            }
            _ => {}
        }
        Action::None
    }

    fn set_device(&mut self, device: Option<String>) {
        // An explicit choice is saved even if it equals a temporary override.
        self.touched.insert(Field::Device);
        self.settings.device = Some(device.clone().unwrap_or_else(|| "all".into()));
        if self.device != device {
            self.device = device;
            self.history.clear();
            // Switching device explicitly resumes live collection.
            self.paused = false;
        }
    }

    fn setup_devices(&self) -> Vec<String> {
        self.visible_devices()
            .into_iter()
            .map(|device| device.name.clone())
            .collect()
    }

    fn setup_len(&self) -> usize {
        match self.setup_category {
            0 => 3,
            1 => self.setup_devices().len() + 3,
            2 => 5,
            3 => 3,
            _ => 4,
        }
    }

    fn setup_key(&mut self, key: KeyEvent) -> Action {
        self.setup_option = self.setup_option.min(self.setup_len().saturating_sub(1));
        match key.code {
            KeyCode::Esc | KeyCode::F(2) | KeyCode::F(10) | KeyCode::Char('q') => {
                self.overlay = Overlay::None;
            }
            KeyCode::Tab | KeyCode::BackTab => self.setup_focus = !self.setup_focus,
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') if !self.setup_focus => {
                self.setup_focus = true;
            }
            KeyCode::Left | KeyCode::Backspace if self.setup_focus => self.setup_focus = false,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.setup_focus {
                    self.setup_option = self.setup_option.saturating_sub(1);
                } else {
                    self.setup_category = self.setup_category.saturating_sub(1);
                    self.setup_option = 0;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.setup_focus {
                    self.setup_option = (self.setup_option + 1).min(self.setup_len() - 1);
                } else {
                    self.setup_category = (self.setup_category + 1).min(CATEGORY_COUNT - 1);
                    self.setup_option = 0;
                }
            }
            KeyCode::Home if self.setup_focus => self.setup_option = 0,
            KeyCode::End if self.setup_focus => self.setup_option = self.setup_len() - 1,
            KeyCode::PageDown if self.setup_focus => {
                self.setup_option = (self.setup_option + 8).min(self.setup_len() - 1)
            }
            KeyCode::PageUp if self.setup_focus => {
                self.setup_option = self.setup_option.saturating_sub(8)
            }
            KeyCode::Right
            | KeyCode::Enter
            | KeyCode::Char(' ')
            | KeyCode::Char('+')
            | KeyCode::Char('=')
            | KeyCode::Char('-')
                if self.setup_focus =>
            {
                let direction = if key.code == KeyCode::Char('-') {
                    -1
                } else {
                    1
                };
                return self.change_setup(direction);
            }
            _ => {}
        }
        Action::None
    }

    /// Editing the home path in Setup: Enter applies, Esc cancels.
    fn edit_key(&mut self, key: KeyEvent) -> Action {
        let Some(buffer) = self.editing.as_mut() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.editing = None;
                self.notice = None;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Enter => {
                let candidate = buffer.clone();
                match scanner::validate_root(&candidate) {
                    Ok(()) => {
                        self.editing = None;
                        self.notice = None;
                        if candidate != self.settings.home_path {
                            self.settings.home_path = candidate;
                            self.home_tree = None;
                            self.home_cwd.clear();
                            return Action::HomeChanged;
                        }
                    }
                    Err(error) => self.show_error(format!("{error:#}")),
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !c.is_control()
                    && buffer.len() + c.len_utf8() <= scanner::MAX_ROOT_BYTES =>
            {
                buffer.push(c);
            }
            _ => {}
        }
        Action::None
    }

    fn change_setup(&mut self, direction: i32) -> Action {
        match (self.setup_category, self.setup_option) {
            (0, 0) => self.settings.color = !self.settings.color,
            (0, 1) => {
                self.settings.interval_ms = (self.settings.interval_ms as i64
                    + i64::from(direction) * 100)
                    .clamp(100, 60_000) as u64
            }
            (0, _) => self.settings.language = self.settings.language.next(direction),
            (1, 0) => self.settings.show_virtual = !self.settings.show_virtual,
            (1, row) => {
                let device = match row {
                    1 => self.auto_device.clone(),
                    2 => None,
                    _ => self.setup_devices().get(row - 3).cloned(),
                };
                self.set_device(device);
                if row == 1 {
                    self.settings.device = None;
                }
                return Action::DeviceChanged;
            }
            (2, 0) => self.settings.show_graph = !self.settings.show_graph,
            (2, 1) => {
                self.settings.history_seconds = (i32::from(self.settings.history_seconds)
                    + direction * 10)
                    .clamp(10, 600) as u16
            }
            (2, 2) => self.settings.graph_style = self.settings.graph_style.next(direction),
            (2, 3) => self.settings.read_color = self.settings.read_color.next(direction),
            (2, _) => self.settings.write_color = self.settings.write_color.next(direction),
            (3, 0) => {
                let action = self.set_view(self.settings.view.next(direction));
                if action != Action::None {
                    return action;
                }
            }
            (3, 1) => {
                let current = SORTS
                    .iter()
                    .position(|sort| *sort == self.settings.sort)
                    .unwrap_or(0) as i32;
                self.settings.sort =
                    SORTS[(current + direction).rem_euclid(SORTS.len() as i32) as usize];
            }
            (3, _) => self.settings.show_idle = !self.settings.show_idle,
            (_, 0) => {
                self.editing = Some(self.settings.home_path.clone());
                return Action::None;
            }
            (_, 1) => {
                let current = RESCAN_STEPS
                    .iter()
                    .position(|step| *step >= self.settings.rescan_minutes)
                    .unwrap_or(RESCAN_STEPS.len() - 1) as i32;
                self.settings.rescan_minutes = RESCAN_STEPS
                    [(current + direction).clamp(0, RESCAN_STEPS.len() as i32 - 1) as usize];
                return Action::HomeChanged;
            }
            (_, 2) => self.settings.apparent_size = !self.settings.apparent_size,
            _ => {
                self.home_active = true;
                self.rescan_pending = true;
                return Action::HomeChanged;
            }
        }
        self.table.select(Some(0));
        Action::SettingsChanged
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.row_count();
        if count == 0 {
            self.table.select(None);
            return;
        }
        let current = self.table.selected().unwrap_or(0) as isize;
        self.table
            .select(Some((current + delta).clamp(0, count as isize - 1) as usize));
    }

    fn row_count(&self) -> usize {
        match self.settings.view {
            View::Processes => self.process_rows().len(),
            View::Filesystems => self.filesystem_rows().len(),
            View::Home => self.home_entries().len(),
        }
    }

    pub fn process_rows(&self) -> Vec<&ProcessRow> {
        let filter = self.filter.to_lowercase();
        let mut rows: Vec<_> = self
            .snapshot
            .processes
            .iter()
            .filter(|row| {
                self.settings.show_idle
                    // Unknown rates are not idle; they stay visible.
                    || row.read_rate.is_none_or(|rate| rate > 0.0)
                    || row.write_rate.is_none_or(|rate| rate > 0.0)
                    || row.cancelled_write_rate.is_some_and(|rate| rate > 0.0)
            })
            .filter(|row| {
                filter.is_empty()
                    || format!("{} {} {} {}", row.pid, row.user, row.name, row.command)
                        .to_lowercase()
                        .contains(&filter)
            })
            .collect();
        let rate = |value: Option<f64>| value.unwrap_or(-1.0);
        rows.sort_by(|a, b| {
            let order = match self.settings.sort {
                Sort::Io => (rate(b.read_rate) + rate(b.write_rate))
                    .total_cmp(&(rate(a.read_rate) + rate(a.write_rate))),
                Sort::Read => rate(b.read_rate).total_cmp(&rate(a.read_rate)),
                Sort::Write => rate(b.write_rate).total_cmp(&rate(a.write_rate)),
                Sort::Total => b
                    .read_bytes
                    .saturating_add(b.write_bytes)
                    .cmp(&a.read_bytes.saturating_add(a.write_bytes)),
                Sort::Pid => a.pid.cmp(&b.pid),
                Sort::Name => a.name.cmp(&b.name),
            };
            order.then_with(|| a.pid.cmp(&b.pid))
        });
        rows
    }

    pub fn filesystem_rows(&self) -> Vec<&Filesystem> {
        let filter = self.filter.to_lowercase();
        self.snapshot
            .filesystems
            .iter()
            .filter(|row| {
                self.settings.show_virtual
                    || row.block_device.as_ref().is_none_or(|name| {
                        self.snapshot
                            .devices
                            .iter()
                            .find(|device| &device.name == name)
                            .is_none_or(|device| !device.kind.hidden_by_default())
                    })
            })
            .filter(|row| {
                filter.is_empty()
                    || format!(
                        "{} {} {} {}",
                        row.mount_point,
                        row.source,
                        row.fs_type,
                        row.block_device.as_deref().unwrap_or("")
                    )
                    .to_lowercase()
                    .contains(&filter)
            })
            .collect()
    }

    /// The node shown in the home view and the displayed path below the root.
    pub fn home_node(&self) -> Option<(&DirNode, &[String])> {
        let tree = self.home_tree.as_ref()?;
        let (index, depth) = tree.resolve(&self.home_cwd);
        Some((tree.node(index)?, &self.home_cwd[..depth]))
    }

    pub fn home_entries(&self) -> Vec<HomeEntry> {
        let Some(tree) = &self.home_tree else {
            return Vec::new();
        };
        let Some((node, _)) = self.home_node() else {
            return Vec::new();
        };
        let apparent = self.settings.apparent_size;
        let size = |allocated: u64, apparent_size: u64| {
            if apparent { apparent_size } else { allocated }
        };
        let mut entries: Vec<HomeEntry> = node
            .children
            .iter()
            .filter_map(|index| tree.node(*index).map(|child| (*index, child)))
            .map(|(index, child)| HomeEntry {
                kind: HomeEntryKind::Directory(index),
                name: child.name.clone(),
                size: size(child.allocated, child.apparent),
                files: child.files,
                dirs: child.dirs,
                growth_previous: child.growth_previous,
                growth_start: child.growth_start,
                partial: child.partial(),
            })
            .collect();
        let summary = |kind, folded: &Folded| HomeEntry {
            kind,
            name: String::new(),
            size: size(folded.allocated, folded.apparent),
            files: folded.files,
            dirs: folded.dirs,
            growth_previous: None,
            growth_start: None,
            partial: false,
        };
        if node.own_files.files > 0 {
            entries.push(summary(HomeEntryKind::Files, &node.own_files));
        }
        if node.other.dirs > 0 {
            entries.push(summary(HomeEntryKind::Folded, &node.other));
        }
        let filter = self.filter.to_lowercase();
        entries.retain(|entry| filter.is_empty() || entry.name.to_lowercase().contains(&filter));
        entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        entries
    }
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width < 36 || area.height < 16 {
        frame.render_widget(
            Paragraph::new(app.lang().pick(
                "disktop\nTerminal too small.\nUse at least 36 x 16.\nq / F10 to quit.",
                "disktop\nTerminal zu klein.\nMindestens 36 x 16.\nq / F10 beendet.",
            ))
            .style(Style::default().fg(if app.settings.color {
                KEY
            } else {
                Color::Reset
            })),
            area,
        );
        return;
    }
    let graph_height = if app.settings.show_graph {
        ((area.height.saturating_sub(10)) / 2).clamp(5, 13)
    } else {
        0
    };
    let status = status_line(app);
    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Length(graph_height),
        Constraint::Length(u16::from(status.is_some())),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .split(area);
    draw_title(frame, app, layout[0]);
    draw_device(frame, app, layout[1]);
    draw_graph(frame, app, layout[2]);
    if let Some((text, color)) = status {
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(color)),
            layout[3],
        );
    }
    match app.settings.view {
        View::Processes => draw_processes(frame, app, layout[4]),
        View::Filesystems => draw_filesystems(frame, app, layout[4]),
        View::Home => draw_home(frame, app, layout[4]),
    }
    draw_footer(frame, app, layout[5]);
    match app.overlay {
        Overlay::Help => draw_help(frame, app, area),
        Overlay::Devices => draw_devices(frame, app, area),
        Overlay::Setup => draw_setup(frame, app, area),
        Overlay::Sort => draw_sort(frame, app, area),
        Overlay::None => {}
    }
    if !app.settings.color {
        for cell in &mut frame.buffer_mut().content {
            cell.set_fg(Color::Reset).set_bg(Color::Reset);
        }
    }
}

fn draw_title(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lang = app.lang();
    let visible = app.visible_devices();
    let index = visible
        .iter()
        .position(|device| Some(device.name.as_str()) == app.device.as_deref());
    let mut spans = vec![
        Span::styled(
            lang.pick("Device ", "Gerät "),
            Style::default().fg(Color::Green),
        ),
        Span::raw(index.map_or_else(
            || lang.pick("all", "alle").into(),
            |index| index.to_string(),
        )),
        Span::raw(" ["),
        Span::raw(truncate(
            app.device
                .as_deref()
                .unwrap_or(lang.pick("all disks", "alle Disks")),
            20,
        )),
        Span::raw("]"),
    ];
    if app.demo {
        spans.push(Span::styled("  DEMO", Style::default().fg(Color::Magenta)));
    }
    if app.paused {
        spans.push(Span::styled(
            lang.pick("  PAUSED", "  PAUSIERT"),
            Style::default().fg(Color::Yellow),
        ));
    }
    let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    // View tabs on the right; narrow terminals show only the current view.
    let views = [View::Processes, View::Filesystems, View::Home];
    let tabs_width: usize = views
        .iter()
        .map(|view| view_label(*view, lang).chars().count() + 3)
        .sum();
    let mut right = Vec::new();
    if usize::from(area.width) >= used + tabs_width + 2 {
        for (index, view) in views.iter().enumerate() {
            let label = format!("{}{}", index + 1, view_label(*view, lang));
            right.push(Span::raw(" "));
            if *view == app.settings.view {
                right.push(Span::styled(label, inverse(KEY)));
            } else {
                right.push(Span::raw(label));
            }
        }
    } else {
        let label = view_label(app.settings.view, lang);
        if usize::from(area.width) >= used + label.chars().count() + 2 {
            right.push(Span::styled(label.to_owned(), inverse(KEY)));
        }
    }
    let right_width: u16 = right
        .iter()
        .map(|span| span.content.chars().count() as u16)
        .sum();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    if right_width > 0 && right_width < area.width {
        frame.render_widget(
            Paragraph::new(Line::from(right)),
            Rect::new(area.right() - right_width, area.y, right_width, 1),
        );
    }
}

fn device_summary(app: &App, width: u16) -> String {
    let lang = app.lang();
    let Some(name) = &app.device else {
        let disks = app
            .snapshot
            .devices
            .iter()
            .filter(|device| !device.partition && !device.kind.is_virtual())
            .count();
        return match lang {
            Lang::En => format!("[ALL] {disks} disks; partitions, dm and md not added twice"),
            Lang::De => format!("[ALLE] {disks} Disks; Partitionen, dm und md nicht doppelt"),
        };
    };
    let Some(device) = app.selected_device() else {
        return format!(
            "{} {name}",
            lang.pick("[device unavailable]", "[Gerät nicht verfügbar]")
        );
    };
    let mut text = format!("[{}", device.kind.label());
    if device.partition {
        text.push_str(&match (lang, &device.parent) {
            (Lang::En, Some(parent)) => format!(" partition of {parent}"),
            (Lang::De, Some(parent)) => format!(" Partition von {parent}"),
            _ => String::new(),
        });
    }
    if device.removable {
        text.push_str(lang.pick(" removable", " wechselbar"));
    }
    text.push(']');
    let identity = [
        device.vendor.as_deref(),
        device.model.as_deref(),
        device.label.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    if !identity.is_empty() {
        text.push(' ');
        text.push_str(&identity);
    }
    if let Some(size) = device.size_bytes {
        text.push_str(&format!("  {}", format_bytes(size as f64)));
    }
    if width >= 100
        && let Some(sector) = device.hw_sector_size
    {
        text.push_str(&format!(
            "  {sector} B {}",
            lang.pick("sectors", "Sektoren")
        ));
    }
    if !device.mounts.is_empty() {
        text.push_str("  ");
        text.push_str(&device.mounts.join(" "));
    }
    truncate(&text, usize::from(width))
}

fn draw_device(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lang = app.lang();
    let rates = app.selected_rates();
    let peak = app
        .history
        .iter()
        .flat_map(|sample| [sample.read, sample.write])
        .flatten()
        .fold(GRAPH_FLOOR, f64::max);
    let device = app.selected_device();
    let (read_total, write_total) = match device {
        Some(device) => (Some(device.read_bytes), Some(device.write_bytes)),
        None if app.device.is_none() => {
            let disks = app
                .snapshot
                .devices
                .iter()
                .filter(|device| !device.partition && !device.kind.is_virtual());
            let (read, write) = disks.fold((0u64, 0u64), |(read, write), device| {
                (
                    read.saturating_add(device.read_bytes),
                    write.saturating_add(device.write_bytes),
                )
            });
            (Some(read), Some(write))
        }
        None => (None, None),
    };
    let meters = [
        rate_meter(
            lang.pick("RD", "LE"),
            rates
                .as_ref()
                .map(|rates| (rates.read_rate, rates.read_iops)),
            read_total,
            plot_color(app.settings.read_color),
            area.width,
            peak,
            lang,
        ),
        rate_meter(
            lang.pick("WR", "SC"),
            rates
                .as_ref()
                .map(|rates| (rates.write_rate, rates.write_iops)),
            write_total,
            plot_color(app.settings.write_color),
            area.width,
            peak,
            lang,
        ),
        utilization_meter(rates.as_ref(), area.width),
    ];
    // One bar width for all three meters keeps their scales comparable.
    let tail = meters
        .iter()
        .map(|meter| meter.suffix.chars().count() + meter.detail.chars().count())
        .max()
        .unwrap_or(0);
    let bar_width = usize::from(area.width)
        .saturating_sub(2 + 2 + 1 + 12 + tail)
        .clamp(4, 54);
    let mut lines = vec![
        // Default foreground keeps the summary readable on light and dark themes.
        Line::from(Span::raw(device_summary(app, area.width))),
    ];
    lines.extend(meters.into_iter().map(|meter| meter.line(bar_width)));
    frame.render_widget(Paragraph::new(lines), area);
}

fn bar(fill_ratio: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let fill = (fill_ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    vec![
        Span::raw("["),
        Span::styled("|".repeat(fill), Style::default().fg(color)),
        Span::styled(
            " ".repeat(width.saturating_sub(fill)),
            Style::default().fg(DIM),
        ),
        Span::raw("]"),
    ]
}

/// One labeled bar of the device header: "RD[|||   ]  12.3 MiB/s  45 IOPS".
struct Meter {
    label: &'static str,
    color: Color,
    ratio: f64,
    value: String,
    /// Default foreground.
    suffix: String,
    /// Dimmed explanation.
    detail: String,
}

impl Meter {
    fn line(self, bar_width: usize) -> Line<'static> {
        let mut spans = vec![Span::styled(self.label, Style::default().fg(self.color))];
        spans.extend(bar(self.ratio, bar_width, self.color));
        spans.push(Span::raw(format!(" {:>12}{}", self.value, self.suffix)));
        spans.push(Span::styled(self.detail, Style::default().fg(DIM)));
        Line::from(spans)
    }
}

#[allow(clippy::too_many_arguments)]
fn rate_meter(
    label: &'static str,
    value: Option<(f64, f64)>,
    total: Option<u64>,
    color: Color,
    width: u16,
    scale: f64,
    lang: Lang,
) -> Meter {
    Meter {
        label,
        color,
        ratio: value.map_or(0.0, |(rate, _)| rate / scale),
        value: value.map_or_else(|| "-".to_owned(), |(rate, _)| format_rate(rate)),
        suffix: match value {
            Some((_, iops)) if width >= 44 => format!("  {} IOPS", format_short_count(iops)),
            _ => String::new(),
        },
        detail: match total {
            Some(total) if width >= 78 => format!(
                "  {} {}",
                lang.pick("total", "gesamt"),
                format_bytes(total as f64)
            ),
            _ => String::new(),
        },
    }
}

fn utilization_meter(rates: Option<&DeviceRates>, width: u16) -> Meter {
    let ms =
        |value: Option<f64>| value.map_or_else(|| "-".to_owned(), |value| format!("{value:.2} ms"));
    Meter {
        label: "IO",
        color: UTIL,
        ratio: rates.map_or(0.0, |rates| rates.utilization_percent / 100.0),
        value: rates.map_or_else(
            || "-".to_owned(),
            |rates| format!("{:.1}%", rates.utilization_percent),
        ),
        suffix: String::new(),
        detail: match rates {
            Some(rates) if width >= 110 => format!(
                "  await {}  r {}  w {}  qd {:.2}",
                ms(rates.await_ms),
                ms(rates.read_await_ms),
                ms(rates.write_await_ms),
                rates.queue_depth
            ),
            Some(rates) if width >= 52 => format!(
                "  await {}  qd {:.1}",
                ms(rates.await_ms),
                rates.queue_depth
            ),
            _ => String::new(),
        },
    }
}

fn draw_graph(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    if app.settings.graph_style == GraphStyle::Steps {
        draw_step_graph(frame, app, area);
        return;
    }
    let now = app.history.back().map_or(0.0, |sample| sample.at);
    let series = |read: bool| -> Vec<(f64, f64)> {
        app.history
            .iter()
            .filter_map(|sample| {
                let value = if read { sample.read } else { sample.write };
                value.map(|value| (sample.at - now, value))
            })
            .collect()
    };
    let (read, write) = (series(true), series(false));
    let peak = graph_peak(app);
    let labels = [0.0, peak / 2.0, peak]
        .map(|value| Line::from(format_rate(value).trim_end_matches("/s").to_string()));
    let lang = app.lang();
    let datasets = vec![
        Dataset::default()
            .name(lang.pick("RD", "LE"))
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(plot_color(app.settings.read_color)))
            .data(&read),
        Dataset::default()
            .name(lang.pick("WR", "SC"))
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(plot_color(app.settings.write_color)))
            .data(&write),
    ];
    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(DIM))
                .bounds([-app.history_seconds(), 0.0])
                .labels([
                    Line::from(format!("-{:.0}s", app.history_seconds())),
                    Line::from(format!("-{:.0}s", app.history_seconds() / 2.0)),
                    Line::from("0s"),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(DIM))
                .bounds([0.0, peak])
                .labels(labels),
        );
    frame.render_widget(chart, area);
}

fn graph_peak(app: &App) -> f64 {
    app.history
        .iter()
        .flat_map(|sample| [sample.read, sample.write])
        .flatten()
        .fold(GRAPH_FLOOR, f64::max)
        * 1.1
}

fn draw_step_graph(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 14 || inner.height < 3 {
        return;
    }
    let peak = graph_peak(app);
    let labels =
        [peak, peak / 2.0, 0.0].map(|value| format_rate(value).trim_end_matches("/s").to_owned());
    let label_width = labels.iter().map(String::len).max().unwrap_or(6).min(11) as u16;
    let plot = Rect {
        x: inner.x + label_width + 1,
        y: inner.y,
        width: inner.width.saturating_sub(label_width + 1),
        height: inner.height - 1,
    };
    for (index, label) in labels.iter().enumerate() {
        let y = plot.y + (plot.height - 1) * index as u16 / 2;
        frame.render_widget(
            Paragraph::new(label.as_str()).alignment(Alignment::Right),
            Rect::new(inner.x, y, label_width, 1),
        );
    }
    let now = app.history.back().map_or(0.0, |sample| sample.at);
    // Draw writes first, so reads stay visible at intersections. Each series
    // uses terminal line characters, as in nvtop, rather than colored dots.
    for (read, color) in [
        (false, app.settings.write_color),
        (true, app.settings.read_color),
    ] {
        let mut columns = vec![None; usize::from(plot.width)];
        for sample in &app.history {
            let progress = (sample.at - now + app.history_seconds()) / app.history_seconds();
            if !(0.0..=1.0).contains(&progress) {
                continue;
            }
            let Some(value) = (if read { sample.read } else { sample.write }) else {
                continue;
            };
            let x = (progress * f64::from(plot.width - 1)).round() as usize;
            let y = ((1.0 - (value / peak).clamp(0.0, 1.0)) * f64::from(plot.height - 1)).round()
                as u16;
            columns[x] = Some(y);
        }
        let mut paths = vec![0u8; usize::from(plot.width) * usize::from(plot.height)];
        let index = |x: u16, y: u16| usize::from(y) * usize::from(plot.width) + usize::from(x);
        let mut previous = None;
        for (x, y) in columns
            .into_iter()
            .enumerate()
            .filter_map(|(x, y)| y.map(|y| (x as u16, y)))
        {
            if let Some((px, py)) = previous {
                for column in px..x {
                    paths[index(column, py)] |= 2;
                    paths[index(column + 1, py)] |= 8;
                }
                for row in py.min(y)..py.max(y) {
                    paths[index(x, row)] |= 4;
                    paths[index(x, row + 1)] |= 1;
                }
            } else {
                paths[index(x, y)] = 10;
            }
            previous = Some((x, y));
        }
        for y in 0..plot.height {
            for x in 0..plot.width {
                let symbol = match paths[index(x, y)] {
                    0 => continue,
                    1 | 4 | 5 => "│",
                    2 | 8 | 10 => "─",
                    3 => "└",
                    6 => "┌",
                    9 => "┘",
                    12 => "┐",
                    7 => "├",
                    11 => "┴",
                    13 => "┤",
                    14 => "┬",
                    _ => "┼",
                };
                frame.buffer_mut()[(plot.x + x, plot.y + y)]
                    .set_symbol(symbol)
                    .set_style(Style::default().fg(plot_color(color)));
            }
        }
    }
    let lang = app.lang();
    for (row, label, color) in [
        (0, lang.pick("RD", "LE"), app.settings.read_color),
        (1, lang.pick("WR", "SC"), app.settings.write_color),
    ] {
        frame.render_widget(
            Paragraph::new(label).style(Style::default().fg(plot_color(color))),
            Rect::new(plot.x + 1, plot.y + row, 2, 1),
        );
    }
    let label_y = inner.bottom() - 1;
    frame.buffer_mut().set_stringn(
        plot.x,
        label_y,
        format!("-{:.0}s", app.history_seconds()),
        usize::from(plot.width),
        Style::default(),
    );
    if plot.width >= 28 {
        let label = format!("-{:.0}s", app.history_seconds() / 2.0);
        frame.buffer_mut().set_stringn(
            plot.x + (plot.width - label.len() as u16) / 2,
            label_y,
            &label,
            label.len(),
            Style::default(),
        );
    }
    frame
        .buffer_mut()
        .set_stringn(plot.right() - 2, label_y, "0s", 2, Style::default());
}

fn status_line(app: &App) -> Option<(String, Color)> {
    let lang = app.lang();
    if let Some(notice) = &app.notice {
        return Some((
            notice.clone(),
            if app.notice_error { Color::Red } else { KEY },
        ));
    }
    if app.searching {
        return Some((
            match lang {
                Lang::En => format!("Search: {}_  Enter apply / Esc clear", app.filter),
                Lang::De => format!("Suche: {}_  Enter übernimmt / Esc leert", app.filter),
            },
            KEY,
        ));
    }
    if !app.filter.is_empty() {
        return Some((
            format!(
                "Filter: {}  {}",
                app.filter,
                lang.pick("Esc clears", "Esc leert")
            ),
            KEY,
        ));
    }
    if app.demo || app.settings.view != View::Processes {
        return None; // DEMO is labeled in the title; other views show their own state.
    }
    let access = &app.snapshot.process_access;
    if !access.privileged && access.unreadable > 0 {
        return Some((
            match lang {
                Lang::En => format!(
                    "{} processes of other users hidden | all: scripts/setup-helper.sh",
                    access.unreadable
                ),
                Lang::De => format!(
                    "{} Prozesse anderer Benutzer verborgen | alle: scripts/setup-helper.sh",
                    access.unreadable
                ),
            },
            Color::Yellow,
        ));
    }
    None
}

fn number_cell(text: String) -> Cell<'static> {
    Cell::from(Line::from(text).alignment(Alignment::Right))
}

fn header_row(headers: Vec<(&str, bool)>) -> Row<'_> {
    Row::new(headers.into_iter().map(|(header, numeric)| {
        if numeric {
            number_cell(header.into())
        } else {
            Cell::from(header)
        }
    }))
    .style(inverse(Color::Green))
}

fn empty_message(frame: &mut Frame<'_>, area: Rect, offset: u16, message: &str, color: Color) {
    if area.height > offset {
        frame.render_widget(
            Paragraph::new(message.to_owned()).style(Style::default().fg(color)),
            Rect::new(area.x, area.y + offset, area.width, 1),
        );
    }
}

fn draw_processes(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
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

fn use_color(percent: f64) -> Color {
    if percent >= 90.0 {
        Color::Red
    } else if percent >= 75.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn optional_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format_bytes(value as f64))
}

fn draw_filesystems(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.visible_rows = usize::from(area.height.saturating_sub(1));
    let wide = area.width >= 94;
    let medium = area.width >= 52;
    let lang = app.lang();
    let records = app.filesystem_rows();
    let count = records.len();
    let read_color = Style::default().fg(plot_color(app.settings.read_color));
    let write_color = Style::default().fg(plot_color(app.settings.write_color));
    let mount_header = lang.pick("MOUNT", "EINHÄNGEPUNKT");
    let free_header = lang.pick("FREE", "FREI");
    let size_header = lang.pick("SIZE", "GRÖSSE");
    let percent =
        |value: Option<f64>| value.map_or_else(|| "-".to_owned(), |value| format!("{value:.0}%"));
    let rate = |value: Option<f64>| {
        value.map_or_else(
            || "-".to_owned(),
            |value| format_rate(value).trim_end_matches("/s").to_owned(),
        )
    };
    let mount = |row: &Filesystem| {
        format!(
            "{}{}",
            if row.contains_home { "*" } else { " " },
            row.mount_point
        )
    };
    let style = |row: &Filesystem| {
        if row.contains_home {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    };
    let (headers, widths, rows): (Vec<(&str, bool)>, Vec<Constraint>, Vec<Row<'_>>) = if wide {
        (
            vec![
                (mount_header, false),
                (lang.pick("DEVICE", "GERÄT"), false),
                (lang.pick("TYPE", "TYP"), false),
                (lang.pick("USE", "BELEGT"), false),
                (size_header, true),
                (free_header, true),
                ("INODES", true),
                (lang.pick("READ/s", "LESEN/s"), true),
                (lang.pick("WRITE/s", "SCHREIB/s"), true),
            ],
            vec![
                Constraint::Min(12),
                Constraint::Length(14),
                Constraint::Length(6),
                Constraint::Length(17),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(6),
                Constraint::Length(10),
                Constraint::Length(10),
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
                        number_cell(optional_bytes(row.total_bytes)),
                        number_cell(optional_bytes(row.available_bytes)),
                        number_cell(percent(row.inode_percent())),
                        number_cell(rate(row.read_rate)).style(read_color),
                        number_cell(rate(row.write_rate)).style(write_color),
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
                        number_cell(optional_bytes(row.total_bytes)),
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
                (free_header, true),
            ],
            vec![
                Constraint::Min(10),
                Constraint::Length(5),
                Constraint::Length(10),
            ],
            records
                .into_iter()
                .map(|row| {
                    let used = row.use_percent();
                    Row::new(vec![
                        Cell::from(mount(row)),
                        number_cell(percent(used))
                            .style(Style::default().fg(use_color(used.unwrap_or(0.0)))),
                        number_cell(optional_bytes(row.available_bytes)),
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
fn home_header(app: &App, width: u16) -> (String, Color) {
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
            text.push_str(
                status
                    .error
                    .as_deref()
                    .unwrap_or(lang.pick("scan failed", "Scan fehlgeschlagen")),
            );
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
    if width >= 100 {
        text.push_str(lang.pick("  r rescan", "  r neu scannen"));
    }
    let path_width = usize::from(width)
        .saturating_sub(text.chars().count())
        .max(8);
    (format!("{}{text}", truncate_left(&path, path_width)), color)
}

fn draw_home(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
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
    let (headers, widths, rows): (Vec<(&str, bool)>, Vec<Constraint>, Vec<Row<'_>>) = if wide {
        (
            vec![
                (name_header, false),
                (size_header, true),
                (lang.pick("SHARE", "ANTEIL"), false),
                (files_header, true),
                (lang.pick("+PREV", "+LETZTER"), true),
                (lang.pick("+START", "+START"), true),
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
                (lang.pick("+PREV", "+LETZTER"), true),
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

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lang = app.lang();
    // German labels are abbreviated like Midnight Commander's on narrow
    // terminals, so every required key fits at 36 columns.
    let wide = area.width >= 60;
    let mut keys = vec![
        ("F1", lang.pick("Help", "Hilfe")),
        ("F2", lang.pick("Setup", "Setup")),
    ];
    if area.width >= 44 {
        keys.push(("F3", lang.pick("Search", "Suche")));
    }
    if area.width >= 80 {
        keys.extend([
            ("F4", lang.pick("View", "Ansicht")),
            ("F5", lang.pick("Device", "Gerät")),
        ]);
    }
    keys.push((
        "F6",
        if wide {
            lang.pick("Sort", "Sortieren")
        } else {
            lang.pick("Sort", "Sort")
        },
    ));
    if area.width >= 94 {
        keys.push((
            "F9",
            if app.paused {
                lang.pick("Resume", "Weiter")
            } else {
                lang.pick("Pause", "Pause")
            },
        ));
    }
    keys.push(("F10", lang.pick("Quit", "Ende")));
    keys.push((
        "F12",
        if wide {
            lang.pick("SaveConfig", "Speichern")
        } else {
            lang.pick("Save", "Speich")
        },
    ));
    draw_key_bar(frame, area, &keys);
}

fn draw_key_bar(frame: &mut Frame<'_>, area: Rect, keys: &[(&str, &str)]) {
    let used: usize = keys
        .iter()
        .map(|(key, label)| key.chars().count() + label.chars().count())
        .sum();
    let padding = usize::from(area.width).saturating_sub(used);
    let mut spans = Vec::new();
    for (index, (key, label)) in keys.iter().enumerate() {
        let spaces = padding / keys.len() + usize::from(index < padding % keys.len());
        spans.push(Span::raw((*key).to_owned()));
        spans.push(Span::styled(
            format!("{label}{}", " ".repeat(spaces)),
            inverse(KEY),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn plot_color_label(color: PlotColor, lang: Lang) -> &'static str {
    match (color, lang) {
        (_, Lang::En) | (PlotColor::Cyan | PlotColor::Magenta, _) => color.label(),
        (PlotColor::Green, Lang::De) => "Grün",
        (PlotColor::Yellow, Lang::De) => "Gelb",
        (PlotColor::Red, Lang::De) => "Rot",
        (PlotColor::Blue, Lang::De) => "Blau",
        (PlotColor::White, Lang::De) => "Weiß",
    }
}

fn setup_options(app: &App) -> Vec<String> {
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
                    "{} Auto ({})",
                    check(app.settings.device.is_none()),
                    app.auto_device
                        .as_deref()
                        .unwrap_or(lang.pick("all", "alle"))
                ),
                format!(
                    "{} {}",
                    check(app.settings.device.as_deref() == Some("all")),
                    lang.pick("All disks", "Alle Disks")
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

fn setup_description(app: &App) -> &'static str {
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
        (1, 0) => lang.pick(
            "Loop, ram and zram devices (snaps, swap) are hidden unless enabled here.",
            "Loop-, RAM- und zram-Geräte (Snaps, Swap) bleiben ohne diese Option verborgen.",
        ),
        (1, _) => lang.pick(
            "Enter selects a device. Auto follows the disk of the home path. All adds physical disks only.",
            "Enter wählt ein Gerät. Auto folgt der Disk des Home-Pfads. Alle summiert nur physische Disks.",
        ),
        (2, 0) => lang.pick(
            "Hide the graph to give the table more space.",
            "Blendet das Diagramm aus, damit die Tabelle mehr Platz hat.",
        ),
        (2, 1) => lang.pick(
            "Show 10 to 600 seconds of history. +/- changes by 10 seconds.",
            "Zeigt 10 bis 600 Sekunden Verlauf. +/- ändert um 10 Sekunden.",
        ),
        (2, 2) => lang.pick(
            "Steps uses nvtop-style terminal lines. Braille draws a finer curve.",
            "Stufen nutzt Terminal-Linien wie nvtop. Braille zeichnet eine feinere Kurve.",
        ),
        (2, _) => lang.pick(
            "Cycle through the terminal's standard colors with Enter or +/-.",
            "Wechselt mit Enter oder +/- durch die Standardfarben des Terminals.",
        ),
        (3, 0) => lang.pick(
            "Processes, filesystems or the home size view. 1, 2, 3 or F4 switch directly.",
            "Prozesse, Dateisysteme oder Home-Größen. 1, 2, 3 oder F4 wechseln direkt.",
        ),
        (3, 1) => lang.pick(
            "Choose the process sort order. F6 opens the same choices in the monitor.",
            "Wählt die Sortierung der Prozesse. F6 bietet dieselbe Auswahl im Monitor.",
        ),
        (3, _) => lang.pick(
            "Include processes without storage I/O in the current interval.",
            "Zeigt auch Prozesse ohne Speicher-I/O im aktuellen Intervall.",
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
            "Allocated counts disk blocks (du); apparent counts file lengths (du -b).",
            "Belegt zählt Plattenblöcke (du); scheinbar zählt Dateilängen (du -b).",
        ),
        _ => lang.pick(
            "Start a new scan now. The previous result stays visible meanwhile.",
            "Startet sofort einen neuen Scan. Das letzte Ergebnis bleibt sichtbar.",
        ),
    }
}

fn draw_setup(frame: &mut Frame<'_>, app: &App, area: Rect) {
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
        .map(|name| name.chars().count() as u16 + 2)
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
    let header = if header.chars().count() > usize::from(panes[2].width) {
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

fn draw_sort(frame: &mut Frame<'_>, app: &App, area: Rect) {
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

fn popup(area: Rect, width: u16, height: u16) -> Rect {
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
    "F4 / v, 1 2 3  Processes / Filesystems / Home view",
    "F5 / d         Choose device; Tab cycles disks",
    "F6             Choose process sorting; s cycles it",
    "F9 / Space     Pause / resume display",
    "F12            Save current settings for next start",
    "Up / Down      Select; PgUp / PgDn scroll",
    "Home view      Enter/Right open, Backspace/Left up,",
    "               r rescan, a allocated/apparent size",
    "q / F10 / Ctrl-C Quit",
    "",
    "RD: read, WR: write (green / yellow), IO: utilization.",
    "Device rates come from /proc/diskstats (512-byte sectors).",
    "Utilization is time with I/O in flight; await is average",
    "request time; qd is the average queue depth.",
    "All adds physical whole disks; partitions, dm and md",
    "would count the same I/O twice.",
    "Process READ/WRITE are bytes a process caused to be read",
    "from or dirtied for storage, across all devices. They",
    "cannot be split per device and exclude cache hits.",
    "Writes count when data is dirtied, not at writeback.",
    "CANCEL: dirty data truncated before writeback; charged",
    "to the truncating process, so it is not subtracted.",
    "Processes that exit between refreshes are not listed;",
    "their I/O remains in the device rates.",
    "Without the helper only your own processes are shown.",
    "Enable all once: ./scripts/setup-helper.sh",
    "* marks the filesystem that contains the home path.",
    "Home: one filesystem, no symlinks, hard links once.",
    "Sizes are allocated blocks (du) unless apparent is set.",
    "! marks directories that were partly unreadable.",
    "Command-line options and NO_COLOR apply only to this run.",
    "",
];

const HELP_DE: &[&str] = &[
    "F1 / ?         Hilfe    Esc schließt",
    "F2             Setup: Allgemein, Gerät, Diagramm, Tabelle, Home",
    "F3 / /         Aktuelle Tabelle durchsuchen",
    "F4 / v, 1 2 3  Prozesse / Dateisysteme / Home",
    "F5 / d         Gerät wählen; Tab wechselt die Disk",
    "F6             Prozess-Sortierung wählen; s wechselt",
    "F9 / Leertaste Anzeige anhalten / fortsetzen",
    "F12            Aktuelle Einstellungen für den nächsten Start speichern",
    "Hoch / Runter  Auswählen; Bild hoch / runter blättert",
    "Home-Ansicht   Enter/Rechts öffnet, Rücktaste/Links hoch,",
    "               r neu scannen, a belegt/scheinbar",
    "q / F10 / Strg-C Beenden",
    "",
    "LE: Lesen, SC: Schreiben (grün / gelb), IO: Auslastung.",
    "Geräteraten stammen aus /proc/diskstats (512-Byte-Sektoren).",
    "Auslastung ist die Zeit mit laufender I/O; await ist die",
    "mittlere Anfragezeit; qd die mittlere Warteschlangentiefe.",
    "Alle summiert physische Disks; Partitionen, dm und md",
    "würden dieselbe I/O doppelt zählen.",
    "Prozess-LESEN/SCHREIB sind Bytes, die ein Prozess vom",
    "Speicher lesen ließ oder dafür schrieb, über alle Geräte.",
    "Sie lassen sich nicht pro Gerät aufteilen; Cache-Treffer",
    "zählen nicht. Schreiben zählt beim Ändern im Cache.",
    "VERWORF: vor dem Rückschreiben abgeschnittene Daten; sie",
    "zählen beim abschneidenden Prozess und werden nicht",
    "abgezogen. Zwischen zwei Messungen beendete Prozesse",
    "fehlen; ihre I/O bleibt in den Geräteraten.",
    "Ohne Helper erscheinen nur eigene Prozesse.",
    "Alle einmalig aktivieren: ./scripts/setup-helper.sh",
    "* markiert das Dateisystem mit dem Home-Pfad.",
    "Home: ein Dateisystem, keine Symlinks, Hardlinks einmal.",
    "Größen sind belegte Blöcke (du), außer scheinbar ist aktiv.",
    "! markiert teilweise unlesbare Verzeichnisse.",
    "Kommandozeilenoptionen und NO_COLOR gelten nur für diesen Lauf.",
    "",
];

fn draw_help(frame: &mut Frame<'_>, app: &App, area: Rect) {
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

fn draw_devices(frame: &mut Frame<'_>, app: &App, area: Rect) {
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
    let total = diskstats::aggregate(&app.snapshot.devices);
    let mut rows = vec![Row::new(vec![
        lang.pick("all", "alle").to_string(),
        lang.pick("disks", "Disks").to_string(),
        rate(total.as_ref()),
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
            rate(device.rates.as_ref()),
        ])
    }));
    let mut state = TableState::default().with_selected(app.picker);
    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new([
            lang.pick("DEVICE", "GERÄT"),
            lang.pick("TYPE / SIZE", "TYP / GRÖSSE"),
            lang.pick("READ WRITE /s", "LESEN SCHREIB /s"),
        ])
        .style(inverse(Color::Green)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(lang.pick(
                " Device  Enter select / Esc close ",
                " Gerät  Enter wählt / Esc schließt ",
            ))
            .border_style(Style::default().fg(KEY)),
    )
    .row_highlight_style(inverse(KEY));
    frame.render_stateful_widget(table, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;
    use ratatui::{Terminal, backend::TestBackend};

    fn app() -> App {
        let mut app = App::new(Some("nvme0n1".into()), Settings::default(), true);
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

    const SIZES: [(u16, u16); 5] = [(36, 16), (40, 24), (52, 18), (80, 24), (120, 36)];

    #[test]
    fn layouts_fit_narrow_and_wide_terminals_in_both_languages_and_views() {
        for (language, device, quit, done, save, read) in [
            (Language::En, "Device ", "F10Quit", "Done", "Save", "READ/s"),
            (
                Language::De,
                "Gerät ",
                "F10Ende",
                "Fertig",
                "Speich",
                "LESEN/s",
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
                    assert!(text.contains(device), "title missing {at} {view:?}\n{text}");
                    assert!(
                        text.contains(marker),
                        "{marker} missing {at} {view:?}\n{text}"
                    );
                    let footer = text.lines().last().unwrap();
                    // Every required function key must be complete on the bottom row.
                    for key in ["F1", "F2", "F6", quit, "F12"] {
                        assert!(footer.contains(key), "{key} missing {at}: {footer:?}");
                    }
                    assert!(footer.contains(save), "save label cut {at}: {footer:?}");
                    // Device header rows: summary, read, write, utilization.
                    assert!(
                        text.lines().nth(4).unwrap().starts_with("IO["),
                        "{at}\n{text}"
                    );
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
            }
        }
    }

    #[test]
    fn german_text_covers_monitor_overlays_and_language_setup_row() {
        let mut app = app();
        app.system_lang = Lang::De;
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        for expected in [
            "Gerät ",
            "BENUTZER",
            "BEFEHL",
            "Hilfe",
            "Speichern",
            "gesamt",
            "Prozesse",
        ] {
            assert!(text.contains(expected), "{expected} missing: {text}");
        }
        for (overlay, expected) in [
            (
                Overlay::Help,
                "Aktuelle Einstellungen für den nächsten Start",
            ),
            (Overlay::Sort, "Prozesse sortieren"),
            (Overlay::Devices, "GERÄT"),
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
        assert!(app.history.iter().all(|sample| sample.at >= 41.0));
    }

    #[test]
    fn device_switch_clears_history_and_tab_cycles_whole_disks() {
        let mut app = app();
        assert_eq!(app.handle_key(KeyCode::Tab.into()), Action::DeviceChanged);
        assert_eq!(app.device.as_deref(), Some("sda"), "partitions are skipped");
        assert!(app.history.is_empty());
        app.handle_key(KeyCode::Tab.into());
        assert_eq!(
            app.device, None,
            "loop0 is hidden, so the cycle returns to all"
        );
        app.handle_key(KeyCode::BackTab.into());
        assert_eq!(app.device.as_deref(), Some("sda"));
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
            .filter(|device| matches!(device.name.as_str(), "nvme0n1" | "sda"))
            .map(|device| device.rates.as_ref().unwrap().read_rate)
            .sum();
        assert!((all.read_rate - disks).abs() < 1e-6);
    }

    #[test]
    fn home_view_drills_down_and_back_and_shows_summaries() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyCode::Char('3').into()),
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
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("42 processes of other users hidden"),
            "{text}"
        );
        let read_line = text.lines().nth(2).unwrap();
        assert!(read_line.contains(&format!(" {:>12}", "-")), "{read_line}");
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
        assert_eq!(summary[0].symbol(), "[");
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
        app.handle_key(KeyCode::Char('2').into());
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
    fn graph_time_labels_stay_visible_and_hiding_graph_frees_rows() {
        let mut app = app();
        let mut terminal = Terminal::new(TestBackend::new(52, 18)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("-60s"));
        assert!(text.contains("-30s"));
        let with_graph = app.visible_rows;
        app.settings.show_graph = false;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.visible_rows > with_graph);
    }
}
