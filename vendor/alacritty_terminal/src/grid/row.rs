//! Defines the Row type which makes up lines in the grid.

use std::cmp::{max, min};
use std::sync::Arc;
use std::ops::{Index, IndexMut, Range, RangeFrom, RangeFull, RangeTo, RangeToInclusive};
use std::slice;

#[cfg(feature = "serde")]
use serde::de::Deserializer;
#[cfg(feature = "serde")]
use serde::ser::{SerializeSeq, SerializeStruct, Serializer};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::grid::GridCell;
use crate::index::Column;
use crate::term::cell::ResetDiscriminant;

/// A small completed-row allocation is cheaper to reuse than to return to the
/// allocator and recreate for the next line. The retained space is bounded by
/// the hot-history frontier and included in storage metrics.
const COMPACT_ROW_REUSE_CELLS: usize = 16;

/// Dictionary-compressed cell storage shared by a bounded group of cold rows.
///
/// Every logical row keeps its own range into `indices`; repeated cell values
/// are retained once in `values`. Returning a cell still produces an ordinary
/// `&T`, so packing does not leak through Alacritty's grid API.
#[derive(Debug)]
pub(crate) struct PackedBlock<T> {
    cells: PackedCells<T>,
}

#[derive(Debug)]
enum PackedCells<T> {
    Direct(Vec<T>),
    Indexed8 { values: Vec<T>, indices: Vec<u8> },
    Indexed16 { values: Vec<T>, indices: Vec<u16> },
}

impl<T> PackedBlock<T> {
    #[inline]
    fn cell(&self, index: usize) -> &T {
        match &self.cells {
            PackedCells::Direct(cells) => &cells[index],
            PackedCells::Indexed8 { values, indices } => &values[indices[index] as usize],
            PackedCells::Indexed16 { values, indices } => &values[indices[index] as usize],
        }
    }

    #[inline]
    pub(crate) fn heap_bytes(&self) -> usize {
        let allocation = std::mem::size_of::<Self>()
            .saturating_add(2usize.saturating_mul(std::mem::size_of::<usize>()));
        allocation.saturating_add(match &self.cells {
            PackedCells::Direct(cells) => {
                cells.capacity().saturating_mul(std::mem::size_of::<T>())
            },
            PackedCells::Indexed8 { values, indices } => values
                .capacity()
                .saturating_mul(std::mem::size_of::<T>())
                .saturating_add(indices.capacity()),
            PackedCells::Indexed16 { values, indices } => values
                .capacity()
                .saturating_mul(std::mem::size_of::<T>())
                .saturating_add(
                    indices
                        .capacity()
                        .saturating_mul(std::mem::size_of::<u16>()),
                ),
        })
    }

    #[inline]
    pub(crate) fn value_capacity(&self) -> usize {
        match &self.cells {
            PackedCells::Direct(cells) => cells.capacity(),
            PackedCells::Indexed8 { values, .. } | PackedCells::Indexed16 { values, .. } => {
                values.capacity()
            },
        }
    }

    #[inline]
    pub(crate) fn allocation_count(&self) -> usize {
        match self.cells {
            PackedCells::Direct(_) => 2,
            PackedCells::Indexed8 { .. } | PackedCells::Indexed16 { .. } => 3,
        }
    }
}

#[derive(Clone, Debug)]
enum RowStorage<T> {
    Dense(Vec<T>),
    Uniform(Arc<T>),
    Packed {
        block: Arc<PackedBlock<T>>,
        start: u32,
        len: u32,
    },
}

impl<T> Default for RowStorage<T> {
    fn default() -> Self {
        Self::Dense(Vec::new())
    }
}

/// A row in the grid.
#[derive(Default, Clone, Debug)]
pub struct Row<T> {
    /// Dense cells, or a compact prefix followed by one repeated suffix cell.
    ///
    /// Scrollback rows frequently end in a long run of identical blank cells.
    /// Keeping one copy of that suffix preserves exact cell contents while
    /// avoiding a full-width allocation for cold history. Active writes thaw
    /// the row before returning mutable access.
    inner: RowStorage<T>,

