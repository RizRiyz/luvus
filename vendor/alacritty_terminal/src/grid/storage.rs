use std::cmp::max;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::mem;
use std::ops::{Deref, Index, IndexMut};
use std::sync::Arc;

#[cfg(feature = "serde")]
use serde::de::Deserializer;
#[cfg(feature = "serde")]
use serde::ser::{SerializeSeq, SerializeStruct, Serializer};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use super::Row;
use crate::index::Line;

/// Maximum shallow allocation retained for rows outside the active grid.
///
/// Upstream caches a fixed 1,000 rows. Since every cached row owns a full cell
/// vector, that makes the spare allocation grow linearly with terminal width.
/// A byte target keeps the reuse benefit without retaining multiple megabytes
/// per wide terminal.
const MAX_CACHE_BYTES: usize = 64 * 1024;

/// Spare outer row descriptors do not carry reusable cell storage, so they
/// have their own smaller budget. Keeping this separate prevents geometric
/// `Vec` growth from retaining almost another complete history after packing.
const MAX_DESCRIPTOR_CACHE_BYTES: usize = 8 * 1024;

/// Keep a few rows ready so ordinary short bursts do not allocate line by line.
const MIN_CACHE_ROWS: usize = 8;
const MAX_REUSABLE_ROW_BUFFERS: usize = 32;
const MAX_REUSABLE_ROW_BYTES: usize = 32 * 1024;

/// Recent history remains as ordinary mutable rows. Older completed rows can
/// share dictionary-compressed blocks without affecting the active parser.
const HOT_HISTORY_ROWS: usize = 32;

/// Bound packing work and block fan-out. This also bounds the memory retained
/// when only part of the oldest block remains inside the history limit.
const PACKED_BLOCK_MAX_ROWS: usize = 512;
const PACKED_BLOCK_TARGET_BYTES: usize = 128 * 1024;
const MIN_PACKED_BLOCK_ROWS: usize = 32;
const LINEAR_DICTIONARY_CELLS: usize = 16;

/// Fast bounded fingerprinting for transient block construction.
///
/// This is never used as an authority boundary: a collision rejects the
/// dictionary and falls back to a lossless direct block. The input is capped
/// by `PACKED_BLOCK_TARGET_BYTES`, so it cannot create unbounded work.
struct FingerprintHasher(u64);

impl Default for FingerprintHasher {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for FingerprintHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// The cell fingerprint is already a complete hash, so hashing that `u64`
/// again inside `HashMap` only burns CPU on completed terminal history.
#[derive(Default)]
struct FingerprintMapHasher(u64);

impl Hasher for FingerprintMapHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut fingerprint = FingerprintHasher::default();
        fingerprint.write(bytes);
        self.0 = fingerprint.finish();
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

type FingerprintMap = HashMap<u64, u16, BuildHasherDefault<FingerprintMapHasher>>;

fn fingerprint<T: Hash>(value: &T) -> u64 {
    let mut hasher = FingerprintHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

fn fingerprint_map<T: Eq + Hash>(values: &[T]) -> Option<FingerprintMap> {
    let mut dictionary = FingerprintMap::with_capacity_and_hasher(
        values.len().saturating_mul(2),
        BuildHasherDefault::default(),
    );
    for (index, value) in values.iter().enumerate() {
        let index = u16::try_from(index).ok()?;
        let key = fingerprint(value);
        if let Some(previous) = dictionary.insert(key, index) {
            if values[previous as usize] != *value {
                return None;
            }
        }
    }
    Some(dictionary)
}

#[inline]
fn reserve_dictionary_value<T>(values: &mut Vec<T>, total_cells: usize) {
    if values.len() != values.capacity() {
        return;
    }
    let target = if values.capacity() == 0 {
        LINEAR_DICTIONARY_CELLS.min(total_cells)
    } else {
        values.capacity().saturating_mul(2).min(total_cells)
    };
    values.reserve_exact(target.saturating_sub(values.len()));
}

/// Convert the encoded prefix when a page cannot benefit from indexing.
///
/// High-entropy input normally assigns every value its own consecutive index;
/// that fast path moves the value allocation directly into the block. Hash
/// collisions and mixed repetition decode only the bounded prefix accumulated
/// before the builder switched to direct cells.
fn indexed_cells_into_direct<T: Clone>(
    mut values: Vec<T>,
    narrow_indices: Vec<u8>,
    wide_indices: Option<Vec<u16>>,
    total_cells: usize,
) -> Vec<T> {
    let indices_are_unique_order = wide_indices.as_ref().map_or_else(
        || {
            narrow_indices.len() == values.len()
                && narrow_indices
                    .iter()
                    .enumerate()
                    .all(|(index, value)| index == usize::from(*value))
        },
        |indices| {
            indices.len() == values.len()
                && indices
                    .iter()
                    .enumerate()
                    .all(|(index, value)| index == usize::from(*value))
        },
    );
    if indices_are_unique_order {
        values.reserve_exact(total_cells.saturating_sub(values.len()));
        return values;
    }

    let mut cells = Vec::with_capacity(total_cells);
    match wide_indices {
        Some(indices) => cells.extend(indices.into_iter().map(|index| values[index as usize].clone())),
        None => cells.extend(
            narrow_indices
                .into_iter()
                .map(|index| values[index as usize].clone()),
        ),
    }
    cells
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StorageMetrics {
    pub(crate) estimated_bytes: usize,
    pub(crate) cache_bytes: usize,
    pub(crate) row_descriptor_bytes: usize,
    pub(crate) dense_cell_bytes: usize,
    pub(crate) packed_block_bytes: usize,
    pub(crate) packed_blocks: usize,
    pub(crate) dense_rows: usize,
    pub(crate) packed_rows: usize,
    pub(crate) allocated_cells: usize,
    pub(crate) allocations: usize,
}

#[derive(Clone, Copy, Debug)]
struct ColdRow {
    end: u32,
    occ: u32,
}

#[derive(Clone, Debug)]
struct ColdPage<T> {
    block: Arc<super::row::PackedBlock<T>>,
    rows: Vec<ColdRow>,
    /// Exclusive cold-row offset of this page's oldest row.
    ///
    /// Pages are ordered newest to oldest, so these cumulative endpoints are
    /// sorted and support logarithmic point lookup during search and copy.
    end_row: usize,
}

impl<T> ColdPage<T> {
    #[inline]
    fn row_bounds(&self, index: usize) -> (u32, u32) {
        let start = index.checked_sub(1).map_or(0, |previous| self.rows[previous].end);
        (start, self.rows[index].end)
    }
}

/// Borrowed or page-backed immutable row view.
///
/// Cold rows construct only their small descriptor; their cell storage stays
/// owned by the shared page.
pub enum RowRef<'a, T> {
    Dense(&'a Row<T>),
    Packed(Row<T>),
}

impl<T> Deref for RowRef<'_, T> {
    type Target = Row<T>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Dense(row) => row,
            Self::Packed(row) => row,
        }
    }
}

fn cache_row_limit<T>(columns: usize) -> usize {
    let row_bytes = mem::size_of::<Row<T>>()
        .saturating_add(columns.max(1).saturating_mul(mem::size_of::<T>()))
        .max(1);
    MAX_CACHE_BYTES.saturating_div(row_bytes).max(MIN_CACHE_ROWS)
}

