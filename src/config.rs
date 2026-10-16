//! User-owned UI preferences. The privileged collector never reads this file.

use std::{path::PathBuf, sync::atomic::AtomicU64};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

mod storage;

#[cfg(test)]
mod tests;

// Additive optional fields keep version 1: older releases ignore them and newer
// releases fill missing fields with defaults.
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphStyle {
    #[default]
    Steps,
    Braille,
}

impl GraphStyle {
    pub fn next(self, direction: i32) -> Self {
        if direction.rem_euclid(2) == 0 {
            self
        } else {
            match self {
                Self::Steps => Self::Braille,
                Self::Braille => Self::Steps,
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Steps => "Steps",
            Self::Braille => "Braille",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlotColor {
    #[default]
    Green,
    Yellow,
    Cyan,
    Red,
    Blue,
    Magenta,
    White,
}

impl PlotColor {
    pub fn next(self, direction: i32) -> Self {
        let colors = [
            Self::Green,
            Self::Yellow,
            Self::Cyan,
            Self::Red,
            Self::Blue,
            Self::Magenta,
            Self::White,
        ];
        let index = colors.iter().position(|color| *color == self).unwrap_or(0);
        colors[(index as i64 + i64::from(direction)).rem_euclid(colors.len() as i64) as usize]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Green => "Green",
            Self::Yellow => "Yellow",
            Self::Cyan => "Cyan",
            Self::Red => "Red",
            Self::Blue => "Blue",
            Self::Magenta => "Magenta",
            Self::White => "White",
        }
    }
}

/// UI language. `Auto` follows LC_ALL, LC_MESSAGES or LANG at startup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    #[default]
    Auto,
    En,
    De,
}

impl Language {
    pub fn next(self, direction: i32) -> Self {
        let choices = [Self::Auto, Self::En, Self::De];
        let index = choices
            .iter()
            .position(|choice| *choice == self)
            .unwrap_or(0);
        choices[(index as i64 + i64::from(direction)).rem_euclid(choices.len() as i64) as usize]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    /// Read plus write rate.
    #[default]
    Io,
    Read,
    Write,
    /// Cumulative read plus write bytes since the process started.
    Total,
    Pid,
    Name,
}

/// The table below the disk overview.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[serde(rename = "io", alias = "processes")]
    Processes,
    #[default]
    Filesystems,
    Home,
}

impl View {
    pub fn next(self, direction: i32) -> Self {
        let views = [Self::Filesystems, Self::Home, Self::Processes];
        let index = views.iter().position(|view| *view == self).unwrap_or(0);
        views[(index as i64 + i64::from(direction)).rem_euclid(views.len() as i64) as usize]
    }
}

pub const DEFAULT_HOME: &str = "/home";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// Block device name, "all", or None for automatic selection.
    pub device: Option<String>,
    pub interval_ms: u64,
    pub history_seconds: u16,
    pub color: bool,
    pub show_graph: bool,
    pub graph_style: GraphStyle,
    pub read_color: PlotColor,
    pub write_color: PlotColor,
    pub view: View,
    pub sort: SortKey,
    pub show_idle: bool,
    pub language: Language,
    /// Show loop, ram and zram devices (and their filesystems).
    pub show_virtual: bool,
    /// Show device-mapper and MD devices that sit on a single shown disk as
    /// their own overview blocks; that disk already includes their mounts.
    pub show_stacked: bool,
    pub home_path: String,
    pub rescan_minutes: u16,
    /// Show apparent sizes (st_size) instead of allocated disk usage.
    pub apparent_size: bool,
    /// Rows of the process strip below the disks (0 hides it, at most 20).
    /// Rows left free by the disk overview are added to the strip.
    pub process_rows: u8,
    /// Per-disk graph series: utilization and fill level, both 0-100%.
    pub show_io_series: bool,
    pub show_usage_series: bool,
    /// Color of the fill-level meter and graph series.
    pub usage_color: PlotColor,
}

pub const MAX_PROCESS_ROWS: u8 = 20;

/// One user preference, used to persist only what the user changed at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    Device,
    IntervalMs,
    HistorySeconds,
    Color,
    ShowGraph,
    GraphStyle,
    ReadColor,
    WriteColor,
    View,
    Sort,
    ShowIdle,
    Language,
    ShowVirtual,
    ShowStacked,
    HomePath,
    RescanMinutes,
    ApparentSize,
    ProcessRows,
    ShowIoSeries,
    ShowUsageSeries,
    UsageColor,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: VERSION,
            device: None,
            interval_ms: 1000,
            history_seconds: 60,
            color: true,
            show_graph: true,
            graph_style: GraphStyle::Steps,
            read_color: PlotColor::Green,
            write_color: PlotColor::Yellow,
            view: View::Filesystems,
            sort: SortKey::Io,
            show_idle: false,
            language: Language::Auto,
            show_virtual: false,
            show_stacked: false,
            home_path: DEFAULT_HOME.into(),
            rescan_minutes: 5,
            apparent_size: false,
            process_rows: 5,
            show_io_series: true,
            show_usage_series: true,
            usage_color: PlotColor::Magenta,
        }
    }
}