    /// Maximum number of occupied entries.
    ///
    /// This is the upper bound on the number of elements in the row, which have been modified
    /// since the last reset. All cells after this point are guaranteed to be equal.
    pub(crate) occ: u32,

    /// Logical column count. This differs from `inner.len()` while compacted.
    columns: u32,
}

// Keep Alacritty's established `{ inner, occ }` wire shape. The compact form
// is an in-memory optimization and must not invalidate existing ref fixtures
// or leak into persisted representations.
#[cfg(feature = "serde")]
impl<T: Serialize> Serialize for Row<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        struct LogicalCells<'a, T>(&'a Row<T>);

        impl<T: Serialize> Serialize for LogicalCells<'_, T> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
                for cell in self.0 {
                    sequence.serialize_element(cell)?;
                }
                sequence.end()
            }
        }

        let mut row = serializer.serialize_struct("Row", 2)?;
        row.serialize_field("inner", &LogicalCells(self))?;
        row.serialize_field("occ", &usize::try_from(self.occ).unwrap_or(usize::MAX))?;
        row.end()
    }
}

#[cfg(feature = "serde")]
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Row<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SerializedRow<T> {
            inner: Vec<T>,
            occ: usize,
        }

        let row = SerializedRow::deserialize(deserializer)?;
        Ok(Self::from_vec(row.inner, row.occ))
    }
}

impl<T: PartialEq> PartialEq for Row<T> {
    fn eq(&self, other: &Self) -> bool {
        self.columns == other.columns && self.into_iter().eq(other)
    }
}

impl<T: Default> Row<T> {
    /// Create a new terminal row.
    ///
    /// Ideally the `template` should be `Copy` in all performance sensitive scenarios.
    pub fn new(columns: usize) -> Row<T> {
        debug_assert!(columns >= 1);
        let mut inner = Vec::with_capacity(columns.min(COMPACT_ROW_REUSE_CELLS));
        inner.push(T::default());

        Row {
            inner: RowStorage::Dense(inner),
            occ: 0,
            columns: u32::try_from(columns).expect("terminal row width exceeds u32"),
        }
    }

    /// Create a logically blank row sharing one immutable template allocation.
    #[inline]
    pub(crate) fn new_uniform(columns: usize, fill: Arc<T>) -> Row<T> {
        debug_assert!(columns >= 1);
        Row {
            inner: RowStorage::Uniform(fill),
            occ: 0,
            columns: u32::try_from(columns).expect("terminal row width exceeds u32"),
        }
    }

    /// Increase the number of columns in the row.
    #[inline]
    pub fn grow(&mut self, columns: usize)
    where
        T: Clone,
    {
        if self.len() >= columns {
            return;
        }

        self.inflate();
        self.dense_mut().resize_with(columns, T::default);
        self.columns = u32::try_from(columns).expect("terminal row width exceeds u32");
    }

    /// Reduce the number of columns in the row.
    ///
    /// This will return all non-empty cells that were removed.
    pub fn shrink(&mut self, columns: usize) -> Option<Vec<T>>
    where
        T: Clone + GridCell,
    {
        if self.len() <= columns {
            return None;
        }

        self.inflate();

        // Split off cells for a new row.
        let mut new_row = self.dense_mut().split_off(columns);
        let index = new_row.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1);
        new_row.truncate(index);

        self.occ = min(self.occ as usize, columns) as u32;
        self.columns = u32::try_from(columns).expect("terminal row width exceeds u32");

