//! Structural validation and the page map. Both come from one walk over every b-tree, the
//! freelist and the special pages, so they cannot disagree about which page belongs to what.

use std::collections::HashSet;
use std::io::{Read, Seek};
use std::sync::Arc;

use crate::btree::{
    expect_index, expect_table, load_node, max_payload, read_cell, Node, PageType, MAX_DEPTH,
};
use crate::db::{Database, ObjectKind, SchemaObject};
use crate::error::{CorruptionKind, Error, Result};
use crate::header::Header;
use crate::inspect::{inspect_page, PageInfo};
use crate::pager::Pager;
use crate::record::validate_record;
use crate::schema::parse_create_table;

/// Offset of the lock-byte page: SQLite never stores data in the page that contains it.
const PENDING_BYTE: u64 = 0x4000_0000;
/// Bytes per pointer-map entry.
const PTRMAP_ENTRY_SIZE: usize = 5;
/// SQLite pads cells to at least this many bytes.
const MIN_CELL_SIZE: usize = 4;
/// SQLite rejects pages with more fragmented bytes than this.
const MAX_FRAGMENTED: usize = 60;

/// A group of related checks, reported as one line by `litewalk check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    /// The 100-byte header.
    Header,
    /// Page numbers and file length.
    PageBoundaries,
    /// B-tree page type bytes.
    PageTypes,
    /// Cell pointers, free blocks and page layout.
    CellPointers,
    /// Tree shape: depth, shared pages, child pointers.
    BTreeStructure,
    /// Rowid and key ordering.
    BTreeOrdering,
    /// Overflow chains.
    OverflowChains,
    /// Freelist trunk and leaf pages.
    Freelist,
    /// `sqlite_schema` and its references.
    Schema,
    /// Record headers and bodies.
    RecordPayloads,
    /// Every page belongs to exactly one structure.
    PageUsage,
}

/// All sections in report order.
pub const SECTIONS: [Section; 11] = [
    Section::Header,
    Section::PageBoundaries,
    Section::PageTypes,
    Section::CellPointers,
    Section::BTreeStructure,
    Section::BTreeOrdering,
    Section::OverflowChains,
    Section::Freelist,
    Section::Schema,
    Section::RecordPayloads,
    Section::PageUsage,
];

impl Section {
    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            Section::Header => "SQLite header",
            Section::PageBoundaries => "page boundaries",
            Section::PageTypes => "page types",
            Section::CellPointers => "cell pointers",
            Section::BTreeStructure => "B-tree structure",
            Section::BTreeOrdering => "B-tree ordering",
            Section::OverflowChains => "overflow chains",
            Section::Freelist => "freelist",
            Section::Schema => "schema",
            Section::RecordPayloads => "record payloads",
            Section::PageUsage => "page usage",
        }
    }

    fn of(kind: CorruptionKind) -> Section {
        match kind {
            CorruptionKind::Header | CorruptionKind::Wal => Section::Header,
            CorruptionKind::Truncated | CorruptionKind::PageNumber => Section::PageBoundaries,
            CorruptionKind::PageType => Section::PageTypes,
            CorruptionKind::CellPointer => Section::CellPointers,
            CorruptionKind::Varint | CorruptionKind::Record => Section::RecordPayloads,
            CorruptionKind::Overflow => Section::OverflowChains,
            CorruptionKind::BTree => Section::BTreeStructure,
            CorruptionKind::Freelist => Section::Freelist,
            CorruptionKind::Schema => Section::Schema,
        }
    }
}

/// One problem found by [`Database::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Which group of checks found it.
    pub section: Section,
    /// The page it was found on, if any.
    pub page: Option<u32>,
    /// What is wrong.
    pub message: String,
}

/// Result of [`Database::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// Pages in the database.
    pub page_count: u32,
    /// Tables in the schema.
    pub tables: usize,
    /// Indexes in the schema.
    pub indexes: usize,
    /// Leaf cells visited in table b-trees.
    pub rows: u64,
    /// Everything that was found, in discovery order.
    pub issues: Vec<Issue>,
    /// Sections that could not run because an earlier problem made them meaningless.
    pub skipped: Vec<Section>,
}

