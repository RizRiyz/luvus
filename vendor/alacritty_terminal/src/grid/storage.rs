use std::cmp::max;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::mem;
use std::ops::{Index, IndexMut};
use std::sync::Arc;

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

/// Keep a few rows ready so ordinary short bursts do not allocate line by line.
const MIN_CACHE_ROWS: usize = 8;
const MAX_REUSABLE_ROW_BUFFERS: usize = 256;

/// Recent history remains as ordinary mutable rows. Older completed rows can
/// share dictionary-compressed blocks without affecting the active parser.
const HOT_HISTORY_ROWS: usize = 128;

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
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Storage<T> {
    inner: Vec<Row<T>>,

    /// Small cell vectors recycled between packed history and the live row.
    /// This is bounded independently from scrollback and never serialized.
    #[cfg_attr(feature = "serde", serde(skip))]
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

impl<T: PartialEq> PartialEq for Storage<T> {
    fn eq(&self, other: &Self) -> bool {
        // Both storage buffers need to be truncated and zeroed.
        assert_eq!(self.zero, 0);
        assert_eq!(other.zero, 0);

        self.inner == other.inner && self.len == other.len
    }
}

impl<T> Storage<T> {
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
        self.len -= shrinkage;
        self.trim_cache();
    }

    /// Shrink the logical buffer without compacting its row cache immediately.
    /// The terminal uses this while parsing alternate-screen output and trims
    /// once when the completed frame is consumed.
    #[inline]
    pub fn shrink_lines_deferred(&mut self, shrinkage: usize) {
        self.len -= shrinkage;
    }

    /// Truncate the invisible elements from the raw buffer.
    #[inline]
    pub fn truncate(&mut self) {
        self.rezero();

        self.inner.truncate(self.len);
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
        let cached_rows = self.inner.len().saturating_sub(self.len);
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
        let row_descriptor_bytes = self.inner.capacity().saturating_mul(mem::size_of::<Row<T>>());
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
        let history_rows = self.len.saturating_sub(self.visible_lines);
        if history_rows < HOT_HISTORY_ROWS + MIN_PACKED_BLOCK_ROWS {
            return 0;
        }

        let first_age = HOT_HISTORY_ROWS;
        let first = self.compute_index(Line(-((first_age + 1) as i32)));
        if self.inner[first].is_packed() {
            return 0;
        }

        let mut dense_run = 0;
        for age in first_age..history_rows {
            let index = self.compute_index(Line(-((age + 1) as i32)));
            if self.inner[index].is_packed() {
                break;
            }
            dense_run += 1;
        }

        if dense_run < MIN_PACKED_BLOCK_ROWS {
            return 0;
        }

        let mut packed_rows = 0;
        while packed_rows < dense_run {
            let remaining = dense_run - packed_rows;
            let mut rows = Vec::with_capacity(remaining.min(PACKED_BLOCK_MAX_ROWS));
            let mut shallow_bytes = 0usize;
            for age in first_age + packed_rows..first_age + dense_run {
                let index = self.compute_index(Line(-((age + 1) as i32)));
                let row_bytes = self.inner[index]
                    .physical_len()
                    .saturating_mul(mem::size_of::<T>());
                if !rows.is_empty()
                    && (rows.len() == PACKED_BLOCK_MAX_ROWS
                        || shallow_bytes.saturating_add(row_bytes)
                            > PACKED_BLOCK_TARGET_BYTES)
                {
                    break;
                }
                rows.push(index);
                shallow_bytes = shallow_bytes.saturating_add(row_bytes);
            }
            self.pack_rows(&rows);
            packed_rows += rows.len();
        }
        packed_rows
    }

    /// Repack every dense cold run after operations such as width reflow.
    pub(crate) fn pack_all_cold_history(&mut self) -> usize
    where
        T: Clone + Eq + Hash,
    {
        let history_rows = self.len.saturating_sub(self.visible_lines);
        if history_rows < HOT_HISTORY_ROWS + MIN_PACKED_BLOCK_ROWS {
            return 0;
        }

        let mut packed_rows = 0;
        let mut age = HOT_HISTORY_ROWS;
        while age < history_rows {
            let index = self.compute_index(Line(-((age + 1) as i32)));
            if self.inner[index].is_packed() {
                age += 1;
                continue;
            }

            let start = age;
            let mut shallow_bytes = 0usize;
            while age < history_rows {
                let index = self.compute_index(Line(-((age + 1) as i32)));
                if self.inner[index].is_packed() || age - start == PACKED_BLOCK_MAX_ROWS {
                    break;
                }
                let row_bytes = self.inner[index]
                    .physical_len()
                    .saturating_mul(mem::size_of::<T>());
                if age != start
                    && shallow_bytes.saturating_add(row_bytes) > PACKED_BLOCK_TARGET_BYTES
                {
                    break;
                }
                shallow_bytes = shallow_bytes.saturating_add(row_bytes);
                age += 1;
            }

            let count = age - start;
            if count == 0 {
                continue;
            }
            let mut rows = Vec::with_capacity(count);
            for row_age in start..age {
                rows.push(self.compute_index(Line(-((row_age + 1) as i32))));
            }
            self.pack_rows(&rows);
            packed_rows += count;
        }
        packed_rows
    }

    fn pack_rows(&mut self, rows: &[usize])
    where
        T: Clone + Eq + Hash,
    {
        debug_assert!(!rows.is_empty());
        debug_assert!(rows.iter().all(|index| !self.inner[*index].is_packed()));

        let total_cells = rows
            .iter()
            .map(|index| self.inner[*index].physical_len())
            .sum::<usize>();
        let mut values = Vec::new();
        let mut dictionary: Option<FingerprintMap> = None;
        let mut narrow_indices = Vec::with_capacity(total_cells);
        let mut wide_indices = None::<Vec<u16>>;
        let mut dictionary_complete = true;

        'rows: for index in rows {
            let row = &self.inner[*index];
            for cell_index in 0..row.physical_len() {
                let cell = row.physical_cell(cell_index);
                let dictionary_index = match dictionary.as_mut() {
                    Some(dictionary) => {
                        let fingerprint = fingerprint(cell);
                        match dictionary.get(&fingerprint) {
                            Some(index) if values[*index as usize] == *cell => *index,
                            Some(_) => {
                                dictionary_complete = false;
                                break 'rows;
                            },
                            None => {
                                let Ok(index) = u16::try_from(values.len()) else {
                                    dictionary_complete = false;
                                    break 'rows;
                                };
                                values.push(cell.clone());
                                dictionary.insert(fingerprint, index);
                                index
                            },
                        }
                    },
                    None => match values.iter().position(|value| value == cell) {
                        Some(index) => index as u16,
                        None => {
                            // Small terminal dictionaries (plain text, ANSI
                            // palettes, prompts) are faster as a linear scan:
                            // most failed comparisons stop after the glyph.
                            // Promote to hashed lookup before this can become
                            // quadratic for high-entropy content.
                            let Ok(index) = u16::try_from(values.len()) else {
                                dictionary_complete = false;
                                break 'rows;
                            };
                            values.push(cell.clone());
                            if values.len() == LINEAR_DICTIONARY_CELLS {
                                let Some(promoted) = fingerprint_map(&values) else {
                                    dictionary_complete = false;
                                    break 'rows;
                                };
                                dictionary = Some(promoted);
                            }
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
            }
        }

        let indexed_width = if wide_indices.is_some() { 2 } else { 1 };
        let indices_len = wide_indices
            .as_ref()
            .map_or(narrow_indices.len(), Vec::len);
        let indexed_bytes = values
            .len()
            .saturating_mul(mem::size_of::<T>())
            .saturating_add(total_cells.saturating_mul(indexed_width));
        let direct_bytes = total_cells.saturating_mul(mem::size_of::<T>());

        let block = if dictionary_complete
            && indices_len == total_cells
            && indexed_bytes < direct_bytes
        {
            match wide_indices {
                Some(indices) => Row::new_indexed16_block(values, indices),
                None => Row::new_indexed8_block(values, narrow_indices),
            }
        } else {
            let mut cells = Vec::with_capacity(total_cells);
            for index in rows {
                let row = &self.inner[*index];
                cells.extend((0..row.physical_len()).map(|cell| row.physical_cell(cell).clone()));
            }
            Row::new_direct_block(cells)
        };

        let mut start = 0;
        for index in rows {
            let len = self.inner[*index].physical_len();
            if let Some(reusable) = self.inner[*index].install_packed(block.clone(), start, len) {
                if self.reusable_rows.len() < MAX_REUSABLE_ROW_BUFFERS {
                    self.reusable_rows.push(reusable);
                }
            }
            start += len;
        }
    }

    pub(crate) fn reset_row<D>(&mut self, line: Line, template: &T)
    where
        T: Clone + Default + super::GridCell + crate::term::cell::ResetDiscriminant<D>,
        D: PartialEq,
    {
        let index = self.compute_index(line);
        self.inner[index].reset_reusing(template, &mut self.reusable_rows);
    }

    /// Swap two rows while respecting the ring buffer's logical indexing.
    ///
    /// Upstream used a fixed-size pointer swap here. `Row` now carries compact
    /// storage metadata, so delegate to `Vec::swap` instead of encoding the
    /// structure's size in unsafe code.
    pub fn swap(&mut self, a: Line, b: Line) {
        let a = self.compute_index(a);
        let b = self.compute_index(b);
        self.inner.swap(a, b);
    }

    /// Rotate the grid, moving all lines up/down in history.
    #[inline]
    pub fn rotate(&mut self, count: isize) {
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
        self.zero = (self.zero + count) % self.inner.len();
    }

    /// Update the raw storage buffer.
    #[inline]
    pub fn replace_inner(&mut self, vec: Vec<Row<T>>) {
        self.len = vec.len();
        self.inner = vec;
        self.zero = 0;
        self.trim_cache();
    }

    /// Remove all rows from storage.
    #[inline]
    pub fn take_all(&mut self) -> Vec<Row<T>> {
        self.truncate();

        let mut buffer = Vec::new();

        mem::swap(&mut buffer, &mut self.inner);
        self.len = 0;

        buffer
    }

    /// Compute actual index in underlying storage given the requested index.
    #[inline]
    fn compute_index(&self, requested: Line) -> usize {
        debug_assert!(requested.0 < self.visible_lines as i32);

        let positive = -(requested - self.visible_lines).0 as usize - 1;

        debug_assert!(positive < self.len);

        let zeroed = self.zero + positive;

        // Use if/else instead of remainder here to improve performance.
        //
        // Requires `zeroed` to be smaller than `self.inner.len() * 2`,
        // but both `self.zero` and `requested` are always smaller than `self.inner.len()`.
        if zeroed >= self.inner.len() { zeroed - self.inner.len() } else { zeroed }
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
        let cache_rows = cache_row_limit::<T>(columns);
        if self.inner.len() > self.len.saturating_add(cache_rows) {
            self.truncate();
        }

        let row_bytes = mem::size_of::<Row<T>>()
            .saturating_add(columns.saturating_mul(mem::size_of::<T>()));
        let cached_bytes = self.inner.len().saturating_sub(self.len).saturating_mul(row_bytes);
        let outer_spare_rows = MAX_CACHE_BYTES.saturating_sub(cached_bytes)
            .saturating_div(mem::size_of::<Row<T>>().max(1));
        self.inner.shrink_to(self.inner.len().saturating_add(outer_spare_rows));
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
        let index = self.compute_index(index);
        &mut self.inner[index]
    }
}

#[cfg(test)]
mod tests {
    use std::mem;

    use crate::grid::GridCell;
    use crate::grid::row::Row;
    use crate::grid::storage::{
        MAX_CACHE_BYTES, MAX_REUSABLE_ROW_BUFFERS, MIN_CACHE_ROWS, Storage, cache_row_limit,
    };
    use crate::index::{Column, Line};
    use crate::term::cell::Flags;

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
    fn cold_history_shares_dictionary_blocks_and_mutation_inflates_one_row() {
        let columns = 80;
        let mut storage = Storage::<char>::with_capacity(3, columns);
        storage.initialize(256, columns);
        for age in 0..256 {
            storage[Line(-((age + 1) as i32))][Column(0)] = 'x';
        }

        let before = storage.storage_metrics();
        assert_eq!(storage.pack_cold_history(), 128);
        let packed = storage.storage_metrics();

        assert_eq!(packed.packed_rows, 128);
        assert_eq!(packed.packed_blocks, 1);
        assert!(packed.packed_block_bytes < before.dense_cell_bytes / 4);
        assert_eq!(storage[Line(-200)][Column(79)], '\0');

        let reusable = storage.reusable_rows.len();
        assert!(reusable > 0);
        assert!(reusable <= MAX_REUSABLE_ROW_BUFFERS);
        storage.reset_row(Line(-200), &'\0');
        assert_eq!(storage.reusable_rows.len(), reusable - 1);
        assert!(!storage[Line(-200)].is_packed());

        storage[Line(-200)][Column(0)] = 'y';
        assert_eq!(storage[Line(-200)][Column(0)], 'y');
        assert_eq!(storage[Line(-201)][Column(0)], 'x');
        assert_eq!(storage.storage_metrics().packed_rows, 127);
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
    }

    #[test]
    fn cold_history_packing_is_idempotent_and_keeps_hot_rows_dense() {
        let columns = 20;
        let mut storage = Storage::<char>::with_capacity(4, columns);
        storage.initialize(320, columns);

        assert_eq!(storage.pack_cold_history(), 192);
        assert_eq!(storage.pack_cold_history(), 0);
        assert!(!storage[Line(-1)].is_packed());
        assert!(!storage[Line(-128)].is_packed());
        assert!(storage[Line(-129)].is_packed());
        assert!(storage[Line(-320)].is_packed());
    }

    fn filled_row(content: char) -> Row<char> {
        let mut row = Row::new(1);
        row[Column(0)] = content;
        row
    }
}