        if new_row.is_empty() { None } else { Some(new_row) }
    }

    /// Reset all cells in the row to the `template` cell.
    #[inline]
    pub fn reset<D>(&mut self, template: &T)
    where
        T: Clone + ResetDiscriminant<D> + GridCell,
        D: PartialEq,
    {
        self.reset_reusing(template, &mut Vec::new());
    }

    pub(crate) fn reset_reusing<D>(&mut self, template: &T, reusable: &mut Vec<Vec<T>>)
    where
        T: Clone + ResetDiscriminant<D> + GridCell,
        D: PartialEq,
    {
        debug_assert!(self.columns != 0);
        let occupied = self.occ as usize;
        let mut fill = self.last().expect("terminal row must contain a cell").clone();
        fill.reset(template);
        let reusable_prefix = min(
            self.len(),
            (self.occ as usize + 1).min(COMPACT_ROW_REUSE_CELLS),
        )
        .max(1);

        if let RowStorage::Dense(dense) = &mut self.inner {
            let reset_all = dense
                .last()
                .is_some_and(|suffix| suffix.discriminant() != template.discriminant());
            dense.truncate(reusable_prefix);
            let reset_len = if reset_all {
                dense.len()
            } else {
                occupied.min(dense.len())
            };
            for cell in &mut dense[..reset_len] {
                cell.reset(template);
            }
            if dense.len() < reusable_prefix {
                dense.resize(reusable_prefix, fill);
            }
            self.occ = 0;
            return;
        }

        // A reset row is logically uniform. Preserve an existing allocation
        // for reuse, but do not inflate packed history back to a full-width
        // vector merely to clear it. Ordinary cell writes grow this compact
        // prefix only as far as the column being changed.
        let mut dense = match std::mem::take(&mut self.inner) {
            RowStorage::Dense(dense) => dense,
            RowStorage::Uniform(_) | RowStorage::Packed { .. } => reusable
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(self.len().min(COMPACT_ROW_REUSE_CELLS))),
        };
        dense.clear();
        dense.resize(reusable_prefix, fill);
        self.inner = RowStorage::Dense(dense);
        self.occ = 0;
    }
}