/// A ring buffer for optimizing indexing and rotation.
///
/// The [`Storage::rotate`] and [`Storage::rotate_down`] functions are fast modular additions on
/// the internal [`zero`] field. As compared with [`slice::rotate_left`] which must rearrange items
/// in memory.
///
/// As a consequence, both [`Index`] and [`IndexMut`] are reimplemented for this type to account
/// for the zeroth element not always being at the start of the allocation.
///
/// Because certain [`Vec`] operations are no longer valid on this type, no [`Deref`]
/// implementation is provided. Anything from [`Vec`] that should be exposed must be done so
/// manually.
///
/// [`slice::rotate_left`]: https://doc.rust-lang.org/std/primitive.slice.html#method.rotate_left
/// [`Deref`]: std::ops::Deref
/// [`zero`]: #structfield.zero
#[derive(Clone, Debug)]
pub struct Storage<T> {
    inner: Vec<Row<T>>,

    /// Page-owned immutable history, ordered from newest page to oldest page.
    /// Active and recent rows stay in `inner`; a cold row has no standalone
    /// `Row<T>` or repeated `Arc` descriptor.
    cold_pages: VecDeque<ColdPage<T>>,

    /// Total rows owned by `cold_pages`.
    ///
    /// Keep this cached because cell indexing is a hot path and must not scan
    /// every retained page just to resolve the active/cold boundary.
    cold_rows: usize,

    /// Small cell vectors recycled between packed history and the live row.
    /// This is bounded independently from scrollback and never serialized.
    reusable_rows: Vec<Vec<T>>,

    /// Starting point for the storage of rows.
    ///
    /// This value represents the starting line offset within the ring buffer. The value of this
    /// offset may be larger than the `len` itself, and will wrap around to the start to form the
    /// ring buffer. It represents the bottommost line of the terminal.
    zero: usize,

    /// Number of visible lines.
    visible_lines: usize,

    /// Total number of lines currently active in the terminal (scrollback + visible)
    ///
    /// Shrinking this length allows reducing the number of lines in the scrollback buffer without
    /// having to truncate the raw `inner` buffer.
    /// As long as `len` is bigger than `inner`, it is also possible to grow the scrollback buffer
    /// without any additional insertions.
    len: usize,
}

#[cfg(feature = "serde")]
impl<T: Serialize> Serialize for Storage<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        struct LogicalRows<'a, T>(&'a Storage<T>);

        impl<T: Serialize> Serialize for LogicalRows<'_, T> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut rows = serializer.serialize_seq(Some(self.0.len))?;
                for logical in 0..self.0.len {
                    let line = Line(self.0.visible_lines as i32 - logical as i32 - 1);
                    rows.serialize_element(self.0.row(line).deref())?;
                }
                rows.end()
            }
        }

        let mut storage = serializer.serialize_struct("Storage", 4)?;
        storage.serialize_field("inner", &LogicalRows(self))?;
        storage.serialize_field("zero", &0usize)?;
        storage.serialize_field("visible_lines", &self.visible_lines)?;
        storage.serialize_field("len", &self.len)?;
        storage.end()
    }
}

#[cfg(feature = "serde")]
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Storage<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StorageWire<T> {
            inner: Vec<Row<T>>,
            zero: usize,
            visible_lines: usize,
            len: usize,
        }

        let wire = StorageWire::deserialize(deserializer)?;
        Ok(Self {
            inner: wire.inner,
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: wire.zero,
            visible_lines: wire.visible_lines,
            len: wire.len,
        })
    }
}

impl<T: PartialEq> PartialEq for Storage<T> {
    fn eq(&self, other: &Self) -> bool {
        // Both storage buffers need to be truncated and zeroed.
        assert_eq!(self.zero, 0);
        assert_eq!(other.zero, 0);

        self.len == other.len
            && self.visible_lines == other.visible_lines
            && (0..self.len).all(|logical| {
                let line = Line(self.visible_lines as i32 - logical as i32 - 1);
                self.row(line).deref() == other.row(line).deref()
            })
    }
}

impl<T> Storage<T> {
    #[inline]
    fn active_len(&self) -> usize {
        self.len.saturating_sub(self.cold_rows)
    }

    fn cold_row(&self, offset: usize) -> (&ColdPage<T>, usize) {
        let page_index = self
            .cold_pages
            .partition_point(|page| page.end_row <= offset);
        let page = &self.cold_pages[page_index];
        let start = page_index
            .checked_sub(1)
            .map_or(0, |previous| self.cold_pages[previous].end_row);
        (page, offset - start)
    }

    pub(crate) fn row(&self, requested: Line) -> RowRef<'_, T> {
        let logical = self.logical_index(requested);
        let active_len = self.active_len();
        if logical < active_len {
            let index = self.compute_active_index(logical);
            return RowRef::Dense(&self.inner[index]);
        }

