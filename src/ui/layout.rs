//! Pure layout selection for the disk overview.

/// How each disk block of the overview is drawn, from most to least detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockStyle {
    /// Capacity label, used/free rows and a fill graph below (zero hides it).
    Space(u16),
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
            Self::Space(graph) => 3 + graph,
            Self::Side(graph) => 1 + graph.max(4),
            Self::Below(graph) => 5 + graph,
            Self::Meters => 5,
            Self::Pair => 3,
            Self::Row => 1,
        }
    }

    pub fn has_graph(self) -> bool {
        matches!(self, Self::Side(_) | Self::Below(_) | Self::Space(1..))
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
pub(super) const TABLE_MIN: u16 = 5;

pub(super) fn column_width(width: u16, columns: u16) -> u16 {
    ((width + 1) / columns.max(1)).saturating_sub(1)
}

pub(super) fn side_meter_width(width: u16) -> u16 {
    (width * 9 / 20).clamp(30, 50)
}

/// Choose the most detailed layout that shows all `count` disks in
/// `width` x `height`: graphs (shorter as needed), then meters only, then
/// one row per disk, and finally a scrolled list with a "+N more" row.
/// Wide terminals use several columns, as htop does for CPU meters.
pub fn disk_layout(width: u16, height: u16, count: usize, graphs: bool) -> DiskLayout {
    disk_layout_for_view(width, height, count, graphs, false)
}

pub(super) fn disk_layout_for_view(
    width: u16,
    height: u16,
    count: usize,
    graphs: bool,
    space: bool,
) -> DiskLayout {
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
                let style = if space {
                    BlockStyle::Space(graph)
                } else if side {
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
    let meter_styles: &[BlockStyle] = if space {
        &[BlockStyle::Space(0)]
    } else {
        &[BlockStyle::Meters, BlockStyle::Pair]
    };
    for &style in meter_styles {
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