#[allow(clippy::len_without_is_empty)]
impl<T> Row<T> {
    #[inline]
    pub fn from_vec(vec: Vec<T>, occ: usize) -> Row<T> {
        let columns = vec.len();
        Row {
            inner: RowStorage::Dense(vec),
            occ: u32::try_from(occ.min(columns)).expect("terminal row occupancy exceeds u32"),
            columns: u32::try_from(columns).expect("terminal row width exceeds u32"),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.columns as usize
    }

    /// Whether this row uses a single retained cell for its repeated suffix.
    #[inline]
    pub(crate) fn is_compacted(&self) -> bool {
        self.is_packed() || self.physical_len() < self.len()
    }

    /// Heap allocation owned by this row's cell vector. Dynamically allocated
    /// data inside individual cells is intentionally not included.
    #[inline]
    pub(crate) fn heap_storage_bytes(&self) -> usize {
        match &self.inner {
            RowStorage::Dense(inner) => {
                inner.capacity().saturating_mul(std::mem::size_of::<T>())
            },
            RowStorage::Uniform(_) | RowStorage::Packed { .. } => 0,
        }
    }

    #[inline]
    pub fn last(&self) -> Option<&T> {
        let len = self.physical_len();
        (len != 0).then(|| self.physical_cell(len - 1))
    }

    /// Return the physical cell capacity retained by this row.
    #[inline]
    pub(crate) fn allocated_cells(&self) -> usize {
        match &self.inner {
            RowStorage::Dense(inner) => inner.capacity(),
            RowStorage::Uniform(_) | RowStorage::Packed { .. } => 0,
        }
    }

    #[inline]
    pub(crate) fn is_packed(&self) -> bool {
        matches!(self.inner, RowStorage::Packed { .. })
    }

    #[inline]
    pub(crate) fn is_uniform(&self) -> bool {
        matches!(self.inner, RowStorage::Uniform(_))
    }

    #[inline]
    pub(crate) fn physical_len(&self) -> usize {
        match &self.inner {
            RowStorage::Dense(inner) => inner.len(),
            RowStorage::Uniform(_) => 1,
            RowStorage::Packed { len, .. } => *len as usize,
        }
    }

    #[inline]
    pub(crate) fn physical_cell(&self, index: usize) -> &T {
        match &self.inner {
            RowStorage::Dense(inner) => &inner[index],
            RowStorage::Uniform(fill) => {
                debug_assert_eq!(index, 0);
                fill
            },
            RowStorage::Packed { block, start, len } => {
                debug_assert!(index < *len as usize);
                block.cell(*start as usize + index)
            },
        }
    }

    #[inline]
    pub(crate) fn packed_block(&self) -> Option<&Arc<PackedBlock<T>>> {
        match &self.inner {
            RowStorage::Packed { block, .. } => Some(block),
            RowStorage::Dense(_) | RowStorage::Uniform(_) => None,
        }
    }

    #[inline]
    pub(crate) fn uniform_fill(&self) -> Option<&Arc<T>> {
        match &self.inner {
            RowStorage::Uniform(fill) => Some(fill),
            RowStorage::Dense(_) | RowStorage::Packed { .. } => None,
        }
    }

    #[inline]
    pub(crate) fn install_packed(
        &mut self,
        block: Arc<PackedBlock<T>>,
        start: usize,
        len: usize,
    ) -> Option<Vec<T>> {
        debug_assert!(!self.is_packed());
        let previous = std::mem::replace(&mut self.inner, RowStorage::Packed {
            block,
            start: u32::try_from(start).expect("packed block offset exceeds u32"),
            len: u32::try_from(len).expect("packed row length exceeds u32"),
        });
        match previous {
            RowStorage::Dense(mut dense) if dense.capacity() <= COMPACT_ROW_REUSE_CELLS => {
                dense.clear();
                Some(dense)
            },
            RowStorage::Dense(_) | RowStorage::Uniform(_) | RowStorage::Packed { .. } => None,
        }
    }

    pub(crate) fn new_direct_block(cells: Vec<T>) -> Arc<PackedBlock<T>> {
        Arc::new(PackedBlock { cells: PackedCells::Direct(cells) })
    }

    pub(crate) fn new_indexed8_block(
        values: Vec<T>,
        indices: Vec<u8>,
    ) -> Arc<PackedBlock<T>> {
        Arc::new(PackedBlock { cells: PackedCells::Indexed8 { values, indices } })
    }

    pub(crate) fn new_indexed16_block(
        values: Vec<T>,
        indices: Vec<u16>,
    ) -> Arc<PackedBlock<T>> {
        Arc::new(PackedBlock { cells: PackedCells::Indexed16 { values, indices } })
    }

    #[inline]
    fn dense(&self) -> &[T] {
        match &self.inner {
            RowStorage::Dense(inner) => inner,
            RowStorage::Uniform(_) | RowStorage::Packed { .. } => {
                panic!("compact row requires logical indexing")
            },
        }
    }

    #[inline]
    fn dense_mut(&mut self) -> &mut Vec<T> {
        match &mut self.inner {
            RowStorage::Dense(inner) => inner,
            RowStorage::Uniform(_) | RowStorage::Packed { .. } => {
                unreachable!("compact row must be inflated before mutation")
            },
        }
    }
}

impl<T: Clone> Row<T> {
    /// Materialize only enough of a compact row for one mutable cell.
    #[inline]
    fn inflate_to(&mut self, index: usize) {
        let columns = self.len();
        debug_assert!(index < columns);
        let target_len = (index + 2).min(columns);
        let dense = match &mut self.inner {
            RowStorage::Dense(inner) if inner.len() < target_len => {
                let fill = inner.last().expect("compacted row must have a suffix cell").clone();
                inner.resize(target_len, fill);
                return;
            },
            RowStorage::Dense(_) => return,
            RowStorage::Uniform(fill) => {
                let capacity = columns.min(COMPACT_ROW_REUSE_CELLS).max(target_len);
                let mut dense = Vec::with_capacity(capacity);
                dense.resize(target_len, fill.as_ref().clone());
                dense
            },
            RowStorage::Packed { block, start, len } => {
                let start = *start as usize;
                let suffix = (*len as usize).saturating_sub(1);
                let capacity = columns.min(COMPACT_ROW_REUSE_CELLS).max(target_len);
                let mut dense = Vec::with_capacity(capacity);
                for offset in 0..target_len {
                    dense.push(block.cell(start + offset.min(suffix)).clone());
                }
                dense
            },
        };
        self.inner = RowStorage::Dense(dense);
    }

