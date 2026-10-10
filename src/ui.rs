//! A compact nvtop-style terminal interface. No data is invented by the renderer.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    buffer::Buffer,
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
    config::{Field, GraphStyle, Language, MAX_PROCESS_ROWS, PlotColor, Settings, View},
    diskstats,
    i18n::Lang,
    model::{
        Device, DeviceRates, DirNode, Filesystem, Folded, HomeReport, HomeRequest, ProcessRow,
        ScanState, Snapshot, StopReason,
    },
    scanner,
    text::{
        Columns, format_bytes, format_bytes_short, format_bytes_tiny, format_count, format_delta,
        format_duration, format_rate, format_short_count, pad, truncate, truncate_left,
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
const VIEWS: [View; 3] = [View::Filesystems, View::Home, View::Processes];
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
        View::Processes => "I/O",
        View::Filesystems => lang.pick("Space", "Speicher"),
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
    Views,
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

/// One refresh of one device, kept per device for its history graph.
#[derive(Clone, Debug)]
pub struct Sample {
    pub at: f64,
    pub read: Option<f64>,
    pub write: Option<f64>,
    /// Utilization in percent.
    pub util: Option<f64>,
    /// Fill level of the mounted filesystems in percent.
    pub used: Option<f64>,
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
    /// Focused device; `None` focuses no disk and summarizes all of them.
    pub device: Option<String>,
    /// History per device name, for the overview graphs and meter scales.
    pub history: BTreeMap<String, VecDeque<Sample>>,
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
    /// First grid row of the disk overview when not all disks fit.
    disk_scroll: usize,
    /// Layout of the last drawn overview, for tests and scrolling.
    pub disk_layout: Option<DiskLayout>,
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
            history: BTreeMap::new(),
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
            disk_scroll: 0,
            disk_layout: None,
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
        if self.device.as_deref() == Some(device.name.as_str()) {
            return true;
        }
        (self.settings.show_virtual || !device.kind.hidden_by_default())
            && (self.settings.show_stacked || !self.stacked_on_shown_disk(device))
    }

    /// A dm or md device on exactly one disk that is itself shown, such as LUKS
    /// or an LVM volume on one disk. Its I/O, mounts and fill level already
    /// appear on that disk; devices spanning several disks stay visible.
    pub fn stacked_on_shown_disk(&self, device: &Device) -> bool {
        let [disk] = device.backing_disks.as_slice() else {
            return false;
        };
        device.kind.is_stacked()
            && disk != &device.name
            && self.snapshot.devices.iter().any(|candidate| {
                &candidate.name == disk
                    && !candidate.partition
                    && !candidate.kind.is_stacked()
                    && (self.settings.show_virtual || !candidate.kind.hidden_by_default())
            })
    }

    pub fn visible_devices(&self) -> Vec<&Device> {
        self.snapshot
            .devices
            .iter()
            .filter(|device| self.device_visible(device))
            .collect()
    }

    /// Devices of the disk overview: every visible whole device (disks, and
    /// dm/md as stacked devices), plus a focused partition after its disk.
    pub fn overview_devices(&self) -> Vec<&Device> {
        let mut devices: Vec<&Device> = self
            .visible_devices()
            .into_iter()
            .filter(|device| !device.partition)
            .collect();
        if let Some(focused) = self.selected_device().filter(|device| device.partition) {
            let position = devices
                .iter()
                .position(|device| focused.parent.as_deref() == Some(device.name.as_str()))
                .map_or(devices.len(), |index| index + 1);
            devices.insert(position, focused);
        }
        devices
    }

    pub fn device_history(&self, name: &str) -> Option<&VecDeque<Sample>> {
        self.history.get(name)
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
            // A rescan can delete or fold the directory currently open. Keep
            // navigation at the surviving ancestor already shown by the UI.
            let (_, depth) = tree.resolve(&self.home_cwd);
            if depth < self.home_cwd.len() {
                self.home_cwd.truncate(depth);
                self.filter.clear();
                self.table.select(Some(0));
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
        let window = self.history_seconds();
        // Removed devices drop their history; a re-added one starts anew.
        let names: BTreeSet<&str> = self
            .snapshot
            .devices
            .iter()
            .map(|device| device.name.as_str())
            .collect();
        self.history.retain(|name, _| names.contains(name.as_str()));
        for device in &self.snapshot.devices {
            let rates = device.rates.as_ref();
            let history = self.history.entry(device.name.clone()).or_default();
            history.push_back(Sample {
                at,
                read: rates.map(|rates| rates.read_rate),
                write: rates.map(|rates| rates.write_rate),
                util: rates.map(|rates| rates.utilization_percent),
                used: device.usage.as_ref().and_then(|usage| usage.percent()),
            });
            while history
                .front()
                .is_some_and(|sample| at - sample.at > window)
            {
                history.pop_front();
            }
            // Also cap storage if the collector's clock does not advance.
            while history.len() > 6001 {
                history.pop_front();
            }
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
            KeyCode::F(4) | KeyCode::Char('v') => {
                self.overlay = Overlay::Views;
                self.picker = VIEWS
                    .iter()
                    .position(|view| *view == self.settings.view)
                    .unwrap_or(0);
            }
            KeyCode::Char('1') => return self.set_view(View::Filesystems),
            KeyCode::Char('2') => return self.set_view(View::Home),
            KeyCode::Char('3') | KeyCode::Char('i') => return self.set_view(View::Processes),
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
                    self.overview_devices()
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
            KeyCode::F(6) if self.settings.view == View::Processes => {
                self.overlay = Overlay::Sort;
                self.picker = SORTS
                    .iter()
                    .position(|sort| *sort == self.settings.sort)
                    .unwrap_or(0);
            }
            KeyCode::Char('s') if self.settings.view == View::Processes => {
                self.settings.sort = self.settings.sort.next();
                self.table.select(Some(0));
            }
            KeyCode::F(9) | KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Esc if home && self.filter.is_empty() && !self.home_cwd.is_empty() => {
                self.home_up()
            }
            KeyCode::Esc if self.filter.is_empty() => return self.set_view(View::Filesystems),
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
            && let Some(node) = self.home_tree.as_ref().and_then(|tree| tree.node(index))
            && (!node.children.is_empty() || node.own_files.files > 0 || node.other.dirs > 0)
        {
            self.home_cwd.push(node.identity().to_owned());
            self.filter.clear();
            self.table.select(Some(0));
        }
    }

    fn home_up(&mut self) {
        if let Some(identity) = self.home_cwd.pop() {
            self.filter.clear();
            let entries = self.home_entries();
            let index = entries
                .iter()
                .position(|entry| {
                    if let HomeEntryKind::Directory(index) = entry.kind {
                        self.home_tree
                            .as_ref()
                            .and_then(|tree| tree.node(index))
                            .is_some_and(|node| node.identity() == identity)
                    } else {
                        false
                    }
                })
                .unwrap_or(0);
            self.table.select(Some(index));
        }
    }

    fn overlay_key(&mut self, key: KeyEvent) -> Action {
        if self.overlay == Overlay::Views {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.picker = self.picker.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.picker = (self.picker + 1).min(VIEWS.len() - 1)
                }
                KeyCode::Enter => {
                    self.overlay = Overlay::None;
                    return self.set_view(VIEWS[self.picker.min(VIEWS.len() - 1)]);
                }
                KeyCode::Char(c @ '1'..='3') => {
                    self.overlay = Overlay::None;
                    return self.set_view(VIEWS[(c as u8 - b'1') as usize]);
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(4) => self.overlay = Overlay::None,
                _ => {}
            }
            return Action::None;
        }
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
            // History is kept per device, so a new focus keeps every graph.
            self.device = device;
            // Changing the focus explicitly resumes live collection.
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
            1 => self.setup_devices().len() + 4,
            2 => 8,
            3 => 4,
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
                    Err(error) => {
                        let message = self.lang().root_error(error);
                        self.show_error(message);
                    }
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
            (1, 1) => self.settings.show_stacked = !self.settings.show_stacked,
            (1, row) => {
                let device = match row {
                    2 => self.auto_device.clone(),
                    3 => None,
                    _ => self.setup_devices().get(row - 4).cloned(),
                };
                self.set_device(device);
                if row == 2 {
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
            (2, 4) => self.settings.write_color = self.settings.write_color.next(direction),
            (2, 5) => self.settings.show_io_series = !self.settings.show_io_series,
            (2, 6) => self.settings.show_usage_series = !self.settings.show_usage_series,
            (2, _) => self.settings.usage_color = self.settings.usage_color.next(direction),
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
            (3, 2) => self.settings.show_idle = !self.settings.show_idle,
            (3, _) => {
                // Cycles 0..=20 like the other choices: Enter or + after 20 is 0.
                let rows = i32::from(self.settings.process_rows) + direction;
                self.settings.process_rows = rows.rem_euclid(i32::from(MAX_PROCESS_ROWS) + 1) as u8;
            }
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
    pub fn home_node(&self) -> Option<(&DirNode, Vec<&str>)> {
        let tree = self.home_tree.as_ref()?;
        let mut node = tree.root_node()?;
        let mut names = Vec::with_capacity(self.home_cwd.len());
        for identity in &self.home_cwd {
            let Some(child) = node.children.iter().find_map(|index| {
                tree.node(*index)
                    .filter(|child| child.identity() == identity)
            }) else {
                break;
            };
            names.push(child.name.as_str());
            node = child;
        }
        Some((node, names))
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

/// How each disk block of the overview is drawn, from most to least detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockStyle {
    /// Label row, four meters and a graph of this many rows beside them.
    Side(u16),
    /// Label row, four meters and a graph of this many rows below them.
    Below(u16),
    /// Label row and four meters.
    Meters,
    /// Label row and two rows of two meters each.
    Pair,
    /// One row per disk.
    Row,
}

impl BlockStyle {
    pub fn height(self) -> u16 {
        match self {
            Self::Side(graph) => 1 + graph.max(4),
            Self::Below(graph) => 5 + graph,
            Self::Meters => 5,
            Self::Pair => 3,
            Self::Row => 1,
        }
    }

    pub fn has_graph(self) -> bool {
        matches!(self, Self::Side(_) | Self::Below(_))
    }
}

/// Placement of the disk blocks: a grid of `columns`, filled row by row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskLayout {
    pub columns: u16,
    pub style: BlockStyle,
    pub grid_rows: usize,
    /// Grid rows on screen; fewer than `grid_rows` adds a "+N more" row.
    pub visible_rows: usize,
}

impl DiskLayout {
    pub fn overflow(&self) -> bool {
        self.visible_rows < self.grid_rows
    }

    pub fn height(&self) -> u16 {
        self.visible_rows as u16 * self.style.height() + u16::from(self.overflow())
    }
}

/// Columns at least this wide put the graph beside the meters.
const SIDE_MIN_WIDTH: u16 = 58;
const GRAPH_MIN: u16 = 4;
const GRAPH_MAX: u16 = 16;
/// Rows reserved for the filesystems and home tables.
const TABLE_MIN: u16 = 5;

fn column_width(width: u16, columns: u16) -> u16 {
    ((width + 1) / columns.max(1)).saturating_sub(1)
}

fn side_meter_width(width: u16) -> u16 {
    (width * 9 / 20).clamp(30, 50)
}

/// Choose the most detailed layout that shows all `count` disks in
/// `width` x `height`: graphs (shorter as needed), then meters only, then
/// one row per disk, and finally a scrolled list with a "+N more" row.
/// Wide terminals use several columns, as htop does for CPU meters.
pub fn disk_layout(width: u16, height: u16, count: usize, graphs: bool) -> DiskLayout {
    let make = |columns: u16, style: BlockStyle| {
        let grid_rows = count.div_ceil(usize::from(columns.max(1)));
        // As few columns as give the same number of rows: 4 disks in three
        // columns become a 2 x 2 grid with wider graphs.
        let columns = count.div_ceil(grid_rows.max(1)) as u16;
        DiskLayout {
            columns,
            style,
            grid_rows,
            visible_rows: grid_rows,
        }
    };
    if count == 0 {
        return make(1, BlockStyle::Row);
    }
    let count_columns = count.min(4) as u16;
    let max_columns = ((width + 1) / 31).clamp(1, 4).min(count_columns);
    let preferred = (width / 60).clamp(1, 4).min(max_columns);
    let fits = |columns: u16, style: BlockStyle| {
        count.div_ceil(usize::from(columns)) * usize::from(style.height()) <= usize::from(height)
    };
    if graphs {
        for columns in preferred..=max_columns {
            let side = column_width(width, columns) >= SIDE_MIN_WIDTH;
            for graph in (GRAPH_MIN..=GRAPH_MAX).rev() {
                let style = if side {
                    BlockStyle::Side(graph)
                } else {
                    BlockStyle::Below(graph)
                };
                if fits(columns, style) {
                    return make(columns, style);
                }
            }
        }
    }
    for style in [BlockStyle::Meters, BlockStyle::Pair] {
        for columns in preferred..=max_columns {
            if fits(columns, style) {
                return make(columns, style);
            }
        }
    }
    // One row per disk needs about 36 columns: the name and four mini meters.
    let row_columns = ((width + 1) / 37).clamp(1, 4).min(count_columns);
    for columns in 1..=row_columns {
        if fits(columns, BlockStyle::Row) {
            return make(columns, BlockStyle::Row);
        }
    }
    let mut layout = make(row_columns, BlockStyle::Row);
    layout.visible_rows = usize::from(height.saturating_sub(1));
    layout
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width < 36 || area.height < 16 {
        frame.render_widget(
            Paragraph::new(app.lang().pick(
                "spctop\nTerminal too small.\nUse at least 36 x 16.\nq / F10 to quit.",
                "spctop\nTerminal zu klein.\nMindestens 36 x 16.\nq / F10 beendet.",
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
    let status = status_line(app);
    let status_rows = u16::from(status.is_some());
    // Title, summary and footer rows; the rest is disks and the lower table.
    let body = area.height - 3 - status_rows;
    let rows = u16::from(app.settings.process_rows);
    let reserved = match app.settings.view {
        View::Processes if rows == 0 => 0,
        // Small terminals keep at most three process rows for the disks.
        View::Processes if body < 20 => 1 + rows.min(3),
        View::Processes => 1 + rows,
        _ => (body / 2).max(TABLE_MIN),
    }
    .min(body.saturating_sub(2));
    let count = app.overview_devices().len();
    let graphs = app.settings.show_graph
        && (app.settings.show_usage_series
            || (app.settings.view == View::Processes && app.settings.show_io_series));
    let mut layout = disk_layout(area.width, body - reserved, count, graphs);
    if count == 0 {
        layout.visible_rows = 0;
    }
    let disk_height = if count == 0 { 1 } else { layout.height() };
    // Keep the focused disk on screen when not all disks fit.
    let columns = usize::from(layout.columns.max(1));
    let max_scroll = layout.grid_rows.saturating_sub(layout.visible_rows);
    if let Some(index) = app
        .overview_devices()
        .iter()
        .position(|device| Some(device.name.as_str()) == app.device.as_deref())
    {
        let row = index / columns;
        if row < app.disk_scroll {
            app.disk_scroll = row;
        } else if row >= app.disk_scroll + layout.visible_rows {
            app.disk_scroll = row + 1 - layout.visible_rows.max(1);
        }
    }
    app.disk_scroll = app.disk_scroll.min(max_scroll);
    app.disk_layout = Some(layout);
    let lower = body - disk_height;
    let layout_rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(status_rows),
        Constraint::Length(disk_height),
        Constraint::Length(lower),
        Constraint::Length(1),
    ])
    .split(area);
    draw_title(frame, app, layout_rows[0]);
    frame.render_widget(
        // Default foreground keeps the summary readable on light and dark themes.
        Paragraph::new(summary_line(app, area.width)),
        layout_rows[1],
    );
    if let Some((text, color)) = status {
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(color)),
            layout_rows[2],
        );
    }
    draw_disks(frame, app, layout_rows[3], layout);
    if lower > 0 && (app.settings.view != View::Processes || rows > 0) {
        match app.settings.view {
            View::Processes => draw_processes(frame, app, layout_rows[4]),
            View::Filesystems => draw_filesystems(frame, app, layout_rows[4]),
            View::Home => draw_home(frame, app, layout_rows[4]),
        }
    } else {
        app.visible_rows = 0;
    }
    draw_footer(frame, app, layout_rows[5]);
    match app.overlay {
        Overlay::Help => draw_help(frame, app, area),
        Overlay::Devices => draw_devices(frame, app, area),
        Overlay::Setup => draw_setup(frame, app, area),
        Overlay::Sort => draw_sort(frame, app, area),
        Overlay::Views => draw_views(frame, app, area),
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
    let count = app.overview_devices().len();
    let mut spans = vec![
        Span::styled("Disks ", Style::default().fg(Color::Green)),
        Span::raw(count.to_string()),
        Span::styled(
            lang.pick("  Focus ", "  Fokus "),
            Style::default().fg(Color::Green),
        ),
        Span::raw("["),
        Span::raw(truncate(
            app.device.as_deref().unwrap_or(lang.pick("all", "alle")),
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
    let used: usize = spans.iter().map(|span| span.content.columns()).sum();
    // View tabs on the right; narrow terminals show only the current view.
    let views = VIEWS;
    let tabs_width: usize = views
        .iter()
        .map(|view| view_label(*view, lang).columns() + 3)
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
        if usize::from(area.width) >= used + label.columns() + 2 {
            right.push(Span::styled(label.to_owned(), inverse(KEY)));
        }
    }
    let right_width: u16 = right.iter().map(|span| span.content.columns() as u16).sum();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    if right_width > 0 && right_width < area.width {
        frame.render_widget(
            Paragraph::new(Line::from(right)),
            Rect::new(area.right() - right_width, area.y, right_width, 1),
        );
    }
}

fn rd_label(lang: Lang) -> &'static str {
    lang.pick("RD", "LE")
}

fn wr_label(lang: Lang) -> &'static str {
    lang.pick("WR", "SC")
}

fn us_label(lang: Lang) -> &'static str {
    lang.pick("US", "BL")
}

/// Join pieces with two spaces while they fit; the first piece is always kept.
fn fit_pieces(pieces: Vec<String>, width: u16) -> String {
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
fn physical_disks(app: &App) -> impl Iterator<Item = &Device> {
    app.snapshot
        .devices
        .iter()
        .filter(|device| !device.partition && !device.kind.is_virtual())
}

/// Used and total bytes of the physical disks' filesystems. A filesystem on
/// several disks (LVM, MD, btrfs RAID, ZFS pools) counts once; replies from older
/// helpers without the filesystem list are summed per disk.
fn combined_usage(app: &App) -> (u64, u64) {
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
fn summary_line(app: &App, width: u16) -> String {
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

fn graph_note(app: &App) -> String {
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

/// One htop-style meter: `RD[|||||     12.3M/s]`.
struct Meter {
    label: &'static str,
    /// Single-letter label for one-row disks on narrow terminals.
    short: &'static str,
    color: Color,
    ratio: f64,
    /// Value texts from longest to shortest; the longest that fits is shown.
    values: Vec<String>,
}

impl Meter {
    fn spans(&self, width: u16, short_label: bool) -> Vec<Span<'static>> {
        let label = if short_label { self.short } else { self.label };
        let inner = usize::from(width).saturating_sub(label.columns() + 2);
        let mut spans = vec![Span::styled(label, Style::default().fg(self.color))];
        if usize::from(width) < label.columns() + 2 {
            return spans;
        }
        let value = self
            .values
            .iter()
            .find(|value| value.columns() + 2 <= inner)
            .or_else(|| self.values.iter().find(|value| value.columns() <= inner))
            .cloned()
            .unwrap_or_else(|| truncate(self.values.last().map_or("", String::as_str), inner));
        let text_start = inner - value.columns();
        let fill = ((self.ratio.clamp(0.0, 1.0) * inner as f64).round() as usize).min(text_start);
        spans.push(Span::raw("["));
        spans.push(Span::styled(
            "|".repeat(fill),
            Style::default().fg(self.color),
        ));
        spans.push(Span::raw(" ".repeat(text_start - fill)));
        // Default foreground keeps values readable on light and dark themes.
        spans.push(Span::raw(value));
        spans.push(Span::raw("]"));
        spans
    }
}

fn rate_values(rate: Option<f64>) -> Vec<String> {
    match rate {
        Some(rate) => vec![
            format_rate(rate),
            format!("{}/s", format_bytes_short(rate)),
            format_bytes_short(rate),
            format_bytes_tiny(rate),
        ],
        None => vec!["-".to_owned()],
    }
}

/// The full scale of every RD and WR meter: the highest read or write rate
/// of any device in the overview within the visible history, so equal bar
/// lengths mean equal rates on every disk.
fn rate_scale(app: &App) -> f64 {
    app.overview_devices()
        .into_iter()
        .filter_map(|device| app.device_history(&device.name))
        .flatten()
        .flat_map(|sample| [sample.read, sample.write])
        .flatten()
        .fold(GRAPH_FLOOR, f64::max)
}

/// The RD, WR, IO and US meters of one device. RD and WR use `peak`, the
/// shared scale of all disks from `rate_scale`.
fn disk_meters(app: &App, device: &Device, peak: f64) -> [Meter; 4] {
    let lang = app.lang();
    let rates = device.rates.as_ref();
    let read = rates.map(|rates| rates.read_rate);
    let write = rates.map(|rates| rates.write_rate);
    let util = rates.map(|rates| rates.utilization_percent.clamp(0.0, 100.0));
    let usage = device.usage.as_ref();
    let percent = usage.and_then(|usage| usage.percent());
    let usage_values = match (usage, percent) {
        (Some(usage), Some(percent)) => {
            let total = usage.used_bytes.saturating_add(usage.available_bytes) as f64;
            let used = usage.used_bytes as f64;
            let partial = if usage.unmeasured > 0 { "!" } else { "" };
            vec![
                format!(
                    "{percent:.0}%{partial} {}/{}",
                    format_bytes(used),
                    format_bytes(total)
                ),
                format!(
                    "{percent:.0}%{partial} {}/{}",
                    format_bytes_short(used),
                    format_bytes_short(total)
                ),
                format!("{percent:.0}%{partial}"),
            ]
        }
        (Some(usage), None) if usage.filesystems == 0 && usage.unmeasured == 0 => {
            if usage.swap {
                vec![
                    lang.pick("swap only", "nur Swap").to_owned(),
                    "swap".to_owned(),
                ]
            } else {
                // "-" would mean "not measured", so the shortest text differs.
                vec![
                    lang.pick("not mounted", "nicht eingehängt").to_owned(),
                    lang.pick("no fs", "kein FS").to_owned(),
                    lang.pick("none", "ohne").to_owned(),
                ]
            }
        }
        _ => vec!["-".to_owned()],
    };
    [
        Meter {
            label: rd_label(lang),
            short: lang.pick("R", "L"),
            color: plot_color(app.settings.read_color),
            ratio: read.map_or(0.0, |rate| rate / peak),
            values: rate_values(read),
        },
        Meter {
            label: wr_label(lang),
            short: lang.pick("W", "S"),
            color: plot_color(app.settings.write_color),
            ratio: write.map_or(0.0, |rate| rate / peak),
            values: rate_values(write),
        },
        Meter {
            label: "IO",
            short: "I",
            color: UTIL,
            ratio: util.map_or(0.0, |util| util / 100.0),
            values: util.map_or_else(
                || vec!["-".to_owned()],
                |util| vec![format!("{util:.1}%"), format!("{util:.0}%")],
            ),
        },
        Meter {
            label: us_label(lang),
            short: lang.pick("U", "B"),
            // Nearly full filesystems turn the meter red.
            color: if percent.is_some_and(|percent| percent >= 90.0) {
                Color::Red
            } else {
                plot_color(app.settings.usage_color)
            },
            ratio: percent.map_or(0.0, |percent| percent / 100.0),
            values: usage_values,
        },
    ]
}

/// The label row of a disk block: name, type, model, size and mounts.
fn disk_label(app: &App, device: &Device, width: u16, focused: bool) -> Line<'static> {
    let lang = app.lang();
    let mut kind = device.kind.label().to_owned();
    if device.partition
        && let Some(parent) = &device.parent
    {
        kind.push_str(&match lang {
            Lang::En => format!(" partition of {parent}"),
            Lang::De => format!(" Partition von {parent}"),
        });
    }
    if device.removable {
        kind.push_str(lang.pick(" removable", " wechselbar"));
    }
    let identity = [
        device.vendor.as_deref(),
        device.model.as_deref(),
        device.label.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    let mut details = format!(" [{kind}]");
    if let Some(size) = device.size_bytes {
        details.push_str(&format!(" {}", format_bytes(size as f64)));
    }
    let width = usize::from(width);
    let name = truncate(&device.name, width);
    let details = truncate(&details, width - name.columns());
    let mut room = width - name.columns() - details.columns();
    // Mounts before the model: they tell what the disk is used for. Mounts
    // that do not fit are counted as "+N".
    let mut mounts = String::new();
    for (index, mount) in device.mounts.iter().enumerate() {
        let rest = device.mounts.len() - index - 1;
        let more = if rest > 0 {
            format!(" +{rest}")
        } else {
            String::new()
        };
        let separator = if index == 0 { "  " } else { " " };
        if mounts.columns() + separator.len() + mount.columns() + more.len() <= room {
            mounts.push_str(separator);
            mounts.push_str(mount);
        } else {
            let more = format!(
                "{}+{}",
                if index == 0 { "  " } else { " " },
                device.mounts.len() - index
            );
            if mounts.columns() + more.len() <= room {
                mounts.push_str(&more);
            }
            break;
        }
    }
    room -= mounts.columns();
    let identity = if identity.is_empty() || room < 4 {
        String::new()
    } else {
        truncate(&format!("  {identity}"), room)
    };
    if focused {
        // The focused disk is shown in reverse video across the whole row.
        let text = format!("{name}{details}{mounts}{identity}");
        let used = text.columns();
        return Line::from(vec![Span::styled(
            format!("{text}{}", " ".repeat(width - used)),
            inverse(KEY).add_modifier(Modifier::BOLD),
        )]);
    }
    Line::from(vec![
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(details),
        Span::raw(mounts),
        Span::styled(identity, Style::default().fg(DIM)),
    ])
}

fn draw_disks(frame: &mut Frame<'_>, app: &App, area: Rect, layout: DiskLayout) {
    let lang = app.lang();
    let devices = app.overview_devices();
    if devices.is_empty() {
        empty_message(
            frame,
            area,
            0,
            lang.pick(
                "No block devices to show (F2 Device: loop, ram, zram).",
                "Keine Blockgeräte (F2 Gerät: loop, ram, zram).",
            ),
            DIM,
        );
        return;
    }
    let columns = layout.columns.max(1);
    let width = column_width(area.width, columns);
    let height = layout.style.height();
    let first = app.disk_scroll * usize::from(columns);
    let shown = layout.visible_rows * usize::from(columns);
    let scale = rate_scale(app);
    // Row style: one shared name width keeps the meters aligned.
    let name_width = devices
        .iter()
        .map(|device| device.name.columns())
        .max()
        .unwrap_or(4)
        .clamp(4, if width < 44 { 7 } else { 12 }) as u16;
    for (slot, device) in devices.iter().skip(first).take(shown).enumerate() {
        let column = slot as u16 % columns;
        let row = (slot / usize::from(columns)) as u16;
        // Distribute the remainder of the width to the first columns.
        let extra = (area.width + 1) - (width + 1) * columns;
        let x = area.x + column * (width + 1) + column.min(extra);
        let block = Rect::new(
            x,
            area.y + row * height,
            width + u16::from(column < extra),
            height,
        );
        let focused = app.device.as_deref() == Some(device.name.as_str());
        draw_disk(
            frame,
            app,
            device,
            block,
            layout.style,
            focused,
            name_width,
            scale,
        );
    }
    if layout.overflow() {
        let hidden = devices.len().saturating_sub(shown);
        let above = first;
        let below = hidden - above.min(hidden);
        let arrows = format!(
            "{}{}",
            if above > 0 {
                format!(" ↑{above}")
            } else {
                String::new()
            },
            if below > 0 {
                format!(" ↓{below}")
            } else {
                String::new()
            }
        );
        let text = match lang {
            Lang::En => format!("+{hidden} more disks{arrows}  Tab / F5 focus scrolls"),
            Lang::De => format!("+{hidden} weitere Disks{arrows}  Tab / F5 Fokus blättert"),
        };
        frame.render_widget(
            Paragraph::new(truncate(&text, usize::from(area.width)))
                .style(Style::default().fg(Color::Yellow)),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_disk(
    frame: &mut Frame<'_>,
    app: &App,
    device: &Device,
    area: Rect,
    style: BlockStyle,
    focused: bool,
    name_width: u16,
    scale: f64,
) {
    let meters = disk_meters(app, device, scale);
    let line = |y: u16, x: u16, width: u16| Rect::new(x, area.y + y, width, 1);
    if app.settings.view != View::Processes {
        if style == BlockStyle::Row {
            let name_width = name_width.min(area.width.saturating_sub(1));
            frame.render_widget(
                Paragraph::new(pad(
                    &truncate(&device.name, usize::from(name_width)),
                    usize::from(name_width),
                ))
                .style(if focused {
                    inverse(KEY)
                } else {
                    Style::default()
                }),
                line(0, area.x, name_width),
            );
            frame_meter(
                frame,
                &meters[3],
                line(
                    0,
                    area.x + name_width + 1,
                    area.width.saturating_sub(name_width + 1),
                ),
            );
            return;
        }
        frame.render_widget(
            Paragraph::new(disk_label(app, device, area.width, focused)),
            line(0, area.x, area.width),
        );
        frame_meter(frame, &meters[3], line(1, area.x, area.width));
        if area.height >= 3 {
            let free = device
                .usage
                .as_ref()
                .filter(|usage| usage.percent().is_some())
                .map_or_else(
                    || "-".to_owned(),
                    |usage| format_bytes(usage.available_bytes as f64),
                );
            frame.render_widget(
                Paragraph::new(format!("{} {free}", app.lang().pick("Free", "Frei"))),
                line(2, area.x, area.width),
            );
        }
        if style.has_graph() && area.height >= 6 {
            draw_disk_graph(
                frame,
                app,
                &device.name,
                Rect::new(area.x, area.y + 3, area.width, area.height - 3),
            );
        }
        return;
    }
    if style == BlockStyle::Row {
        let name_width = name_width.min(area.width.saturating_sub(1));
        let name = truncate(&device.name, usize::from(name_width));
        let name_style = if focused {
            inverse(KEY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        frame.render_widget(
            Paragraph::new(pad(&name, usize::from(name_width))).style(name_style),
            line(0, area.x, name_width),
        );
        let rest = area.width.saturating_sub(name_width + 1);
        let each = rest / 4;
        let short = each < 10;
        // Brackets separate the meters; a gap is added when there is room.
        let gap = u16::from(each >= 9);
        let mut x = area.x + name_width + 1;
        for (index, meter) in meters.iter().enumerate() {
            let width = each + u16::from((index as u16) < rest % 4);
            frame.render_widget(
                Paragraph::new(Line::from(meter.spans(width - gap, short))),
                line(0, x, width),
            );
            x += width;
        }
        return;
    }
    frame.render_widget(
        Paragraph::new(disk_label(app, device, area.width, focused)),
        line(0, area.x, area.width),
    );
    match style {
        BlockStyle::Side(graph) => {
            let width = side_meter_width(area.width);
            for (index, meter) in meters.iter().enumerate() {
                frame_meter(frame, meter, line(1 + index as u16, area.x, width));
            }
            draw_disk_graph(
                frame,
                app,
                &device.name,
                Rect::new(
                    area.x + width + 1,
                    area.y + 1,
                    area.width - width - 1,
                    graph.max(4),
                ),
            );
        }
        BlockStyle::Below(graph) => {
            for (index, meter) in meters.iter().enumerate() {
                frame_meter(frame, meter, line(1 + index as u16, area.x, area.width));
            }
            draw_disk_graph(
                frame,
                app,
                &device.name,
                Rect::new(area.x, area.y + 5, area.width, graph),
            );
        }
        BlockStyle::Meters => {
            for (index, meter) in meters.iter().enumerate() {
                frame_meter(frame, meter, line(1 + index as u16, area.x, area.width));
            }
        }
        _ => {
            let left = (area.width - 1) / 2;
            let right = area.width - left - 1;
            for (index, meter) in meters.iter().enumerate() {
                let row = 1 + index as u16 / 2;
                if index % 2 == 0 {
                    frame_meter(frame, meter, line(row, area.x, left));
                } else {
                    frame_meter(frame, meter, line(row, area.x + left + 1, right));
                }
            }
        }
    }
}

fn frame_meter(frame: &mut Frame<'_>, meter: &Meter, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(meter.spans(area.width, false))),
        area,
    );
}

/// Reads one series from a history sample.
type SampleValue = fn(&Sample) -> Option<f64>;

/// Per-disk history: utilization and fill level on one 0-100% scale.
fn draw_disk_graph(frame: &mut Frame<'_>, app: &App, name: &str, area: Rect) {
    if area.width < 12 || area.height < 3 {
        return;
    }
    let lang = app.lang();
    let axis = Style::default().fg(DIM);
    let label_width = 4;
    let buffer = frame.buffer_mut();
    buffer.set_stringn(area.x, area.y, "100%", 4, axis);
    buffer.set_stringn(area.x, area.bottom() - 1, "  0%", 4, axis);
    if area.height >= 7 {
        buffer.set_stringn(area.x, area.y + (area.height - 1) / 2, " 50%", 4, axis);
    }
    let usage_color = plot_color(app.settings.usage_color);
    // The legend sits in the axis column, so it never hides the curves.
    let mut legend_row = 1;
    for (enabled, label, color) in [
        (
            app.settings.view == View::Processes && app.settings.show_io_series,
            "IO",
            UTIL,
        ),
        (app.settings.show_usage_series, us_label(lang), usage_color),
    ] {
        if enabled && legend_row < area.height - 1 {
            buffer.set_stringn(
                area.x + 1,
                area.y + legend_row,
                label,
                2,
                Style::default().fg(color),
            );
            legend_row += 1;
        }
    }
    // A thin axis line separates the labels from the plot.
    for y in area.y..area.bottom() {
        buffer[(area.x + label_width, y)]
            .set_symbol("│")
            .set_style(axis);
    }
    let plot = Rect {
        x: area.x + label_width + 1,
        width: area.width - label_width - 1,
        ..area
    };
    let Some(history) = app.device_history(name) else {
        return;
    };
    let window = app.history_seconds();
    let now = history.back().map_or(0.0, |sample| sample.at);
    // Fill level first, so utilization stays visible where both meet.
    let series: Vec<(Color, SampleValue)> = [
        (
            app.settings.show_usage_series,
            usage_color,
            (|sample: &Sample| sample.used) as SampleValue,
        ),
        (
            app.settings.view == View::Processes && app.settings.show_io_series,
            UTIL,
            |sample: &Sample| sample.util,
        ),
    ]
    .into_iter()
    .filter(|(enabled, _, _)| *enabled)
    .map(|(_, color, value)| (color, value))
    .collect();
    if app.settings.graph_style == GraphStyle::Steps {
        for (color, value) in &series {
            let points = history.iter().filter_map(|sample| {
                let progress = (sample.at - now + window) / window;
                let value = value(sample)?;
                (0.0..=1.0)
                    .contains(&progress)
                    .then_some((progress, value / 100.0))
            });
            draw_steps(frame.buffer_mut(), plot, points, *color);
        }
        return;
    }
    let data: Vec<Vec<(f64, f64)>> = series
        .iter()
        .map(|(_, value)| {
            history
                .iter()
                .filter_map(|sample| Some((sample.at - now, value(sample)?.clamp(0.0, 100.0))))
                .collect()
        })
        .collect();
    let datasets = series
        .iter()
        .zip(&data)
        .map(|((color, _), data)| {
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(*color))
                .data(data)
        })
        .collect();
    let chart = Chart::new(datasets)
        .x_axis(Axis::default().bounds([-window, 0.0]))
        .y_axis(Axis::default().bounds([0.0, 100.0]));
    frame.render_widget(chart, plot);
}

/// nvtop-style stepped line from (progress 0..1, value 0..1) points, drawn
/// with terminal line characters rather than colored dots.
fn draw_steps(
    buffer: &mut Buffer,
    plot: Rect,
    points: impl Iterator<Item = (f64, f64)>,
    color: Color,
) {
    if plot.width == 0 || plot.height == 0 {
        return;
    }
    let mut columns = vec![None; usize::from(plot.width)];
    for (progress, value) in points {
        let x = (progress * f64::from(plot.width - 1)).round() as usize;
        let y = ((1.0 - value.clamp(0.0, 1.0)) * f64::from(plot.height - 1)).round() as u16;
        let last = columns.len() - 1;
        columns[x.min(last)] = Some(y);
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
            buffer[(plot.x + x, plot.y + y)]
                .set_symbol(symbol)
                .set_style(Style::default().fg(color));
        }
    }
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
    if !app.snapshot.truncated.is_empty() {
        return Some((lang.truncated(&app.snapshot.truncated), Color::Yellow));
    }
    let unavailable_pools: BTreeSet<_> = app
        .snapshot
        .filesystems
        .iter()
        .filter_map(|filesystem| filesystem.zfs.as_ref())
        .filter(|pool| pool.error.is_some())
        .map(|pool| crate::text::sanitize(&pool.name))
        .collect();
    if !unavailable_pools.is_empty() {
        return Some((
            format!(
                "{}: {}",
                lang.pick("ZFS pool data unavailable", "ZFS-Pooldaten nicht verfügbar"),
                unavailable_pools.into_iter().collect::<Vec<_>>().join(", ")
            ),
            Color::Yellow,
        ));
    }
    if app.demo || app.settings.view != View::Processes {
        return None; // DEMO is labeled in the title; other views show their own state.
    }
    let access = &app.snapshot.process_access;
    if access.message.contains("/proc hidepid") {
        return Some((
            lang.pick(
                "hidepid: process details restricted (F1)",
                "hidepid: Prozessdetails beschränkt (F1)",
            )
            .to_owned(),
            Color::Yellow,
        ));
    }
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
    let mount_header = lang.pick("MOUNT", "EINHÄNGEPUNKT");
    let free_header = lang.pick("FREE", "FREI");
    let size_header = lang.pick("USED", "BELEGT");
    let percent =
        |value: Option<f64>| value.map_or_else(|| "-".to_owned(), |value| format!("{value:.0}%"));
    let mount = |row: &Filesystem| {
        format!(
            "{}{}{}",
            if row.contains_home { "*" } else { " " },
            row.mount_point,
            if row.not_responding {
                format!("  {}", lang.not_responding())
            } else {
                String::new()
            }
        )
    };
    let style = |row: &Filesystem| {
        if row.not_responding {
            Style::default().fg(Color::Yellow)
        } else if row.contains_home {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    };
    let (headers, widths, rows): (Vec<(&str, bool)>, Vec<Constraint>, Vec<Row<'_>>) =
        if wide {
            (
                vec![
                    (mount_header, false),
                    (lang.pick("DEVICE", "GERÄT"), false),
                    (lang.pick("TYPE", "TYP"), false),
                    (lang.pick("USE", "BELEGT"), false),
                    (size_header, true),
                    (free_header, true),
                    ("INODES", true),
                ],
                vec![
                    Constraint::Min(12),
                    Constraint::Length(14),
                    Constraint::Length(6),
                    Constraint::Length(17),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(6),
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
                            number_cell(optional_bytes(row.used_bytes)),
                            number_cell(optional_bytes(row.available_bytes)),
                            number_cell(percent(row.inode_percent())),
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
                            number_cell(optional_bytes(row.used_bytes)),
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
                    (size_header, true),
                    (free_header, true),
                ],
                vec![
                    Constraint::Min(10),
                    Constraint::Length(5),
                    Constraint::Length(8),
                    Constraint::Length(8),
                ],
                records
                    .into_iter()
                    .map(|row| {
                        let used = row.use_percent();
                        Row::new(vec![
                            Cell::from(mount(row)),
                            number_cell(percent(used))
                                .style(Style::default().fg(use_color(used.unwrap_or(0.0)))),
                            number_cell(row.used_bytes.map_or_else(
                                || "-".into(),
                                |value| format_bytes_short(value as f64),
                            )),
                            number_cell(row.available_bytes.map_or_else(
                                || "-".into(),
                                |value| format_bytes_short(value as f64),
                            )),
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
            match (&status.error, status.error_kind) {
                (Some(error), Some(kind)) => {
                    text.push_str(&lang.home_error(kind, &status.root, error));
                }
                (Some(error), None) => text.push_str(error),
                (None, _) => text.push_str(lang.pick("scan failed", "Scan fehlgeschlagen")),
            }
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
    if status.is_some_and(|status| status.helper_refused) {
        text.push_str("  ");
        text.push_str(&lang.helper_refused());
        if color == Color::Reset {
            color = Color::Yellow;
        }
    }
    if width >= 100 {
        text.push_str(lang.pick("  r rescan", "  r neu scannen"));
    }
    let path_width = usize::from(width).saturating_sub(text.columns()).max(8);
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
        ("F2", if area.width < 40 { "Set" } else { "Setup" }),
    ];
    if area.width >= 66 {
        keys.push(("F3", lang.pick("Search", "Suche")));
    }
    keys.push(("F4", lang.pick("View", "Sicht")));
    if area.width >= 80 {
        keys.push(("F5", lang.pick("Focus", "Fokus")));
    }
    if app.settings.view == View::Processes && area.width >= 44 {
        keys.push((
            "F6",
            if wide {
                lang.pick("Sort", "Sortieren")
            } else {
                lang.pick("Sort", "Sort")
            },
        ));
    }
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
        .map(|(key, label)| key.columns() + label.columns())
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

fn draw_views(frame: &mut Frame<'_>, app: &App, area: Rect) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{demo, model::DeviceKind};
    use ratatui::{Terminal, backend::TestBackend};

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
                    "RD[", "WR[", "IO[", "LE[", "SC[", "READ/s", "WRITE/s", "IOPS", "COMMAND",
                    "BEFEHL",
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

    const SIZES: [(u16, u16); 7] = [
        (36, 16),
        (40, 24),
        (52, 18),
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
                                !text.contains("RD[")
                                    && !text.contains("WR[")
                                    && !text.contains("IO["),
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
}
