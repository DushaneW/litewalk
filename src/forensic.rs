//! Read-only structural analysis: how the file is laid out and where unused space is.
//!
//! This reports what the file contains. It does not recover deleted records.

use std::io::{Read, Seek};

use crate::btree::max_payload;
use crate::check::{analyze, Issue, Usage};
use crate::db::Database;
use crate::error::Result;
use crate::inspect::inspect_page;

/// Kind of unused region inside a b-tree page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeRegionKind {
    /// Gap between the cell pointer array and the cell content area.
    Unallocated,
    /// A block on the page's free block chain (left behind by deleted or shrunk cells).
    FreeBlock,
}

/// A byte range inside a b-tree page that holds no live cell. Old cell content may still be
/// present in it; litewalk does not interpret it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeRegion {
    /// Page number.
    pub page: u32,
    /// Kind of region.
    pub kind: FreeRegionKind,
    /// Offset within the page.
    pub offset: usize,
    /// Length in bytes.
    pub len: usize,
}

/// Unused bytes summed over all b-tree pages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FreeSpaceSummary {
    /// Bytes in unallocated gaps.
    pub unallocated_bytes: u64,
    /// Bytes in free blocks.
    pub freeblock_bytes: u64,
    /// Number of free blocks.
    pub freeblock_count: u64,
    /// Fragmented bytes (too small to be free blocks).
    pub fragmented_bytes: u64,
}

/// Size of one table or index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSize {
    /// Table or index name.
    pub name: String,
    /// B-tree pages.
    pub pages: u32,
    /// Leaf cells (rows for tables, entries for indexes).
    pub cells: u64,
}

/// Structural report. `anomalies` are the findings of [`Database::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForensicReport {
    /// Length of the main database file.
    pub file_size: u64,
    /// Pages in the database.
    pub page_count: u32,
    /// Page size in bytes.
    pub page_size: usize,
    /// Tables in the schema.
    pub tables: usize,
    /// Indexes in the schema.
    pub indexes: usize,
    /// Overflow pages in use.
    pub overflow_pages: u64,
    /// Pages on the freelist.
    pub freelist_pages: u64,
    /// The b-tree with the most pages (excluding `sqlite_schema`).
    pub largest_object: Option<ObjectSize>,
    /// Largest cell payload: `(bytes, table or index, rowid)`.
    pub largest_payload: Option<(usize, String, Option<i64>)>,
    /// Every table and index with its size.
    pub objects: Vec<ObjectSize>,
    /// Unused space inside b-tree pages.
    pub free_space: FreeSpaceSummary,
    /// Structural problems.
    pub anomalies: Vec<Issue>,
}

impl<R: Read + Seek> Database<R> {
    /// Structural report over the whole file.
    pub fn forensic(&mut self) -> Result<ForensicReport> {
        let a = analyze(self)?;
        let free_space = self.free_space_summary(&a.map)?;
        let objects: Vec<ObjectSize> = a
            .per_object
            .iter()
            .map(|(name, pages, cells)| ObjectSize {
                name: name.clone(),
                pages: *pages,
                cells: *cells,
            })
            .collect();
        let largest_object = objects
            .iter()
            .filter(|o| o.name != "sqlite_schema")
            .max_by_key(|o| o.pages)
            .cloned();
        Ok(ForensicReport {
            file_size: self.file_len,
            page_count: a.report.page_count,
            page_size: self.pager.header.page_size,
            tables: a.report.tables,
            indexes: a.report.indexes,
            overflow_pages: a.overflow_pages,
            freelist_pages: (a.freelist.trunks.len() + a.freelist.leaves.len()) as u64,
            largest_object,
            largest_payload: a.largest_payload,
            objects,
            free_space,
            anomalies: a.report.issues,
        })
    }

    fn free_space_summary(&mut self, map: &[crate::check::MapEntry]) -> Result<FreeSpaceSummary> {
        let mut sum = FreeSpaceSummary::default();
        let usable = self.pager.usable();
        let max = max_payload(&self.pager);
        for e in map.iter().filter(|e| e.page_type.is_some()) {
            let page = self.pager.read_page(e.page)?;
            let info = inspect_page(&page, e.page, usable, max)?;
            sum.unallocated_bytes += info.unallocated as u64;
            sum.freeblock_bytes += info.freeblocks.iter().map(|b| b.size as u64).sum::<u64>();
            sum.freeblock_count += info.freeblocks.len() as u64;
            sum.fragmented_bytes += info.fragmented_bytes as u64;
        }
        Ok(sum)
    }

    /// Lists unused regions of every b-tree page, largest first. Experimental: it shows where
    /// stale bytes can be, not what they were.
    pub fn free_regions(&mut self) -> Result<Vec<FreeRegion>> {
        let map = self.map()?;
        let usable = self.pager.usable();
        let max = max_payload(&self.pager);
        let mut out = Vec::new();
        for e in map
            .iter()
            .filter(|e| e.page_type.is_some() && !matches!(e.usage, Usage::Unreferenced))
        {
            let page = self.pager.read_page(e.page)?;
            let info = inspect_page(&page, e.page, usable, max)?;
            if info.unallocated > 0 {
                let kind_len = if info.page_type.is_some_and(|t| t.is_leaf()) {
                    8
                } else {
                    12
                };
                let start = info.header_offset + kind_len + 2 * info.cell_count;
                out.push(FreeRegion {
                    page: e.page,
                    kind: FreeRegionKind::Unallocated,
                    offset: start,
                    len: info.unallocated,
                });
            }
            for b in &info.freeblocks {
                out.push(FreeRegion {
                    page: e.page,
                    kind: FreeRegionKind::FreeBlock,
                    offset: b.offset,
                    len: b.size,
                });
            }
        }
        out.sort_by_key(|r| (std::cmp::Reverse(r.len), r.page, r.offset));
        Ok(out)
    }
}