    /// Restore a compact scrollback row to ordinary dense Alacritty storage.
    #[inline]
    fn inflate(&mut self) {
        let columns = self.len();
        let dense = match &mut self.inner {
            RowStorage::Dense(inner) if inner.len() < columns => {
                let fill = inner.pop().expect("compacted row must have a suffix cell");
                inner.resize(columns, fill);
                return;
            },
            RowStorage::Dense(_) => return,
            RowStorage::Uniform(fill) => vec![fill.as_ref().clone(); columns],
            RowStorage::Packed { block, start, len } => {
                let start = *start as usize;
                let suffix = (*len as usize).saturating_sub(1);
                let mut dense = Vec::with_capacity(columns);
                for index in 0..columns {
                    dense.push(block.cell(start + index.min(suffix)).clone());
                }
                dense
            },
        };
        self.inner = RowStorage::Dense(dense);
    }

    /// Return a mutable contiguous cell range while retaining a compact suffix.
    ///
    /// Terminal text runs can use this to thaw a row once per run instead of
    /// repeating the compact-storage dispatch for every character.
    #[inline]
    pub(crate) fn cells_mut(&mut self, range: Range<usize>) -> &mut [T] {
        debug_assert!(range.start < range.end);
        debug_assert!(range.end <= self.len());
        self.inflate_to(range.end - 1);
        self.occ = max(self.occ, range.end as u32);
        &mut self.dense_mut()[range]
    }

    /// Collapse an identical trailing run into a single cell.
    ///
    /// This is deliberately lossless: unlike text-only scrollback, the stored
    /// suffix retains its original colors, flags, hyperlink, and grapheme data.
    /// Returns whether the physical representation became smaller.
    pub(crate) fn compact_trailing(&mut self) -> bool
    where
        T: PartialEq,
    {
        if self.is_packed() || self.is_uniform() || self.columns <= 1 {
            return false;
        }

        if self.physical_len() < self.len() {
            return false;
        }

        let columns = self.len();
        // `occ` is the upper bound of cells which may differ from the row's
        // repeated suffix. Avoid comparing the untouched remainder of every
        // completed line; wide terminals otherwise turn line archival into a
        // full-width scan even when only a short prompt or counter was drawn.
        let occupied = min(self.occ as usize, columns - 1);
        let inner = self.dense_mut();
        debug_assert_eq!(inner.len(), columns);
        let fill = inner[columns - 1].clone();
        let prefix_len = inner[..occupied]
            .iter()
            .rposition(|cell| cell != &fill)
            .map_or(0, |index| index + 1);

        // A one-cell suffix cannot save storage.
        if prefix_len + 1 >= columns {
            return false;
        }

        inner.truncate(prefix_len);
        inner.push(fill);
        // Keep the small active-row allocation until block packing takes
        // ownership of this cold row. Shrinking every completed line would
        // add an allocator round trip to the terminal parser's hottest path.
        true
    }

    #[inline]
    pub fn last_mut(&mut self) -> Option<&mut T> {
        self.inflate();
        self.occ = self.columns;
        self.dense_mut().last_mut()
    }

    #[inline]
    pub fn append(&mut self, vec: &mut Vec<T>)
    where
        T: GridCell,
    {
        self.inflate();
        self.occ = self.occ.saturating_add(vec.len() as u32);
        self.dense_mut().append(vec);
        self.columns = self.physical_len() as u32;
    }

    #[inline]
    pub fn append_front(&mut self, mut vec: Vec<T>) {
        self.inflate();
        self.occ = self.occ.saturating_add(vec.len() as u32);

        vec.append(self.dense_mut());
        self.columns = vec.len() as u32;
        self.inner = RowStorage::Dense(vec);
    }

    /// Check if all cells in the row are empty.
    #[inline]
    pub fn is_clear(&self) -> bool
    where
        T: GridCell,
    {
        self.into_iter().all(GridCell::is_empty)
    }

