//! Keyboard navigation, overlays and settings edits.

use super::{Action, App, CATEGORY_COUNT, HomeEntryKind, Overlay, RESCAN_STEPS, SORTS, VIEWS};
use crate::config::{Field, MAX_PROCESS_ROWS, View};
use crate::scanner;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl App {
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        let before = self.settings.clone();
        let action = self.handle_key_inner(key);
        self.touched.extend(self.settings.differing(&before));
        action
    }

    pub(super) fn handle_key_inner(&mut self, key: KeyEvent) -> Action {
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

    pub(super) fn set_view(&mut self, view: View) -> Action {
        self.settings.view = view;
        self.filter.clear();
        self.table.select(Some(0));
        if view == View::Home && !self.home_active {
            self.home_active = true;
            return Action::HomeChanged;
        }
        Action::None
    }

    pub(super) fn home_enter(&mut self) {
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

    pub(super) fn home_up(&mut self) {
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

    pub(super) fn overlay_key(&mut self, key: KeyEvent) -> Action {
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

    pub(super) fn set_device(&mut self, device: Option<String>) {
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

    pub(super) fn setup_devices(&self) -> Vec<String> {
        self.visible_devices()
            .into_iter()
            .map(|device| device.name.clone())
            .collect()
    }

    pub(super) fn setup_len(&self) -> usize {
        match self.setup_category {
            0 => 3,
            1 => self.setup_devices().len() + 4,
            2 => 8,
            3 => 4,
            _ => 4,
        }
    }

    pub(super) fn setup_key(&mut self, key: KeyEvent) -> Action {
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
    pub(super) fn edit_key(&mut self, key: KeyEvent) -> Action {
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

    pub(super) fn change_setup(&mut self, direction: i32) -> Action {
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

    pub(super) fn move_selection(&mut self, delta: isize) {
        let count = self.row_count();
        if count == 0 {
            self.table.select(None);
            return;
        }
        let current = self.table.selected().unwrap_or(0) as isize;
        self.table
            .select(Some((current + delta).clamp(0, count as isize - 1) as usize));
    }

    pub(super) fn row_count(&self) -> usize {
        match self.settings.view {
            View::Processes => self.process_rows().len(),
            View::Filesystems => self.filesystem_rows().len(),
            View::Home => self.home_entries().len(),
        }
    }
}