impl CheckReport {
    /// True when no issue was found.
    pub fn is_valid(&self) -> bool {
        self.issues.is_empty()
    }

    /// Issues belonging to `section`.
    pub fn issues_in(&self, section: Section) -> Vec<&Issue> {
        self.issues
            .iter()
            .filter(|i| i.section == section)
            .collect()
    }
}

/// What a page is used for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Usage {
    /// Not reachable from the schema, the freelist or the special pages.
    Unreferenced,
    /// A page of the `sqlite_schema` b-tree (page 1 is its root).
    Schema,
    /// A page of a table b-tree.
    Table(Arc<str>),
    /// A page of an index b-tree.
    Index {
        /// Index name.
        name: Arc<str>,
        /// Indexed table.
        table: Arc<str>,
    },
    /// An overflow page.
    Overflow {
        /// Table or index owning the cell.
        object: Arc<str>,
        /// B-tree page holding the cell.
        owner_page: u32,
        /// Rowid of the cell, for table cells.
        rowid: Option<i64>,
    },
    /// A freelist trunk page.
    FreelistTrunk,
    /// A freelist leaf page.
    FreelistLeaf,
    /// An auto-vacuum pointer-map page.
    PointerMap,
    /// The page containing the lock byte at 1 GiB, which SQLite never uses.
    LockByte,
}

impl Usage {
    /// Short category name.
    pub fn category(&self) -> &'static str {
        match self {
            Usage::Unreferenced => "UNREFERENCED",
            Usage::Schema => "SCHEMA",
            Usage::Table(_) => "TABLE",
            Usage::Index { .. } => "INDEX",
            Usage::Overflow { .. } => "OVERFLOW",
            Usage::FreelistTrunk => "FREELIST TRUNK",
            Usage::FreelistLeaf => "FREELIST",
            Usage::PointerMap => "POINTER MAP",
            Usage::LockByte => "LOCK BYTE",
        }
    }
}

/// One line of the page map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntry {
    /// Page number.
    pub page: u32,
    /// Owner of the page.
    pub usage: Usage,
    /// B-tree page type, for b-tree pages.
    pub page_type: Option<PageType>,
}

/// Freelist contents as found by walking it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Freelist {
    /// Trunk pages in chain order.
    pub trunks: Vec<u32>,
    /// Leaf pages listed by the trunks.
    pub leaves: Vec<u32>,
}

pub(crate) struct Analysis {
    pub report: CheckReport,
    pub map: Vec<MapEntry>,
    pub freelist: Freelist,
    pub overflow_pages: u64,
    pub largest_payload: Option<(usize, String, Option<i64>)>,
    /// `(table or index name, b-tree pages, leaf cells)` per object.
    pub per_object: Vec<(String, u32, u64)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TreeKind {
    Table,
    Index,
}

struct Work {
    page: u32,
    parent: Option<u32>,
    depth: usize,
    lo: Option<i64>,
    hi: Option<i64>,
}

struct Analyzer<'a, R> {
    pager: &'a mut Pager<R>,
    usage: Vec<Usage>,
    types: Vec<Option<PageType>>,
    issues: Vec<Issue>,
    complete: bool,
    rows: u64,
    overflow_pages: u64,
    largest_payload: Option<(usize, String, Option<i64>)>,
}