    #[inline]
    pub fn front_split_off(&mut self, at: usize) -> Vec<T> {
        self.inflate();
        self.occ = self.occ.saturating_sub(at as u32);

        let mut split = self.dense_mut().split_off(at);
        std::mem::swap(&mut split, self.dense_mut());
        self.columns = self.physical_len() as u32;
        split
    }
}

/// Logical row iterator which transparently repeats a compact suffix cell.
pub struct RowIter<'a, T> {
    row: &'a Row<T>,
    front: usize,
    back: usize,
}

impl<'a, T> Iterator for RowIter<'a, T> {
    type Item = &'a T;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let index = self.front;
        self.front += 1;
        Some(&self.row[Column(index)])
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.back - self.front;
        (len, Some(len))
    }
}

impl<T> DoubleEndedIterator for RowIter<'_, T> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        Some(&self.row[Column(self.back)])
    }
}

impl<T> ExactSizeIterator for RowIter<'_, T> {}

impl<'a, T> IntoIterator for &'a Row<T> {
    type IntoIter = RowIter<'a, T>;
    type Item = &'a T;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        RowIter { row: self, front: 0, back: self.len() }
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut Row<T> {
    type IntoIter = slice::IterMut<'a, T>;
    type Item = &'a mut T;

    #[inline]
    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.inflate();
        self.occ = self.columns;
        self.dense_mut().iter_mut()
    }
}

impl<T> Index<Column> for Row<T> {
    type Output = T;

    #[inline]
    fn index(&self, index: Column) -> &T {
        debug_assert!(index.0 < self.len());
        let prefix_len = self.physical_len() - 1;
        self.physical_cell(index.0.min(prefix_len))
    }
}

impl<T: Clone> IndexMut<Column> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Column) -> &mut T {
        self.inflate_to(index.0);
        self.occ = max(self.occ, (*index + 1) as u32);
        &mut self.dense_mut()[index.0]
    }
}

impl<T> Index<Range<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: Range<Column>) -> &[T] {
        assert!(!self.is_compacted(), "range indexing requires a dense row");
        &self.dense()[(index.start.0)..(index.end.0)]
    }
}

impl<T: Clone> IndexMut<Range<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Range<Column>) -> &mut [T] {
        self.inflate();
        self.occ = max(self.occ, *index.end as u32);
        &mut self.dense_mut()[(index.start.0)..(index.end.0)]
    }
}

impl<T> Index<RangeTo<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeTo<Column>) -> &[T] {
        assert!(!self.is_compacted(), "range indexing requires a dense row");
        &self.dense()[..(index.end.0)]
    }
}

impl<T: Clone> IndexMut<RangeTo<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeTo<Column>) -> &mut [T] {
        self.inflate();
        self.occ = max(self.occ, *index.end as u32);
        &mut self.dense_mut()[..(index.end.0)]
    }
}

impl<T> Index<RangeFrom<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeFrom<Column>) -> &[T] {
        assert!(!self.is_compacted(), "range indexing requires a dense row");
        &self.dense()[(index.start.0)..]
    }
}

impl<T: Clone> IndexMut<RangeFrom<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeFrom<Column>) -> &mut [T] {
        self.inflate();
        self.occ = self.columns;
        &mut self.dense_mut()[(index.start.0)..]
    }
}

impl<T> Index<RangeFull> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, _: RangeFull) -> &[T] {
        assert!(!self.is_compacted(), "range indexing requires a dense row");
        self.dense()
    }
}

impl<T: Clone> IndexMut<RangeFull> for Row<T> {
    #[inline]
    fn index_mut(&mut self, _: RangeFull) -> &mut [T] {
        self.inflate();
        self.occ = self.columns;
        self.dense_mut()
    }
}

impl<T> Index<RangeToInclusive<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeToInclusive<Column>) -> &[T] {
        assert!(!self.is_compacted(), "range indexing requires a dense row");
        &self.dense()[..=(index.end.0)]
    }
}