        let columns = u32::try_from(self.columns()).expect("terminal row width exceeds u32");
        let (page, row_index) = self.cold_row(logical - active_len);
        let (start, end) = page.row_bounds(row_index);
        RowRef::Packed(Row::from_packed(
            page.block.clone(),
            start,
            end - start,
            columns,
            page.rows[row_index].occ,
        ))
    }

    pub(crate) fn row_mut(&mut self, requested: Line) -> &mut Row<T> {
        if self.logical_index(requested) >= self.active_len() {
            self.materialize_cold();
        }
        let index = self.compute_index(requested);
        &mut self.inner[index]
    }

    #[inline]
    pub(crate) fn cell(&self, line: Line, column: crate::index::Column) -> &T {
        let logical = self.logical_index(line);
        let active_len = self.active_len();
        if logical < active_len {
            return &self.inner[self.compute_active_index(logical)][column];
        }

        let (page, row_index) = self.cold_row(logical - active_len);
        let (start, end) = page.row_bounds(row_index);
        let physical_len = (end - start) as usize;
        debug_assert!(physical_len != 0);
        page.block
            .cell(start as usize + column.0.min(physical_len - 1))
    }

    fn materialize_cold(&mut self) {
        if self.cold_pages.is_empty() {
            return;
        }
        self.rezero();
        self.inner.truncate(self.active_len());
        let columns = u32::try_from(self.columns()).expect("terminal row width exceeds u32");
        for page in self.cold_pages.drain(..) {
            for (index, meta) in page.rows.iter().enumerate() {
                let (start, end) = page.row_bounds(index);
                self.inner.push(Row::from_packed(
                    page.block.clone(),
                    start,
                    end - start,
                    columns,
                    meta.occ,
                ));
            }
        }
        self.cold_rows = 0;
        debug_assert_eq!(self.inner.len(), self.len);
    }

    fn evict_oldest(&mut self) {
        if let Some(page) = self.cold_pages.back_mut() {
            page.rows.pop();
            page.end_row -= 1;
            self.cold_rows -= 1;
            if page.rows.is_empty() {
                self.cold_pages.pop_back();
            }
        } else {
            self.rezero();
            self.inner.truncate(self.active_len());
            self.inner.pop();
        }
        self.len = self.len.saturating_sub(1);
    }

    fn shrink_oldest(&mut self, mut shrinkage: usize) {
        while shrinkage != 0 {
            let Some(page) = self.cold_pages.back_mut() else {
                self.len -= shrinkage;
                return;
            };
            let removed = shrinkage.min(page.rows.len());
            page.rows.truncate(page.rows.len() - removed);
            page.end_row -= removed;
            self.cold_rows -= removed;
            self.len -= removed;
            shrinkage -= removed;
            if page.rows.is_empty() {
                self.cold_pages.pop_back();
            }
        }
    }

    pub(crate) fn scroll_up_full(&mut self, positions: usize, max_len: usize)
    where
        T: Default,
    {
        self.rezero();
        self.inner.truncate(self.active_len());
        for _ in 0..positions {
            if self.len == max_len {
                self.evict_oldest();
            }
            let columns = self.columns();
            self.inner.insert(0, Row::new(columns));
            self.len += 1;
        }
    }

    #[inline]
    fn reusable_row_bytes(&self) -> usize {
        self.reusable_rows
            .iter()
            .map(|row| row.capacity().saturating_mul(mem::size_of::<T>()))
            .sum()
    }

    #[inline]
    fn cache_reusable_row(&mut self, row: Vec<T>) {
        if self.reusable_rows.len() >= MAX_REUSABLE_ROW_BUFFERS {
            return;
        }
        let bytes = row.capacity().saturating_mul(mem::size_of::<T>());
        if self.reusable_row_bytes().saturating_add(bytes) <= MAX_REUSABLE_ROW_BYTES {
            self.reusable_rows.push(row);
        }
    }

    #[inline]
    pub fn with_capacity(visible_lines: usize, columns: usize) -> Storage<T>
    where
        T: Default,
    {
        // Initialize visible lines; the scrollback buffer is initialized dynamically.
        let mut inner = Vec::with_capacity(visible_lines);
        let fill = Arc::new(T::default());
        inner.resize_with(visible_lines, || Row::new_uniform(columns, fill.clone()));

        Storage {
            inner,
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines,
            len: visible_lines,
        }
    }

    /// Increase the number of lines in the buffer.
    #[inline]
    pub fn grow_visible_lines(&mut self, next: usize)
    where
        T: Default,
    {
        // Number of lines the buffer needs to grow.
        let additional_lines = next - self.visible_lines;

        let columns = self[Line(0)].len();
        self.initialize(additional_lines, columns);

        // Update visible lines.
        self.visible_lines = next;
    }

    /// Decrease the number of lines in the buffer.
    #[inline]
    pub fn shrink_visible_lines(&mut self, next: usize) {
        // Shrink the size without removing any lines.
        let shrinkage = self.visible_lines - next;
        self.shrink_lines(shrinkage);

        // Update visible lines.
        self.visible_lines = next;
    }

    /// Shrink the number of lines in the buffer.
    #[inline]
    pub fn shrink_lines(&mut self, shrinkage: usize) {
        self.shrink_oldest(shrinkage);
        self.trim_cache();
    }

    /// Shrink the logical buffer without compacting its row cache immediately.
    /// The terminal uses this while parsing alternate-screen output and trims
    /// once when the completed frame is consumed.
    #[inline]
    pub fn shrink_lines_deferred(&mut self, shrinkage: usize) {
        self.shrink_oldest(shrinkage);
    }

    /// Truncate the invisible elements from the raw buffer.
    #[inline]
    pub fn truncate(&mut self) {
        self.rezero();
        self.inner.truncate(self.active_len());
    }

    /// Release all inactive rows and the outer vector's spare capacity.
    #[inline]
    pub fn compact(&mut self) {
        self.truncate();
        self.inner.shrink_to_fit();
        self.reusable_rows.clear();
        self.reusable_rows.shrink_to_fit();
    }

    /// Dynamically grow the storage buffer at runtime.
    #[inline]
    pub fn initialize(&mut self, additional_rows: usize, columns: usize)
    where
        T: Default,
    {
        self.materialize_cold();
        if self.len + additional_rows > self.inner.len() {
            self.rezero();

            let cache_rows = cache_row_limit::<T>(columns);
            let realloc_size = self.inner.len() + max(additional_rows, cache_rows);
            let fill = Arc::new(T::default());
            self.inner
                .resize_with(realloc_size, || Row::new_uniform(columns, fill.clone()));
        }

        self.len += additional_rows;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Estimated shallow bytes held by inactive rows and outer-vector capacity.
    /// Dynamic cell extras are deliberately excluded.
    #[inline]
    pub fn cache_bytes(&self) -> usize {
        let row_bytes = mem::size_of::<Row<T>>()
            .saturating_add(self.columns().saturating_mul(mem::size_of::<T>()));
        let cached_rows = self.inner.len().saturating_sub(self.active_len());
        let outer_spare = self.inner.capacity().saturating_sub(self.inner.len())
            .saturating_mul(mem::size_of::<Row<T>>());
        let reusable_bytes = self
            .reusable_rows
            .capacity()
            .saturating_mul(mem::size_of::<Vec<T>>())
            .saturating_add(
                self.reusable_rows
                    .iter()
                    .map(|row| row.capacity().saturating_mul(mem::size_of::<T>()))
                    .sum::<usize>(),
            );
        cached_rows
            .saturating_mul(row_bytes)
            .saturating_add(outer_spare)
            .saturating_add(reusable_bytes)
    }

    /// Shallow storage owned by all allocated rows and the outer vector.
    #[inline]
    pub fn estimated_storage_bytes(&self) -> usize {
        self.storage_metrics().estimated_bytes
    }

    /// Physical cell slots allocated by active, historical, and cached rows.
    #[inline]
    pub fn allocated_cell_capacity(&self) -> usize {
        self.storage_metrics().allocated_cells
    }

    pub(crate) fn storage_metrics(&self) -> StorageMetrics {
        let row_descriptor_bytes = self
            .inner
            .capacity()
            .saturating_mul(mem::size_of::<Row<T>>())
            .saturating_add(
                self.cold_pages
                    .iter()
                    .map(|page| page.rows.capacity().saturating_mul(mem::size_of::<ColdRow>()))
                    .sum::<usize>(),
            );
        let mut metrics = StorageMetrics {
            row_descriptor_bytes,
            cache_bytes: self.cache_bytes(),
            allocations: usize::from(self.inner.capacity() != 0),
            ..StorageMetrics::default()
        };
        let mut packed = HashSet::new();
        let mut uniform = HashSet::new();

        for row in &self.inner {
            if let Some(block) = row.packed_block() {
                metrics.packed_rows += 1;
                let pointer = Arc::as_ptr(block) as usize;
                if packed.insert(pointer) {
                    metrics.packed_blocks += 1;
                    metrics.packed_block_bytes = metrics
                        .packed_block_bytes
                        .saturating_add(block.heap_bytes());
                    metrics.allocated_cells = metrics
                        .allocated_cells
                        .saturating_add(block.value_capacity());
                    metrics.allocations = metrics
                        .allocations
                        .saturating_add(block.allocation_count());
                }
            } else if let Some(fill) = row.uniform_fill() {
                metrics.dense_rows += 1;
                let pointer = Arc::as_ptr(fill) as usize;
                if uniform.insert(pointer) {
                    metrics.dense_cell_bytes = metrics.dense_cell_bytes.saturating_add(
                        mem::size_of::<T>()
                            .saturating_add(2usize.saturating_mul(mem::size_of::<usize>())),
                    );
                    metrics.allocated_cells = metrics.allocated_cells.saturating_add(1);
                    metrics.allocations = metrics.allocations.saturating_add(1);
                }
            } else {
                metrics.dense_rows += 1;
                metrics.dense_cell_bytes = metrics
                    .dense_cell_bytes
                    .saturating_add(row.heap_storage_bytes());
                metrics.allocated_cells = metrics
                    .allocated_cells
                    .saturating_add(row.allocated_cells());
                metrics.allocations = metrics
                    .allocations
                    .saturating_add(usize::from(row.allocated_cells() != 0));
            }
        }

        for page in &self.cold_pages {
            metrics.packed_rows = metrics.packed_rows.saturating_add(page.rows.len());
            let pointer = Arc::as_ptr(&page.block) as usize;
            if packed.insert(pointer) {
                metrics.packed_blocks += 1;
                metrics.packed_block_bytes = metrics
                    .packed_block_bytes
                    .saturating_add(page.block.heap_bytes());
                metrics.allocated_cells = metrics
                    .allocated_cells
                    .saturating_add(page.block.value_capacity());
                metrics.allocations = metrics
                    .allocations
                    .saturating_add(page.block.allocation_count());
            }
            metrics.allocations = metrics.allocations.saturating_add(1);
        }

        metrics.dense_cell_bytes = metrics.dense_cell_bytes.saturating_add(
            self.reusable_rows
                .iter()
                .map(|row| row.capacity().saturating_mul(mem::size_of::<T>()))
                .sum::<usize>()
                .saturating_add(
                    self.reusable_rows
                        .capacity()
                        .saturating_mul(mem::size_of::<Vec<T>>()),
                ),
        );
        metrics.allocated_cells = metrics.allocated_cells.saturating_add(
            self.reusable_rows.iter().map(Vec::capacity).sum::<usize>(),
        );
        metrics.allocations = metrics.allocations.saturating_add(self.reusable_rows.len());

        metrics.estimated_bytes = row_descriptor_bytes
            .saturating_add(metrics.dense_cell_bytes)
            .saturating_add(metrics.packed_block_bytes);
        metrics
    }

    #[inline]
    fn columns(&self) -> usize {
        self.inner.first().map(Row::len).unwrap_or(1)
    }

    /// Pack newly cold rows at the mutable/immutable boundary.
    ///
    /// Normal output leaves at most `HOT_HISTORY_ROWS` plus
    /// `MIN_PACKED_BLOCK_ROWS - 1` dense history rows. The first call for an
    /// already populated terminal packs the complete cold run in bounded
    /// blocks; later calls inspect only the new dense frontier.
    pub(crate) fn pack_cold_history(&mut self) -> usize
    where
        T: Clone + Eq + Hash,
    {
        let mut packed_rows = 0;
        while let count @ 1.. = self.pack_one_cold_page(true) {
            packed_rows += count;
        }
        packed_rows
    }

    /// Incremental consumer boundary. Visits at most 512 rows and encodes at
    /// most one ordinary block. Reset `cursor` after any grid mutation.
    pub(crate) fn pack_cold_history_step(&mut self, cursor: &mut usize, full_scan: bool) -> bool
    where
        T: Clone + Eq + Hash,
    {
        let _ = full_scan;
        let packed = self.pack_one_cold_page(full_scan);
        *cursor = self.len.saturating_sub(self.visible_lines);
        packed != 0
    }

    /// Repack every dense cold run after operations such as width reflow.
    pub(crate) fn pack_all_cold_history(&mut self) -> usize
    where
        T: Clone + Eq + Hash,
    {
        let mut packed_rows = 0;
        while let count @ 1.. = self.pack_one_cold_page(true) {
            packed_rows += count;
        }
        packed_rows
    }

    fn pack_one_cold_page(&mut self, allow_small: bool) -> usize
    where
        T: Clone + Eq + Hash,
    {
        self.rezero();
        self.inner.truncate(self.active_len());
        let frontier = self.visible_lines.saturating_add(HOT_HISTORY_ROWS);
        let candidates = self.inner.len().saturating_sub(frontier);
        if candidates == 0 || (!allow_small && candidates < MIN_PACKED_BLOCK_ROWS) {
            return 0;
        }

        let end = self.inner.len();
        let mut start = end;
        let mut shallow_bytes = 0usize;
        while start > frontier && end - start < PACKED_BLOCK_MAX_ROWS {
            let next = start - 1;
            let row_bytes = self.inner[next]
                .physical_len()
                .saturating_mul(mem::size_of::<T>());
            if start != end
                && shallow_bytes.saturating_add(row_bytes) > PACKED_BLOCK_TARGET_BYTES
            {
                break;
            }
            shallow_bytes = shallow_bytes.saturating_add(row_bytes);
            start = next;
        }
        if !allow_small && end - start < MIN_PACKED_BLOCK_ROWS {
            return 0;
        }

        let rows: Vec<_> = self.inner.drain(start..end).collect();
        let count = rows.len();
        let mut page = self.pack_rows(rows);
        for older in &mut self.cold_pages {
            older.end_row += count;
        }
        page.end_row = count;
        self.cold_pages.push_front(page);
        self.cold_rows += count;
        count
    }

    fn pack_rows(&mut self, mut rows: Vec<Row<T>>) -> ColdPage<T>
    where
        T: Clone + Eq + Hash,
    {
        debug_assert!(!rows.is_empty());
        debug_assert!(rows.iter().all(|row| !row.is_packed()));

        let total_cells = rows.iter().map(Row::physical_len).sum::<usize>();
        let row_layout: Vec<_> = rows
            .iter()
            .map(|row| (row.physical_len(), row.occupancy()))
            .collect();
        let direct_bytes = total_cells.saturating_mul(mem::size_of::<T>());
        let mut values = Vec::new();
        let mut dictionary: Option<FingerprintMap> = None;
        let mut narrow_indices = Vec::with_capacity(total_cells);
        let mut wide_indices = None::<Vec<u16>>;
        let mut direct_cells = None::<Vec<T>>;

        // Release each dense source allocation as soon as its cells have been
        // encoded. The final page grows while the remaining source shrinks,
        // avoiding two complete page representations at peak ingestion.
        for row in &mut rows {
            let mut cells = row.take_cells_for_packing();
            for cell in cells.drain(..) {
                if let Some(direct) = direct_cells.as_mut() {
                    dictionary = None;
                    direct.push(cell);
                    continue;
                }

                let mut added_value = false;
                let dictionary_index = match dictionary.as_mut() {
                    Some(dictionary) => {
                        let key = fingerprint(&cell);
                        match dictionary.get(&key).copied() {
                            Some(index) if values[index as usize] == cell => index,
                            Some(_) => {
                                let mut direct = indexed_cells_into_direct(
                                    mem::take(&mut values),
                                    mem::take(&mut narrow_indices),
                                    wide_indices.take(),
                                    total_cells,
                                );
                                direct.push(cell);
                                direct_cells = Some(direct);
                                continue;
                            },
                            None => {
                                let Ok(index) = u16::try_from(values.len()) else {
                                    let mut direct = indexed_cells_into_direct(
                                        mem::take(&mut values),
                                        mem::take(&mut narrow_indices),
                                        wide_indices.take(),
                                        total_cells,
                                    );
                                    direct.push(cell);
                                    direct_cells = Some(direct);
                                    continue;
                                };
                                reserve_dictionary_value(&mut values, total_cells);
                                values.push(cell);
                                added_value = true;
                                dictionary.insert(key, index);
                                index
                            },
                        }
                    },
                    None => match values.iter().position(|value| value == &cell) {
                        Some(index) => index as u16,
                        None => {
                            let Ok(index) = u16::try_from(values.len()) else {
                                let mut direct = indexed_cells_into_direct(
                                    mem::take(&mut values),
                                    mem::take(&mut narrow_indices),
                                    wide_indices.take(),
                                    total_cells,
                                );
                                direct.push(cell);
                                direct_cells = Some(direct);
                                continue;
                            };
                            reserve_dictionary_value(&mut values, total_cells);
                            values.push(cell);
                            added_value = true;
                            index
                        },
                    },
                };

                match wide_indices.as_mut() {
                    Some(indices) => indices.push(dictionary_index),
                    None => match u8::try_from(dictionary_index) {
                        Ok(index) => narrow_indices.push(index),
                        Err(_) => {
                            let mut indices = Vec::with_capacity(total_cells);
                            indices.extend(narrow_indices.drain(..).map(u16::from));
                            indices.push(dictionary_index);
                            wide_indices = Some(indices);
                        },
                    },
                }

                if added_value
                    && dictionary.is_none()
                    && values.len() == LINEAR_DICTIONARY_CELLS
                {
                    let Some(promoted) = fingerprint_map(&values) else {
                        direct_cells = Some(indexed_cells_into_direct(
                            mem::take(&mut values),
                            mem::take(&mut narrow_indices),
                            wide_indices.take(),
                            total_cells,
                        ));
                        continue;
                    };
                    dictionary = Some(promoted);
                }

                if added_value {
                    let index_width = if wide_indices.is_some() { 2 } else { 1 };
                    let indexed_bytes = values
                        .len()
                        .saturating_mul(mem::size_of::<T>())
                        .saturating_add(total_cells.saturating_mul(index_width));
                    if indexed_bytes >= direct_bytes {
                        direct_cells = Some(indexed_cells_into_direct(
                            mem::take(&mut values),
                            mem::take(&mut narrow_indices),
                            wide_indices.take(),
                            total_cells,
                        ));
                        dictionary = None;
                    }
                }
            }
            self.cache_reusable_row(cells);
        }

        let block = match direct_cells {
            Some(cells) => Row::new_direct_block(cells),
            None => match wide_indices {
                Some(indices) => Row::new_indexed16_block(values, indices),
                None => Row::new_indexed8_block(values, narrow_indices),
            },
        };
        drop(rows);

        let mut start = 0usize;
        let mut metadata = Vec::with_capacity(row_layout.len());
        for (len, occ) in row_layout {
            start += len;
            metadata.push(ColdRow {
                end: u32::try_from(start).expect("packed page offset exceeds u32"),
                occ,
            });
        }
        ColdPage { block, rows: metadata, end_row: 0 }
    }

    pub(crate) fn reset_row<D>(&mut self, line: Line, template: &T)
    where
        T: Clone + Default + super::GridCell + crate::term::cell::ResetDiscriminant<D>,
        D: PartialEq,
    {
        if self.logical_index(line) >= self.active_len() {
            self.materialize_cold();
        }
        let index = self.compute_index(line);
        self.inner[index].reset_reusing(template, &mut self.reusable_rows);
    }

    /// Swap two rows while respecting the ring buffer's logical indexing.
    ///
    /// Upstream used a fixed-size pointer swap here. `Row` now carries compact
    /// storage metadata, so delegate to `Vec::swap` instead of encoding the
    /// structure's size in unsafe code.
    pub fn swap(&mut self, a: Line, b: Line) {
        if self.logical_index(a) >= self.active_len() || self.logical_index(b) >= self.active_len() {
            self.materialize_cold();
        }
        let a = self.compute_index(a);
        let b = self.compute_index(b);
        self.inner.swap(a, b);
    }

    /// Rotate the grid, moving all lines up/down in history.
    #[inline]
    pub fn rotate(&mut self, count: isize) {
        self.materialize_cold();
        debug_assert!(count.unsigned_abs() <= self.inner.len());

        let len = self.inner.len();
        self.zero = (self.zero as isize + count + len as isize) as usize % len;
    }

    /// Rotate all existing lines down in history.
    ///
    /// This is a faster, specialized version of [`rotate_left`].
    ///
    /// [`rotate_left`]: https://doc.rust-lang.org/std/vec/struct.Vec.html#method.rotate_left
    #[inline]
    pub fn rotate_down(&mut self, count: usize) {
        self.materialize_cold();
        self.zero = (self.zero + count) % self.inner.len();
    }

    /// Update the raw storage buffer.
    #[inline]
    pub fn replace_inner(&mut self, vec: Vec<Row<T>>) {
        self.cold_pages.clear();
        self.cold_rows = 0;
        self.len = vec.len();
        self.inner = vec;
        self.zero = 0;
        self.trim_cache();
    }

    /// Remove all rows from storage.
    #[inline]
    pub fn take_all(&mut self) -> Vec<Row<T>> {
        self.materialize_cold();
        self.truncate();

        let mut buffer = Vec::new();

        mem::swap(&mut buffer, &mut self.inner);
        self.len = 0;

        buffer
    }

    /// Compute actual index in underlying storage given the requested index.
    #[inline]
    fn logical_index(&self, requested: Line) -> usize {
        debug_assert!(requested.0 < self.visible_lines as i32);

        let positive = -(requested - self.visible_lines).0 as usize - 1;

        debug_assert!(positive < self.len);

        positive
    }

    #[inline]
    fn compute_active_index(&self, logical: usize) -> usize {
        debug_assert!(logical < self.active_len());

        let zeroed = self.zero + logical;

        // Use if/else instead of remainder here to improve performance.
        //
        // Requires `zeroed` to be smaller than `self.inner.len() * 2`,
        // but both `self.zero` and `requested` are always smaller than `self.inner.len()`.
        if zeroed >= self.inner.len() { zeroed - self.inner.len() } else { zeroed }
    }

    #[inline]
    fn compute_index(&self, requested: Line) -> usize {
        self.compute_active_index(self.logical_index(requested))
    }

    /// Rotate the ringbuffer to reset `self.zero` back to index `0`.
    #[inline]
    fn rezero(&mut self) {
        if self.zero == 0 {
            return;
        }

        self.inner.rotate_left(self.zero);
        self.zero = 0;
    }

    /// Keep both cached rows and spare outer-vector capacity inside the byte
    /// target. Width reflow can replace storage with a short vector that still
    /// owns capacity for tens of thousands of row descriptors, so limiting
    /// only initialized rows is insufficient.
    pub(super) fn trim_cache(&mut self) {
        let columns = self.columns();
        let active_len = self.active_len();
        let cache_rows = cache_row_limit::<T>(columns);
        if self.inner.len() > active_len.saturating_add(cache_rows) {
            self.truncate();
        }

        while self.reusable_rows.len() > MAX_REUSABLE_ROW_BUFFERS
            || self.reusable_row_bytes() > MAX_REUSABLE_ROW_BYTES
        {
            self.reusable_rows.pop();
        }

        let outer_spare_rows = MAX_DESCRIPTOR_CACHE_BYTES
            .saturating_div(mem::size_of::<Row<T>>().max(1));
        let target_capacity = self.active_len().saturating_add(outer_spare_rows);
        let shrink_threshold = target_capacity.saturating_add(outer_spare_rows.max(MIN_CACHE_ROWS));
        if self.inner.capacity() > shrink_threshold {
            self.inner.shrink_to(target_capacity);
        }
    }
}