impl Settings {
    /// Preferences whose values differ between both settings.
    pub fn differing(&self, other: &Self) -> Vec<Field> {
        // Exhaustive destructuring makes a new preference a compile error here
        // until it is also handled by `copy_field`.
        let Self {
            version: _,
            device,
            interval_ms,
            history_seconds,
            color,
            show_graph,
            graph_style,
            read_color,
            write_color,
            view,
            sort,
            show_idle,
            language,
            show_virtual,
            show_stacked,
            home_path,
            rescan_minutes,
            apparent_size,
            process_rows,
            show_io_series,
            show_usage_series,
            usage_color,
        } = self;
        [
            (Field::Device, *device != other.device),
            (Field::IntervalMs, *interval_ms != other.interval_ms),
            (
                Field::HistorySeconds,
                *history_seconds != other.history_seconds,
            ),
            (Field::Color, *color != other.color),
            (Field::ShowGraph, *show_graph != other.show_graph),
            (Field::GraphStyle, *graph_style != other.graph_style),
            (Field::ReadColor, *read_color != other.read_color),
            (Field::WriteColor, *write_color != other.write_color),
            (Field::View, *view != other.view),
            (Field::Sort, *sort != other.sort),
            (Field::ShowIdle, *show_idle != other.show_idle),
            (Field::Language, *language != other.language),
            (Field::ShowVirtual, *show_virtual != other.show_virtual),
            (Field::ShowStacked, *show_stacked != other.show_stacked),
            (Field::HomePath, *home_path != other.home_path),
            (
                Field::RescanMinutes,
                *rescan_minutes != other.rescan_minutes,
            ),
            (Field::ApparentSize, *apparent_size != other.apparent_size),
            (Field::ProcessRows, *process_rows != other.process_rows),
            (Field::ShowIoSeries, *show_io_series != other.show_io_series),
            (
                Field::ShowUsageSeries,
                *show_usage_series != other.show_usage_series,
            ),
            (Field::UsageColor, *usage_color != other.usage_color),
        ]
        .into_iter()
        .filter_map(|(field, differs)| differs.then_some(field))
        .collect()
    }

    pub fn copy_field(&mut self, from: &Self, field: Field) {
        match field {
            Field::Device => self.device.clone_from(&from.device),
            Field::IntervalMs => self.interval_ms = from.interval_ms,
            Field::HistorySeconds => self.history_seconds = from.history_seconds,
            Field::Color => self.color = from.color,
            Field::ShowGraph => self.show_graph = from.show_graph,
            Field::GraphStyle => self.graph_style = from.graph_style,
            Field::ReadColor => self.read_color = from.read_color,
            Field::WriteColor => self.write_color = from.write_color,
            Field::View => self.view = from.view,
            Field::Sort => self.sort = from.sort,
            Field::ShowIdle => self.show_idle = from.show_idle,
            Field::Language => self.language = from.language,
            Field::ShowVirtual => self.show_virtual = from.show_virtual,
            Field::ShowStacked => self.show_stacked = from.show_stacked,
            Field::HomePath => self.home_path.clone_from(&from.home_path),
            Field::RescanMinutes => self.rescan_minutes = from.rescan_minutes,
            Field::ApparentSize => self.apparent_size = from.apparent_size,
            Field::ProcessRows => self.process_rows = from.process_rows,
            Field::ShowIoSeries => self.show_io_series = from.show_io_series,
            Field::ShowUsageSeries => self.show_usage_series = from.show_usage_series,
            Field::UsageColor => self.usage_color = from.usage_color,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            bail!(
                "unsupported settings version {}; this dwtop supports version {VERSION}",
                self.version
            );
        }
        if !(100..=60_000).contains(&self.interval_ms) {
            bail!("interval_ms must be between 100 and 60000");
        }
        if !(10..=600).contains(&self.history_seconds) {
            bail!("history_seconds must be between 10 and 600");
        }
        if !(1..=1440).contains(&self.rescan_minutes) {
            bail!("rescan_minutes must be between 1 and 1440");
        }
        if self.process_rows > MAX_PROCESS_ROWS {
            bail!("process_rows must be between 0 and {MAX_PROCESS_ROWS}");
        }
        if let Some(name) = &self.device
            && !crate::diskstats::valid_name(name)
        {
            bail!("device must be 'all' or a block device name such as nvme0n1");
        }
        crate::scanner::validate_root(&self.home_path)
            .map_err(|error| anyhow::anyhow!("home_path: {error}"))?;
        Ok(())
    }
}

/// Loaded preferences plus top-level keys that this version does not know.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    pub settings: Settings,
    pub unknown_keys: Vec<String>,
}

/// Saving or loading as root in another user's settings directory, typically
/// after `sudo -E dwtop`, would create root-owned files there.
#[derive(Debug)]
pub struct RootWithForeignSettings;

impl std::fmt::Display for RootWithForeignSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "dwtop runs as root, but the settings directory belongs to another user \
             (for example after sudo -E); start dwtop without sudo to change preferences",
        )
    }
}

impl std::error::Error for RootWithForeignSettings {}

#[derive(Clone, Debug)]
pub struct ConfigFile {
    path: PathBuf,
    previous_path: Option<PathBuf>,
    legacy_path: Option<PathBuf>,
}