impl<R: Read + Seek> Analyzer<'_, R> {
    fn issue(&mut self, section: Section, page: Option<u32>, message: impl Into<String>) {
        self.issues.push(Issue {
            section,
            page,
            message: message.into(),
        });
    }

    /// Records a corruption as an issue; any other error (I/O) aborts the analysis.
    fn record(&mut self, e: Error, page: Option<u32>) -> Result<()> {
        match e {
            Error::Corrupt(c) => {
                let mut message = c.reason.clone();
                let ctx: Vec<String> = c
                    .context()
                    .into_iter()
                    .filter(|(k, _)| *k != "page" && *k != "table")
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect();
                if !ctx.is_empty() {
                    message = format!("{message} ({})", ctx.join(", "));
                }
                self.issue(Section::of(c.kind), c.page.or(page), message);
                Ok(())
            }
            other => Err(other),
        }
    }

    fn claim(
        &mut self,
        page: u32,
        usage: Usage,
        ty: Option<PageType>,
    ) -> std::result::Result<(), String> {
        let slot = &mut self.usage[page as usize - 1];
        if *slot != Usage::Unreferenced {
            return Err(format!("page is already used as {}", describe(slot)));
        }
        *slot = usage;
        self.types[page as usize - 1] = ty;
        Ok(())
    }

    fn layout(&mut self, number: u32) -> Result<()> {
        let page = self.pager.read_page(number)?;
        let usable = self.pager.usable();
        let info = inspect_page(&page, number, usable, max_payload(self.pager))?;
        for p in &info.problems {
            self.issue(Section::CellPointers, Some(number), p.clone());
        }
        for message in layout_problems(&info) {
            self.issue(Section::CellPointers, Some(number), message);
        }
        Ok(())
    }

    fn walk_tree(
        &mut self,
        root: u32,
        kind: TreeKind,
        name: &Arc<str>,
        table_of_index: Option<&Arc<str>>,
        schema: bool,
    ) -> Result<(u32, u64)> {
        let page_count = self.pager.page_count();
        let mut stack = vec![Work {
            page: root,
            parent: None,
            depth: 0,
            lo: None,
            hi: None,
        }];
        let mut leaf_depth: Option<usize> = None;
        let (mut pages, mut leaf_cells) = (0u32, 0u64);
        let mut last_rowid: Option<i64> = None;

        while let Some(w) = stack.pop() {
            if w.page == 0 || w.page > page_count {
                self.complete = false;
                let what = if w.parent.is_none() {
                    "root page"
                } else {
                    "child pointer"
                };
                self.issue(
                    Section::PageBoundaries,
                    w.parent.or(Some(w.page)),
                    format!(
                        "{what} points to page {} outside the database (1..={page_count})",
                        w.page
                    ),
                );
                continue;
            }
            let usage = if schema {
                Usage::Schema
            } else if let Some(t) = table_of_index {
                Usage::Index {
                    name: Arc::clone(name),
                    table: Arc::clone(t),
                }
            } else {
                Usage::Table(Arc::clone(name)) // includes WITHOUT ROWID tables, stored as index b-trees
            };
            if let Err(msg) = self.claim(w.page, usage, None) {
                self.complete = false;
                self.issue(Section::BTreeStructure, Some(w.page), msg);
                continue;
            }
            pages += 1;
            if w.depth >= MAX_DEPTH {
                self.complete = false;
                self.issue(
                    Section::BTreeStructure,
                    Some(w.page),
                    format!("b-tree is deeper than {MAX_DEPTH} levels"),
                );
                continue;
            }
            let node = match load_node(self.pager, w.page) {
                Ok(n) => n,
                Err(e) => {
                    self.complete = false;
                    self.record(e, Some(w.page))?;
                    continue;
                }
            };
            self.types[w.page as usize - 1] = Some(node.page_type());
            let kind_ok = match kind {
                TreeKind::Table => expect_table(&node),
                TreeKind::Index => expect_index(&node),
            };
            if let Err(e) = kind_ok {
                self.complete = false;
                self.record(e, Some(w.page))?;
                continue;
            }
            self.layout(w.page)?;

            match node {
                Node::InteriorTable { children, keys, .. } => {
                    if keys.is_empty() && w.depth > 0 {
                        self.issue(
                            Section::BTreeStructure,
                            Some(w.page),
                            "non-root interior page has no cells",
                        );
                    }
                    for (i, &k) in keys.iter().enumerate() {
                        if w.lo.is_some_and(|lo| k <= lo) || w.hi.is_some_and(|hi| k > hi) {
                            self.issue(Section::BTreeOrdering, Some(w.page), format!("cell {i} key {k} is outside the range its parent allows ({:?}, {:?}]", w.lo, w.hi));
                        }
                    }
                    // Pushed right to left so the stack pops them left to right.
                    for (i, &child) in children.iter().enumerate().rev() {
                        let lo = if i == 0 { w.lo } else { Some(keys[i - 1]) };
                        let hi = if i == keys.len() { w.hi } else { Some(keys[i]) };
                        stack.push(Work {
                            page: child,
                            parent: Some(w.page),
                            depth: w.depth + 1,
                            lo,
                            hi,
                        });
                    }
                }
                Node::InteriorIndex {
                    number,
                    page,
                    cells,
                    rightmost,
                } => {
                    if cells.is_empty() && w.depth > 0 {
                        self.issue(
                            Section::BTreeStructure,
                            Some(number),
                            "non-root interior page has no cells",
                        );
                    }
                    stack.push(Work {
                        page: rightmost,
                        parent: Some(number),
                        depth: w.depth + 1,
                        lo: None,
                        hi: None,
                    });
                    for (i, &off) in cells.iter().enumerate().rev() {
                        match read_cell(self.pager, &page, number, i, PageType::InteriorIndex, off)
                        {
                            Ok((meta, payload)) => {
                                self.cell_payload(number, name, None, &meta, &payload)?;
                                let child = meta.left_child.expect("interior cells have children");
                                stack.push(Work {
                                    page: child,
                                    parent: Some(number),
                                    depth: w.depth + 1,
                                    lo: None,
                                    hi: None,
                                });
                            }
                            Err(e) => {
                                self.complete = false;
                                self.record(e, Some(number))?;
                            }
                        }
                    }
                }
                Node::LeafTable {
                    number,
                    page,
                    cells,
                } => {
                    self.leaf_depth(&mut leaf_depth, number, w.depth);
                    for (i, &off) in cells.iter().enumerate() {
                        match read_cell(self.pager, &page, number, i, PageType::LeafTable, off) {
                            Ok((meta, payload)) => {
                                let rowid = meta.rowid.expect("table leaf cells have rowids");
                                leaf_cells += 1;
                                self.rows += 1;
                                if last_rowid.is_some_and(|p| p >= rowid)
                                    || w.lo.is_some_and(|lo| rowid <= lo)
                                    || w.hi.is_some_and(|hi| rowid > hi)
                                {
                                    self.issue(Section::BTreeOrdering, Some(number), format!("rowid {rowid} (cell {i}) is out of order or outside its parent's range"));
                                }
                                last_rowid = Some(rowid);
                                self.cell_payload(number, name, Some(rowid), &meta, &payload)?;
                            }
                            Err(e) => {
                                self.complete = false;
                                self.record(e, Some(number))?;
                            }
                        }
                    }
                }
                Node::LeafIndex {
                    number,
                    page,
                    cells,
                } => {
                    self.leaf_depth(&mut leaf_depth, number, w.depth);
                    for (i, &off) in cells.iter().enumerate() {
                        match read_cell(self.pager, &page, number, i, PageType::LeafIndex, off) {
                            Ok((meta, payload)) => {
                                leaf_cells += 1;
                                self.cell_payload(number, name, None, &meta, &payload)?;
                            }
                            Err(e) => {
                                self.complete = false;
                                self.record(e, Some(number))?;
                            }
                        }
                    }
                }
            }
        }
        Ok((pages, leaf_cells))
    }

    fn leaf_depth(&mut self, expected: &mut Option<usize>, page: u32, depth: usize) {
        match *expected {
            None => *expected = Some(depth),
            Some(d) if d != depth => {
                self.issue(
                    Section::BTreeStructure,
                    Some(page),
                    format!("leaf is at depth {depth}, other leaves are at depth {d}"),
                );
            }
            Some(_) => {}
        }
    }

    /// Claims a cell's overflow pages and validates its record.
    fn cell_payload(
        &mut self,
        page: u32,
        object: &Arc<str>,
        rowid: Option<i64>,
        meta: &crate::btree::CellMeta,
        payload: &crate::btree::Payload,
    ) -> Result<()> {
        let total = payload.bytes.len();
        match &self.largest_payload {
            Some((largest, _, _)) if total <= *largest => {}
            _ => self.largest_payload = Some((total, object.to_string(), rowid)),
        }
        for &op in &payload.overflow {
            self.overflow_pages += 1;
            let usage = Usage::Overflow {
                object: Arc::clone(object),
                owner_page: page,
                rowid,
            };
            if let Err(msg) = self.claim(op, usage, None) {
                self.complete = false;
                self.issue(
                    Section::OverflowChains,
                    Some(op),
                    format!(
                        "{msg} (overflow of page {page}, cell at offset {})",
                        meta.offset
                    ),
                );
            }
        }
        if payload.tail != 0 {
            let last = payload.overflow.last().copied();
            self.issue(
                Section::OverflowChains,
                last,
                format!("last overflow page links to page {}; a complete chain ends with 0 (cell at offset {} on page {page})", payload.tail, meta.offset),
            );
        }
        if let Err(e) = validate_record(&payload.bytes) {
            let e = e.with_page(page);
            let e = match rowid {
                Some(r) => e.with_rowid(r),
                None => e,
            };
            self.record(e, Some(page))?;
        }
        Ok(())
    }

    fn walk_freelist(&mut self, header: &Header) -> Result<Freelist> {
        let page_count = self.pager.page_count();
        let usable = self.pager.usable();
        let mut list = Freelist::default();
        let mut seen = HashSet::new();
        let mut trunk = header.freelist_trunk;
        while trunk != 0 {
            if trunk > page_count {
                self.issue(
                    Section::Freelist,
                    Some(trunk),
                    format!(
                        "freelist trunk pointer {trunk} is outside the database (1..={page_count})"
                    ),
                );
                break;
            }
            if !seen.insert(trunk) {
                self.issue(
                    Section::Freelist,
                    Some(trunk),
                    "freelist trunk chain contains a cycle",
                );
                break;
            }
            if let Err(msg) = self.claim(trunk, Usage::FreelistTrunk, None) {
                self.issue(Section::Freelist, Some(trunk), msg);
                break;
            }
            list.trunks.push(trunk);
            let page = self.pager.read_page(trunk)?;
            let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            let n = u32::from_be_bytes([page[4], page[5], page[6], page[7]]) as usize;
            let capacity = usable / 4 - 2;
            if n > capacity {
                self.issue(
                    Section::Freelist,
                    Some(trunk),
                    format!("trunk lists {n} leaf pages but a page holds at most {capacity}"),
                );
                break;
            }
            for i in 0..n {
                let at = 8 + 4 * i;
                let leaf = u32::from_be_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]]);
                if leaf == 0 || leaf > page_count {
                    self.issue(Section::Freelist, Some(trunk), format!("freelist leaf pointer {leaf} is outside the database (1..={page_count})"));
                    continue;
                }
                match self.claim(leaf, Usage::FreelistLeaf, None) {
                    Ok(()) => list.leaves.push(leaf),
                    Err(msg) => self.issue(Section::Freelist, Some(leaf), msg),
                }
            }
            trunk = next;
        }
        let found = (list.trunks.len() + list.leaves.len()) as u32;
        if found != header.freelist_count {
            self.issue(
                Section::Freelist,
                None,
                format!(
                    "header says the freelist has {} pages, walking it found {found}",
                    header.freelist_count
                ),
            );
        }
        Ok(list)
    }
}

