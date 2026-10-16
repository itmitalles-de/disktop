//! Snapshot history, selection and visible table rows.

use super::{App, HomeEntry, HomeEntryKind, Overlay, Sample, Sort};
use crate::config::{Settings, View};
use crate::diskstats;
use crate::i18n::Lang;
use crate::model::{
    Device, DeviceRates, DirNode, Filesystem, Folded, HomeRequest, ProcessRow, Snapshot,
};
use ratatui::widgets::TableState;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

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
        self.history.clear();
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
        // A restarted collector has a new monotonic clock and rate baseline.
        if at < self.snapshot.elapsed {
            self.history.clear();
        }
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

    pub(super) fn clamp_selection(&mut self) {
        let count = self.row_count();
        self.table
            .select((count > 0).then(|| self.table.selected().unwrap_or(0).min(count - 1)));
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