impl<T> Index<Line> for Storage<T> {
    type Output = Row<T>;

    #[inline]
    fn index(&self, index: Line) -> &Self::Output {
        let index = self.compute_index(index);
        &self.inner[index]
    }
}

impl<T> IndexMut<Line> for Storage<T> {
    #[inline]
    fn index_mut(&mut self, index: Line) -> &mut Self::Output {
        self.row_mut(index)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::hash::{Hash, Hasher};
    use std::mem;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::grid::GridCell;
    use crate::grid::row::Row;
    use crate::grid::storage::{
        MAX_CACHE_BYTES, MAX_DESCRIPTOR_CACHE_BYTES, MAX_REUSABLE_ROW_BUFFERS,
        MAX_REUSABLE_ROW_BYTES, MIN_CACHE_ROWS, Storage, cache_row_limit,
    };
    use crate::index::{Column, Line};
    use crate::term::cell::Flags;

    #[derive(Debug)]
    struct CloneProbe {
        value: u32,
        clones: Arc<AtomicUsize>,
    }

    impl Clone for CloneProbe {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::Relaxed);
            Self { value: self.value, clones: self.clones.clone() }
        }
    }

    impl Default for CloneProbe {
        fn default() -> Self {
            Self { value: 0, clones: Arc::new(AtomicUsize::new(0)) }
        }
    }

    impl PartialEq for CloneProbe {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Eq for CloneProbe {}

    impl Hash for CloneProbe {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.value.hash(state);
        }
    }

    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    struct CollisionProbe(u32);

    impl Hash for CollisionProbe {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0_u8.hash(state);
        }
    }

    impl GridCell for char {
        fn is_empty(&self) -> bool {
            *self == ' ' || *self == '\t'
        }

        fn reset(&mut self, template: &Self) {
            *self = *template;
        }

        fn flags(&self) -> &Flags {
            unimplemented!();
        }

        fn flags_mut(&mut self) -> &mut Flags {
            unimplemented!();
        }
    }

    #[test]
    fn with_capacity() {
        let storage = Storage::<char>::with_capacity(3, 1);

        assert_eq!(storage.inner.len(), 3);
        assert_eq!(storage.len, 3);
        assert_eq!(storage.zero, 0);
        assert_eq!(storage.visible_lines, 3);
    }

    #[test]
    fn indexing() {
        let mut storage = Storage::<char>::with_capacity(3, 1);

        storage[Line(0)] = filled_row('0');
        storage[Line(1)] = filled_row('1');
        storage[Line(2)] = filled_row('2');

        storage.zero += 1;

        assert_eq!(storage[Line(0)], filled_row('2'));
        assert_eq!(storage[Line(1)], filled_row('0'));
        assert_eq!(storage[Line(2)], filled_row('1'));
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn indexing_above_inner_len() {
        let storage = Storage::<char>::with_capacity(1, 1);
        let _ = &storage[Line(-1)];
    }

    #[test]
    fn rotate() {
        let mut storage = Storage::<char>::with_capacity(3, 1);
        storage.rotate(2);
        assert_eq!(storage.zero, 2);
        storage.shrink_lines(2);
        assert_eq!(storage.len, 1);
        assert_eq!(storage.inner.len(), 3);
        assert_eq!(storage.zero, 2);
    }

    /// Grow the buffer one line at the end of the buffer.
    ///
    /// Before:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: -
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: -
    ///   3: \0
    ///   ...
    ///   byte-capped cache: \0
    #[test]
    fn grow_after_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('-')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines: 3,
            len: 3,
        };

        // Grow buffer.
        storage.grow_visible_lines(4);

        // Make sure the result is correct.
        let mut expected = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('-')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines: 4,
            len: 4,
        };
        expected.inner.append(&mut vec![filled_row('\0'); cache_row_limit::<char>(1)]);

        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Grow the buffer one line at the start of the buffer.
    ///
    /// Before:
    ///   0: -
    ///   1: 0 <- Zero
    ///   2: 1
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: -
    ///   3: \0
    ///   ...
    ///   byte-capped cache: \0
    #[test]
    fn grow_before_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('-'), filled_row('0'), filled_row('1')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 1,
            visible_lines: 3,
            len: 3,
        };

        // Grow buffer.
        storage.grow_visible_lines(4);

        // Make sure the result is correct.
        let mut expected = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('-')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines: 4,
            len: 4,
        };
        expected.inner.append(&mut vec![filled_row('\0'); cache_row_limit::<char>(1)]);

        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Shrink the buffer one line at the start of the buffer.
    ///
    /// Before:
    ///   0: 2
    ///   1: 0 <- Zero
    ///   2: 1
    /// After:
    ///   0: 2 <- Hidden
    ///   0: 0 <- Zero
    ///   1: 1
    #[test]
    fn shrink_before_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('2'), filled_row('0'), filled_row('1')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 1,
            visible_lines: 3,
            len: 3,
        };

        // Shrink buffer.
        storage.shrink_visible_lines(2);

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![filled_row('2'), filled_row('0'), filled_row('1')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 1,
            visible_lines: 2,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Shrink the buffer one line at the end of the buffer.
    ///
    /// Before:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: 2
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    ///   2: 2 <- Hidden
    #[test]
    fn shrink_after_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('2')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines: 3,
            len: 3,
        };

        // Shrink buffer.
        storage.shrink_visible_lines(2);

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![filled_row('0'), filled_row('1'), filled_row('2')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines: 2,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Shrink the buffer at the start and end of the buffer.
    ///
    /// Before:
    ///   0: 4
    ///   1: 5
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2
    ///   5: 3
    /// After:
    ///   0: 4 <- Hidden
    ///   1: 5 <- Hidden
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2 <- Hidden
    ///   5: 3 <- Hidden
    #[test]
    fn shrink_before_and_after_zero() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 2,
            visible_lines: 6,
            len: 6,
        };

        // Shrink buffer.
        storage.shrink_visible_lines(2);

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 2,
            visible_lines: 2,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Check that when truncating all hidden lines are removed from the raw buffer.
    ///
    /// Before:
    ///   0: 4 <- Hidden
    ///   1: 5 <- Hidden
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2 <- Hidden
    ///   5: 3 <- Hidden
    /// After:
    ///   0: 0 <- Zero
    ///   1: 1
    #[test]
    fn truncate_invisible_lines() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 2,
            visible_lines: 1,
            len: 2,
        };

        // Truncate buffer.
        storage.truncate();

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![filled_row('0'), filled_row('1')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines: 1,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// Truncate buffer only at the beginning.
    ///
    /// Before:
    ///   0: 1
    ///   1: 2 <- Hidden
    ///   2: 0 <- Zero
    /// After:
    ///   0: 1
    ///   0: 0 <- Zero
    #[test]
    fn truncate_invisible_lines_beginning() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('1'), filled_row('2'), filled_row('0')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 2,
            visible_lines: 1,
            len: 2,
        };

        // Truncate buffer.
        storage.truncate();

        // Make sure the result is correct.
        let expected = Storage {
            inner: vec![filled_row('0'), filled_row('1')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines: 1,
            len: 2,
        };
        assert_eq!(storage.visible_lines, expected.visible_lines);
        assert_eq!(storage.inner, expected.inner);
        assert_eq!(storage.zero, expected.zero);
        assert_eq!(storage.len, expected.len);
    }

    /// First shrink the buffer and then grow it again.
    ///
    /// Before:
    ///   0: 4
    ///   1: 5
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2
    ///   5: 3
    /// After Shrinking:
    ///   0: 4 <- Hidden
    ///   1: 5 <- Hidden
    ///   2: 0 <- Zero
    ///   3: 1
    ///   4: 2
    ///   5: 3 <- Hidden
    /// After Growing:
    ///   0: 4
    ///   1: 5
    ///   2: -
    ///   3: 0 <- Zero
    ///   4: 1
    ///   5: 2
    ///   6: 3
    #[test]
    fn shrink_then_grow() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 2,
            visible_lines: 0,
            len: 6,
        };

        // Shrink buffer.
        storage.shrink_lines(3);

        // Make sure the result after shrinking is correct.
        let shrinking_expected = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 2,
            visible_lines: 0,
            len: 3,
        };
        assert_eq!(storage.inner, shrinking_expected.inner);
        assert_eq!(storage.zero, shrinking_expected.zero);
        assert_eq!(storage.len, shrinking_expected.len);

        // Grow buffer.
        storage.initialize(1, 1);

        // Make sure the previously freed elements are reused.
        let growing_expected = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 2,
            visible_lines: 0,
            len: 4,
        };

        assert_eq!(storage.inner, growing_expected.inner);
        assert_eq!(storage.zero, growing_expected.zero);
        assert_eq!(storage.len, growing_expected.len);
    }

    #[test]
    fn initialize() {
        // Setup storage area.
        let mut storage: Storage<char> = Storage {
            inner: vec![
                filled_row('4'),
                filled_row('5'),
                filled_row('0'),
                filled_row('1'),
                filled_row('2'),
                filled_row('3'),
            ],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 2,
            visible_lines: 0,
            len: 6,
        };

        // Initialize additional lines.
        let init_size = 3;
        storage.initialize(init_size, 1);

        // Generate expected grid.
        let mut expected_inner = vec![
            filled_row('0'),
            filled_row('1'),
            filled_row('2'),
            filled_row('3'),
            filled_row('4'),
            filled_row('5'),
        ];
        let expected_init_size = std::cmp::max(init_size, cache_row_limit::<char>(1));
        expected_inner.append(&mut vec![filled_row('\0'); expected_init_size]);
        let expected_storage = Storage {
            inner: expected_inner,
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines: 0,
            len: 9,
        };

        assert_eq!(storage.len, expected_storage.len);
        assert_eq!(storage.zero, expected_storage.zero);
        assert_eq!(storage.inner, expected_storage.inner);
    }

    #[test]
    fn rotate_wrap_zero() {
        let mut storage: Storage<char> = Storage {
            inner: vec![filled_row('-'), filled_row('-'), filled_row('-')],
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 2,
            visible_lines: 0,
            len: 3,
        };

        storage.rotate(2);

        assert!(storage.zero < storage.inner.len());
    }

    #[test]
    fn cache_rows_follow_the_byte_target() {
        for columns in [80, 120, 240] {
            let rows = cache_row_limit::<[u8; 32]>(columns);
            let row_bytes = mem::size_of::<Row<[u8; 32]>>() + columns * mem::size_of::<[u8; 32]>();
            assert!(rows >= MIN_CACHE_ROWS);
            assert!(rows == MIN_CACHE_ROWS || rows * row_bytes <= MAX_CACHE_BYTES);
        }
        assert!(
            cache_row_limit::<[u8; 32]>(80) > cache_row_limit::<[u8; 32]>(240),
            "wide terminals retain fewer spare rows"
        );
    }

    #[test]
    fn replacing_reflowed_rows_trims_outer_capacity() {
        let mut rows = Vec::with_capacity(20_000);
        rows.extend((0..32).map(|_| filled_row('x')));
        let mut storage = Storage::with_capacity(1, 1);

        storage.replace_inner(rows);

        assert_eq!(storage.len, 32);
        assert!(storage.cache_bytes() <= MAX_CACHE_BYTES);
    }

    #[test]
    fn compact_releases_cached_rows_and_outer_capacity() {
        let mut inner = Vec::with_capacity(64);
        inner.extend((0..32).map(|_| filled_row('x')));
        let mut storage = Storage {
            inner,
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 7,
            visible_lines: 3,
            len: 3,
        };

        storage.compact();

        assert_eq!(storage.inner.len(), 3);
        assert_eq!(storage.inner.capacity(), 3);
        assert_eq!(storage.zero, 0);
        assert_eq!(storage.cache_bytes(), 0);
    }

    #[test]
    fn quiet_trim_reclaims_geometric_descriptor_capacity_with_hysteresis() {
        let mut inner = Vec::with_capacity(20_000);
        inner.extend((0..512).map(|_| filled_row('x')));
        let mut storage = Storage {
            inner,
            cold_pages: VecDeque::new(),
            cold_rows: 0,
            reusable_rows: Vec::new(),
            zero: 0,
            visible_lines: 3,
            len: 512,
        };

        storage.trim_cache();

        let descriptor_reserve = MAX_DESCRIPTOR_CACHE_BYTES / mem::size_of::<Row<char>>();
        assert!(storage.inner.capacity() <= storage.inner.len() + descriptor_reserve);
        let stable_capacity = storage.inner.capacity();
        storage.trim_cache();
        assert_eq!(storage.inner.capacity(), stable_capacity);
    }

    #[test]
    fn reusable_row_cache_is_bounded_by_count_and_bytes() {
        let mut storage = Storage::<char>::with_capacity(3, 1);
        storage.reusable_rows.clear();

        for _ in 0..MAX_REUSABLE_ROW_BUFFERS + 8 {
            storage.cache_reusable_row(vec!['x']);
        }
        assert_eq!(storage.reusable_rows.len(), MAX_REUSABLE_ROW_BUFFERS);

        storage.reusable_rows.clear();
        let oversized_cells = MAX_REUSABLE_ROW_BYTES / mem::size_of::<char>() + 1;
        storage.cache_reusable_row(vec!['x'; oversized_cells]);
        assert!(storage.reusable_rows.is_empty());
    }

    #[test]
    fn cold_history_shares_dictionary_blocks_and_mutation_inflates_one_row() {
        let columns = 80;
        let mut storage = Storage::<char>::with_capacity(3, columns);
        storage.initialize(256, columns);
        for age in 0..256 {
            storage[Line(-((age + 1) as i32))][Column(0)] = 'x';
        }

        let before = storage.storage_metrics();
        assert_eq!(storage.pack_cold_history(), 224);
        let packed = storage.storage_metrics();

        assert_eq!(packed.packed_rows, 224);
        assert_eq!(packed.packed_blocks, 1);
        assert!(packed.packed_block_bytes < before.dense_cell_bytes / 4);
        assert_eq!(*storage.cell(Line(-200), Column(79)), '\0');

        let reusable = storage.reusable_rows.len();
        assert!(reusable > 0);
        assert!(reusable <= MAX_REUSABLE_ROW_BUFFERS);
        assert!(storage.reusable_row_bytes() <= MAX_REUSABLE_ROW_BYTES);
        storage.reset_row(Line(-200), &'\0');
        assert_eq!(storage.reusable_rows.len(), reusable - 1);
        assert!(!storage.row(Line(-200)).is_packed());

        storage[Line(-200)][Column(0)] = 'y';
        assert_eq!(storage[Line(-200)][Column(0)], 'y');
        assert_eq!(storage[Line(-201)][Column(0)], 'x');
        assert_eq!(storage.storage_metrics().packed_rows, 223);
    }

    #[test]
    fn high_entropy_blocks_stay_inside_the_shallow_byte_target() {
        let columns = 80;
        let history_rows = 240;
        let mut storage = Storage::<[u8; 32]>::with_capacity(3, columns);
        storage.initialize(history_rows, columns);

        for age in 0..history_rows {
            for column in 0..columns {
                let mut value = [0; 32];
                value[..8]
                    .copy_from_slice(&((age * columns + column) as u64).to_le_bytes());
                storage[Line(-((age + 1) as i32))][Column(column)] = value;
            }
        }

        let packed_rows = history_rows - super::HOT_HISTORY_ROWS;
        assert_eq!(storage.pack_cold_history(), packed_rows);
        let metrics = storage.storage_metrics();
        assert!(metrics.packed_blocks > 1);
        assert_eq!(metrics.packed_rows, packed_rows);
        let direct_cell_bytes = packed_rows * columns * mem::size_of::<[u8; 32]>();
        assert!(metrics.packed_block_bytes >= direct_cell_bytes);
        assert!(
            metrics.packed_block_bytes
                <= direct_cell_bytes.saturating_add(metrics.packed_blocks * 128)
        );
        assert!(
            metrics.packed_block_bytes
                <= metrics.packed_blocks * (super::PACKED_BLOCK_TARGET_BYTES + 128)
        );
        for age in 0..history_rows {
            for column in 0..columns {
                let expected = ((age * columns + column) as u64).to_le_bytes();
                assert_eq!(
                    storage.cell(Line(-((age + 1) as i32)), Column(column))[..8],
                    expected
                );
            }
        }
    }

    #[test]
    fn cold_page_builder_moves_dense_cells_without_cloning() {
        let columns = 80;
        let history_rows = 64;

        for unique in [false, true] {
            let clones = Arc::new(AtomicUsize::new(0));
            let mut storage = Storage::<CloneProbe>::with_capacity(3, columns);
            storage.initialize(history_rows, columns);
            for age in 0..history_rows {
                for column in 0..columns {
                    let value = if unique { (age * columns + column) as u32 } else { column as u32 };
                    storage[Line(-((age + 1) as i32))][Column(column)] =
                        CloneProbe { value, clones: clones.clone() };
                }
            }

            clones.store(0, Ordering::Relaxed);
            assert_eq!(storage.pack_cold_history(), history_rows - super::HOT_HISTORY_ROWS);
            assert_eq!(clones.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn fingerprint_collision_falls_back_without_losing_cells() {
        let columns = 40;
        let history_rows = 64;
        let mut storage = Storage::<CollisionProbe>::with_capacity(3, columns);
        storage.initialize(history_rows, columns);
        for age in 0..history_rows {
            for column in 0..columns {
                let value = if column % 7 == 0 { 7 } else { (age * columns + column) as u32 };
                storage[Line(-((age + 1) as i32))][Column(column)] = CollisionProbe(value);
            }
        }

        assert_eq!(storage.pack_cold_history(), history_rows - super::HOT_HISTORY_ROWS);
        for age in 0..history_rows {
            for column in 0..columns {
                let expected = if column % 7 == 0 { 7 } else { (age * columns + column) as u32 };
                assert_eq!(
                    storage.cell(Line(-((age + 1) as i32)), Column(column)).0,
                    expected
                );
            }
        }
    }

    #[test]
    fn cold_history_packing_is_idempotent_and_keeps_a_small_frontier() {
        let columns = 20;
        let mut storage = Storage::<char>::with_capacity(4, columns);
        storage.initialize(320, columns);

        assert_eq!(storage.pack_cold_history(), 288);
        assert_eq!(storage.pack_cold_history(), 0);
        assert!(!storage.row(Line(-1)).is_packed());
        assert!(!storage.row(Line(-32)).is_packed());
        assert!(storage.row(Line(-33)).is_packed());
        assert!(storage.row(Line(-320)).is_packed());
    }

    #[test]
    fn cold_page_lookup_and_oldest_shrink_cross_page_boundaries() {
        let history_rows = 1_100;
        let mut storage = Storage::<u32>::with_capacity(2, 1);
        storage.initialize(history_rows, 1);
        for age in 1..=history_rows {
            storage[Line(-(age as i32))][Column(0)] = age as u32;
        }

        assert_eq!(storage.pack_cold_history(), history_rows - super::HOT_HISTORY_ROWS);
        assert!(storage.cold_pages.len() >= 3);
        assert_eq!(storage.cold_pages.back().unwrap().end_row, storage.cold_rows);
        assert!(
            storage
                .cold_pages
                .iter()
                .map(|page| page.end_row)
                .is_sorted()
        );
        for age in [33, 511, 512, 513, 1_024, 1_100] {
            assert_eq!(*storage.cell(Line(-age), Column(0)), age as u32);
        }

        storage.shrink_lines(300);
        assert_eq!(storage.len(), 802);
        assert_eq!(storage.cold_pages.back().unwrap().end_row, storage.cold_rows);
        assert_eq!(*storage.cell(Line(-800), Column(0)), 800);
        for age in [33, 511, 512, 513, 799] {
            assert_eq!(*storage.cell(Line(-age), Column(0)), age as u32);
        }
    }

    fn filled_row(content: char) -> Row<char> {
        let mut row = Row::new(1);
        row[Column(0)] = content;
        row
    }
}