fn describe(u: &Usage) -> String {
    match u {
        Usage::Table(n) => format!("table {n}"),
        Usage::Index { name, .. } => format!("index {name}"),
        Usage::Overflow { object, .. } => format!("overflow of {object}"),
        other => other.category().to_lowercase(),
    }
}

fn layout_problems(info: &PageInfo) -> Vec<String> {
    let mut out = Vec::new();
    if info.fragmented_bytes > MAX_FRAGMENTED {
        out.push(format!(
            "{} fragmented bytes (limit {MAX_FRAGMENTED})",
            info.fragmented_bytes
        ));
    }
    let mut spans: Vec<(usize, usize, String)> = Vec::new();
    for c in info.cells.iter().filter(|c| c.error.is_none()) {
        if c.offset < info.content_start {
            out.push(format!(
                "cell {} starts at {}, before the cell content area at {}",
                c.index, c.offset, info.content_start
            ));
        }
        spans.push((
            c.offset,
            c.offset + c.size.max(MIN_CELL_SIZE),
            format!("cell {}", c.index),
        ));
    }
    for b in &info.freeblocks {
        spans.push((b.offset, b.offset + b.size, "free block".into()));
    }
    spans.sort();
    for w in spans.windows(2) {
        if w[1].0 < w[0].1 {
            out.push(format!(
                "{} at {} overlaps {} at {}",
                w[1].2, w[1].0, w[0].2, w[0].0
            ));
        }
    }
    out
}