impl<T: Clone> IndexMut<RangeToInclusive<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeToInclusive<Column>) -> &mut [T] {
        self.inflate();
        self.occ = max(self.occ, (*index.end + 1) as u32);
        &mut self.dense_mut()[..=(index.end.0)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::cell::Cell;

    #[test]
    fn row_descriptor_stays_compact() {
        assert!(std::mem::size_of::<Row<char>>() <= 32);
    }

    #[test]
    fn compact_suffix_is_transparent_and_mutation_inflates() {
        let mut row = Row::<char>::new(8);
        row[Column(0)] = 'a';
        row[Column(1)] = 'b';

        row.compact_trailing();
        assert!(row.is_compacted());
        assert_eq!(row.len(), 8);
        assert_eq!(row.dense(), &['a', 'b', '\0']);
        assert_eq!(row[Column(0)], 'a');
        assert_eq!(row[Column(7)], '\0');
        assert_eq!(row.into_iter().copied().collect::<Vec<_>>(), vec!['a', 'b', '\0', '\0', '\0', '\0', '\0', '\0']);

        row[Column(7)] = 'z';
        assert!(!row.is_compacted());
        assert_eq!(row.dense().len(), 8);
        assert_eq!(row[Column(0)], 'a');
        assert_eq!(row[Column(7)], 'z');
    }

    #[test]
    fn compact_suffix_preserves_non_default_fill() {
        let mut row = Row::<char>::new(6);
        for cell in &mut row {
            *cell = '-';
        }
        row[Column(0)] = 'x';

        assert!(row.compact_trailing());
        assert_eq!(row.dense(), &['x', '-']);
        assert_eq!(row.into_iter().copied().collect::<String>(), "x-----");
    }

    #[test]
    fn resetting_packed_history_materializes_only_the_written_prefix() {
        let mut row = Row::<Cell>::new(80);
        row[Column(0)].c = 'x';
        row.compact_trailing();
        let block = Row::new_direct_block(
            (0..row.physical_len())
                .map(|index| row.physical_cell(index).clone())
                .collect(),
        );
        let _ = row.install_packed(block, 0, row.physical_len());

        row.reset(&Cell::default());
        assert_eq!(row.physical_len(), 2);
        row[Column(5)].c = 'z';

        assert_eq!(row.physical_len(), 7);
        assert_eq!(row[Column(5)].c, 'z');
        assert_eq!(row[Column(79)], Cell::default());
    }

    #[test]
    fn reset_reuses_the_previous_written_prefix() {
        let mut row = Row::<Cell>::new(80);
        for column in 0..8 {
            row[Column(column)].c = 'x';
        }

        row.reset(&Cell::default());

        assert_eq!(row.physical_len(), 9);
        assert!(row.allocated_cells() >= COMPACT_ROW_REUSE_CELLS);
        for column in 0..8 {
            row[Column(column)].c = 'y';
        }
        assert_eq!(row.physical_len(), 9);
        assert_eq!(row[Column(79)], Cell::default());
    }

    #[test]
    fn first_uniform_write_reserves_the_reusable_prefix() {
        let fill = Arc::new(Cell::default());
        let mut row = Row::new_uniform(80, fill);

        row[Column(5)].c = 'z';

        assert_eq!(row.physical_len(), 7);
        assert!(row.allocated_cells() >= COMPACT_ROW_REUSE_CELLS);
        assert_eq!(row[Column(5)].c, 'z');
        assert_eq!(row[Column(79)], Cell::default());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn packed_row_keeps_the_dense_wire_format() {
        let mut row = Row::<char>::new(8);
        row[Column(0)] = 'a';
        row.compact_trailing();
        let block = Row::new_indexed8_block(vec!['a', '\0'], vec![0, 1]);
        let _ = row.install_packed(block, 0, 2);

        let serialized = serde_json::to_string(&row).expect("serialize packed row");
        let restored: Row<char> = serde_json::from_str(&serialized).expect("restore packed row");

        assert_eq!(restored, row);
        assert!(!restored.is_packed());
        assert_eq!(restored.len(), 8);
        assert_eq!(restored[Column(0)], 'a');
        assert_eq!(restored[Column(7)], '\0');
    }
}