fn check_header(h: &Header, issues: &mut Vec<Issue>) {
    let mut add = |m: String| {
        issues.push(Issue {
            section: Section::Header,
            page: None,
            message: m,
        })
    };
    if h.schema_format > 4 {
        add(format!("schema format {} is not in 0..=4", h.schema_format));
    }
    if h.freelist_count > h.page_count {
        add(format!(
            "freelist count {} exceeds the page count {}",
            h.freelist_count, h.page_count
        ));
    }
    if h.largest_root_page > h.page_count {
        add(format!(
            "largest root page {} exceeds the page count {}",
            h.largest_root_page, h.page_count
        ));
    }
    if h.freelist_trunk > h.page_count {
        add(format!(
            "freelist trunk page {} exceeds the page count {}",
            h.freelist_trunk, h.page_count
        ));
    }
}

pub(crate) fn analyze<R: Read + Seek>(db: &mut Database<R>) -> Result<Analysis> {
    let (objects, schema_error) = db.schema_lenient()?;
    let header = db.pager.header.clone();
    let pager = &mut db.pager;
    let page_count = header.page_count as usize;
    let mut a = Analyzer {
        pager,
        usage: vec![Usage::Unreferenced; page_count],
        types: vec![None; page_count],
        issues: Vec::new(),
        complete: true,
        rows: 0,
        overflow_pages: 0,
        largest_payload: None,
    };
    check_header(&header, &mut a.issues);

    // Pages that exist but hold no b-tree.
    if header.largest_root_page != 0 {
        let step = a.pager.usable() / PTRMAP_ENTRY_SIZE + 1;
        let mut p = 2usize;
        while p <= page_count {
            let _ = a.claim(p as u32, Usage::PointerMap, None);
            p += step;
        }
    }
    let lock = PENDING_BYTE / header.page_size as u64 + 1;
    if lock <= page_count as u64 {
        let _ = a.claim(lock as u32, Usage::LockByte, None);
    }

    let mut per_object = Vec::new();
    let schema_name: Arc<str> = Arc::from("sqlite_schema");
    if let Some(e) = schema_error {
        a.complete = false;
        a.record(e, Some(1))?;
    }
    let (pages, cells) = a.walk_tree(1, TreeKind::Table, &schema_name, None, true)?;
    per_object.push(("sqlite_schema".to_owned(), pages, cells));

    let mut names: HashSet<String> = HashSet::new();
    let table_names: HashSet<String> = objects
        .iter()
        .filter(|o| o.kind == ObjectKind::Table)
        .map(|o| o.name.to_ascii_lowercase())
        .collect();
    let (mut tables, mut indexes) = (0, 0);
    for o in &objects {
        if !names.insert(o.name.to_ascii_lowercase()) {
            a.issue(
                Section::Schema,
                Some(1),
                format!("duplicate schema object name \"{}\"", o.name),
            );
        }
        match o.kind {
            ObjectKind::View | ObjectKind::Trigger => {
                if o.root_page != 0 {
                    a.issue(
                        Section::Schema,
                        Some(1),
                        format!(
                            "{} \"{}\" has root page {}; expected 0",
                            o.kind.as_str(),
                            o.name,
                            o.root_page
                        ),
                    );
                }
                continue;
            }
            ObjectKind::Table => tables += 1,
            ObjectKind::Index => indexes += 1,
        }
        if o.kind == ObjectKind::Index && !table_names.contains(&o.table_name.to_ascii_lowercase())
        {
            a.issue(
                Section::Schema,
                Some(1),
                format!(
                    "index \"{}\" belongs to unknown table \"{}\"",
                    o.name, o.table_name
                ),
            );
        }
        if o.root_page == 0 {
            if !is_virtual(o) {
                a.issue(
                    Section::Schema,
                    Some(1),
                    format!("{} \"{}\" has root page 0", o.kind.as_str(), o.name),
                );
            }
            continue;
        }
        let name: Arc<str> = Arc::from(o.name.as_str());
        let (kind, table_of_index) = match o.kind {
            ObjectKind::Index => (
                TreeKind::Index,
                Some(Arc::<str>::from(o.table_name.as_str())),
            ),
            _ => {
                let without_rowid = o
                    .sql
                    .as_deref()
                    .and_then(parse_create_table)
                    .is_some_and(|d| d.without_rowid);
                (
                    if without_rowid {
                        TreeKind::Index
                    } else {
                        TreeKind::Table
                    },
                    None,
                )
            }
        };
        let (pages, cells) =
            a.walk_tree(o.root_page, kind, &name, table_of_index.as_ref(), false)?;
        per_object.push((o.name.clone(), pages, cells));
    }

    let freelist = a.walk_freelist(&header)?;

    let mut skipped = Vec::new();
    if a.complete {
        for (i, u) in a.usage.iter().enumerate() {
            if *u == Usage::Unreferenced {
                let page = i as u32 + 1;
                a.issues.push(Issue {
                    section: Section::PageUsage,
                    page: Some(page),
                    message: "page is not used by any b-tree, overflow chain or freelist".into(),
                });
            }
        }
    } else {
        skipped.push(Section::PageUsage);
    }

    let map = a
        .usage
        .iter()
        .zip(&a.types)
        .enumerate()
        .map(|(i, (u, t))| MapEntry {
            page: i as u32 + 1,
            usage: u.clone(),
            page_type: *t,
        })
        .collect();
    let report = CheckReport {
        page_count: header.page_count,
        tables,
        indexes,
        rows: a.rows,
        issues: a.issues,
        skipped,
    };
    Ok(Analysis {
        report,
        map,
        freelist,
        overflow_pages: a.overflow_pages,
        largest_payload: a.largest_payload,
        per_object,
    })
}

fn is_virtual(o: &SchemaObject) -> bool {
    o.sql.as_deref().is_some_and(|s| {
        s.trim_start()
            .to_ascii_uppercase()
            .starts_with("CREATE VIRTUAL")
    })
}

/// Report for input that is not even a readable database header.
fn header_failure(error: Error) -> Result<CheckReport> {
    let (section, message) = match &error {
        Error::NotSqlite => (Section::Header, "not a SQLite database".to_owned()),
        Error::Corrupt(c) => (Section::of(c.kind), c.to_string()),
        _ => return Err(error),
    };
    Ok(CheckReport {
        page_count: 0,
        tables: 0,
        indexes: 0,
        rows: 0,
        issues: vec![Issue {
            section,
            page: None,
            message,
        }],
        skipped: SECTIONS.into_iter().filter(|s| *s != section).collect(),
    })
}

/// Checks a database from a reader. Only I/O failures are returned as `Err`; an unreadable
/// header is reported as an issue.
pub fn check_reader<R: Read + Seek>(src: R) -> Result<CheckReport> {
    match Database::new(src) {
        Ok(mut db) => db.check(),
        Err(e) => header_failure(e),
    }
}

impl<R: Read + Seek> Database<R> {
    /// Validates the whole file structure. See the README for the list of checks. Findings are
    /// returned in the report; `Err` is only returned for I/O failures.
    pub fn check(&mut self) -> Result<CheckReport> {
        analyze(self).map(|a| a.report)
    }

    /// Which structure owns each page, in page order.
    pub fn map(&mut self) -> Result<Vec<MapEntry>> {
        analyze(self).map(|a| a.map)
    }

    /// Walks the freelist. Problems found on the way are reported by [`check`](Self::check).
    pub fn freelist(&mut self) -> Result<Freelist> {
        analyze(self).map(|a| a.freelist)
    }

    /// Decodes one page without following any pointers. Non-b-tree pages (overflow, freelist,
    /// pointer map) come back with `page_type == None`.
    pub fn page(&mut self, number: u32) -> Result<PageInfo> {
        let count = self.pager.page_count();
        if number == 0 || number > count {
            return Err(Error::NoSuchPage {
                page: number,
                page_count: count,
            });
        }
        let page = self.pager.read_page(number)?;
        let usable = self.pager.usable();
        let max = max_payload(&self.pager);
        inspect_page(&page, number, usable, max)
    }

    /// The raw bytes of a page.
    pub fn page_bytes(&mut self, number: u32) -> Result<Arc<[u8]>> {
        let count = self.pager.page_count();
        if number == 0 || number > count {
            return Err(Error::NoSuchPage {
                page: number,
                page_count: count,
            });
        }
        self.pager.read_page(number)
    }
}
