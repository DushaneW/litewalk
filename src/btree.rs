//! B-tree pages: header and cell decoding, payloads and overflow chains.

use std::collections::HashSet;
use std::io::{Read, Seek};
use std::sync::Arc;

use crate::error::{corrupt, CorruptionKind, Result};
use crate::header::HEADER_SIZE;
use crate::pager::Pager;
use crate::varint::read_varint;

/// Deepest b-tree this crate will descend. SQLite itself limits depth far below this.
pub(crate) const MAX_DEPTH: usize = 64;

const PAGE_TYPE_INTERIOR_INDEX: u8 = 0x02;
const PAGE_TYPE_INTERIOR_TABLE: u8 = 0x05;
const PAGE_TYPE_LEAF_INDEX: u8 = 0x0a;
const PAGE_TYPE_LEAF_TABLE: u8 = 0x0d;

const LEAF_HEADER_LEN: usize = 8;
const INTERIOR_HEADER_LEN: usize = 12;
const OVERFLOW_POINTER_LEN: usize = 4;
const CELL_POINTER_LEN: usize = 2;

/// B-tree page types (first byte of the page header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    /// Interior page of an index b-tree (0x02).
    InteriorIndex,
    /// Interior page of a table b-tree (0x05).
    InteriorTable,
    /// Leaf page of an index b-tree (0x0a).
    LeafIndex,
    /// Leaf page of a table b-tree (0x0d).
    LeafTable,
}

impl PageType {
    pub(crate) fn from_byte(b: u8) -> Option<PageType> {
        match b {
            PAGE_TYPE_INTERIOR_INDEX => Some(PageType::InteriorIndex),
            PAGE_TYPE_INTERIOR_TABLE => Some(PageType::InteriorTable),
            PAGE_TYPE_LEAF_INDEX => Some(PageType::LeafIndex),
            PAGE_TYPE_LEAF_TABLE => Some(PageType::LeafTable),
            _ => None,
        }
    }

    /// True for leaf pages.
    pub fn is_leaf(self) -> bool {
        matches!(self, PageType::LeafIndex | PageType::LeafTable)
    }

    /// True for table b-tree pages (as opposed to index b-tree pages).
    pub fn is_table(self) -> bool {
        matches!(self, PageType::InteriorTable | PageType::LeafTable)
    }

    /// Human-readable name, e.g. "table leaf".
    pub fn name(self) -> &'static str {
        match self {
            PageType::InteriorIndex => "index interior",
            PageType::InteriorTable => "table interior",
            PageType::LeafIndex => "index leaf",
            PageType::LeafTable => "table leaf",
        }
    }

    fn header_len(self) -> usize {
        if self.is_leaf() {
            LEAF_HEADER_LEN
        } else {
            INTERIOR_HEADER_LEN
        }
    }
}

/// Decoded b-tree page header.
#[derive(Debug, Clone)]
pub(crate) struct PageHeader {
    pub kind: PageType,
    /// 100 on page 1 (after the database header), 0 elsewhere.
    pub base: usize,
    pub first_freeblock: usize,
    pub ncells: usize,
    /// Start of the cell content area (the page header stores 0 for 65536).
    pub content_start: usize,
    pub fragmented: usize,
    pub rightmost: Option<u32>,
}

impl PageHeader {
    /// Offset of the first cell pointer.
    pub fn pointers_start(&self) -> usize {
        self.base + self.kind.header_len()
    }

    /// Offset just past the cell pointer array.
    pub fn pointers_end(&self) -> usize {
        self.pointers_start() + CELL_POINTER_LEN * self.ncells
    }
}

fn be16(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_be_bytes([b[at], b[at + 1]]))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

pub(crate) fn parse_header(page: &[u8], number: u32) -> Result<PageHeader> {
    let base = if number == 1 { HEADER_SIZE } else { 0 };
    let type_byte = page[base];
    let Some(kind) = PageType::from_byte(type_byte) else {
        return Err(corrupt(
            CorruptionKind::PageType,
            format!("invalid b-tree page type {type_byte:#04x}"),
        )
        .with_offset(base)
        .with_expected("0x02, 0x05, 0x0a or 0x0d", format!("{type_byte:#04x}"))
        .with_page(number));
    };
    let content_raw = be16(page, base + 5);
    Ok(PageHeader {
        kind,
        base,
        first_freeblock: be16(page, base + 1),
        ncells: be16(page, base + 3),
        content_start: if content_raw == 0 {
            65_536
        } else {
            content_raw
        },
        fragmented: usize::from(page[base + 7]),
        rightmost: if kind.is_leaf() {
            None
        } else {
            be32(page, base + 8)
        },
    })
}

/// Reads and bounds-checks the cell pointer array.
pub(crate) fn cell_offsets(
    page: &[u8],
    hdr: &PageHeader,
    usable: usize,
    number: u32,
) -> Result<Vec<usize>> {
    let end = hdr.pointers_end();
    if end > usable {
        return Err(corrupt(
            CorruptionKind::CellPointer,
            "cell pointer array extends past the page",
        )
        .with_offset(hdr.pointers_start())
        .with_expected(format!("at most {usable}"), end)
        .with_page(number));
    }
    (0..hdr.ncells)
        .map(|i| {
            let at = hdr.pointers_start() + CELL_POINTER_LEN * i;
            let off = be16(page, at);
            if off < end || off >= usable {
                return Err(corrupt(
                    CorruptionKind::CellPointer,
                    "cell offset is outside the cell content area",
                )
                .with_offset(at)
                .with_cell(i)
                .with_expected(format!("{end}..{usable}"), off)
                .with_page(number));
            }
            Ok(off)
        })
        .collect()
}

/// A b-tree page whose cells are decoded on demand. Point lookups use this instead of
/// [`load_node`], which decodes every cell of the page.
pub(crate) struct PageView {
    pub number: u32,
    pub page: Arc<[u8]>,
    pub hdr: PageHeader,
    usable: usize,
}

impl PageView {
    pub fn load<R: Read + Seek>(pager: &mut Pager<R>, number: u32) -> Result<PageView> {
        let page = pager.read_page(number)?;
        let usable = pager.usable();
        let hdr = parse_header(&page, number)?;
        if hdr.pointers_end() > usable {
            return Err(corrupt(
                CorruptionKind::CellPointer,
                "cell pointer array extends past the page",
            )
            .with_offset(hdr.pointers_start())
            .with_expected(format!("at most {usable}"), hdr.pointers_end())
            .with_page(number));
        }
        Ok(PageView {
            number,
            page,
            hdr,
            usable,
        })
    }

    /// Offset of cell `i`, validated against the cell content area.
    pub fn cell_offset(&self, i: usize) -> Result<usize> {
        let at = self.hdr.pointers_start() + CELL_POINTER_LEN * i;
        let off = be16(&self.page, at);
        if off < self.hdr.pointers_end() || off >= self.usable {
            return Err(corrupt(
                CorruptionKind::CellPointer,
                "cell offset is outside the cell content area",
            )
            .with_offset(at)
            .with_cell(i)
            .with_expected(format!("{}..{}", self.hdr.pointers_end(), self.usable), off)
            .with_page(self.number));
        }
        Ok(off)
    }

    /// Parses cell `i`.
    pub fn cell(&self, i: usize, max_payload: usize) -> Result<CellMeta> {
        cell_meta(
            &self.page,
            self.usable,
            self.hdr.kind,
            self.cell_offset(i)?,
            max_payload,
        )
        .map_err(|e| e.with_page(self.number).with_cell(i))
    }
}

/// A loaded b-tree page, reduced to what traversal needs.
pub(crate) enum Node {
    /// `children.len() == keys.len() + 1`; `keys[i]` is the largest rowid in `children[i]`.
    InteriorTable {
        number: u32,
        children: Vec<u32>,
        keys: Vec<i64>,
    },
    LeafTable {
        number: u32,
        page: Arc<[u8]>,
        cells: Vec<usize>,
    },
    InteriorIndex {
        number: u32,
        page: Arc<[u8]>,
        cells: Vec<usize>,
        rightmost: u32,
    },
    LeafIndex {
        number: u32,
        page: Arc<[u8]>,
        cells: Vec<usize>,
    },
}

impl Node {
    pub fn page_type(&self) -> PageType {
        match self {
            Node::InteriorTable { .. } => PageType::InteriorTable,
            Node::LeafTable { .. } => PageType::LeafTable,
            Node::InteriorIndex { .. } => PageType::InteriorIndex,
            Node::LeafIndex { .. } => PageType::LeafIndex,
        }
    }
}

pub(crate) fn load_node<R: Read + Seek>(pager: &mut Pager<R>, number: u32) -> Result<Node> {
    let page = pager.read_page(number)?;
    let usable = pager.usable();
    let hdr = parse_header(&page, number)?;
    let cells = cell_offsets(&page, &hdr, usable, number)?;

    match hdr.kind {
        PageType::LeafTable => Ok(Node::LeafTable {
            number,
            page,
            cells,
        }),
        PageType::LeafIndex => Ok(Node::LeafIndex {
            number,
            page,
            cells,
        }),
        PageType::InteriorIndex => {
            let Some(rightmost) = hdr.rightmost else {
                return Err(
                    corrupt(CorruptionKind::BTree, "missing rightmost pointer").with_page(number)
                );
            };
            // An interior page with no cells and only a rightmost child is legal: SQLite leaves
            // one as the root of the schema b-tree when the child's content does not fit on
            // page 1 (which loses 100 bytes to the database header).
            Ok(Node::InteriorIndex {
                number,
                page,
                cells,
                rightmost,
            })
        }
        PageType::InteriorTable => {
            let mut children = Vec::with_capacity(cells.len() + 1);
            let mut keys: Vec<i64> = Vec::with_capacity(cells.len());
            for (i, &off) in cells.iter().enumerate() {
                let meta = cell_meta(&page, usable, PageType::InteriorTable, off, usize::MAX)
                    .map_err(|e| e.with_page(number).with_cell(i))?;
                let key = meta.rowid.expect("interior table cells have keys");
                if keys.last().is_some_and(|&prev| prev >= key) {
                    return Err(corrupt(
                        CorruptionKind::BTree,
                        "interior keys are not strictly increasing",
                    )
                    .with_page(number)
                    .with_cell(i)
                    .with_expected(format!("> {}", keys[keys.len() - 1]), key));
                }
                children.push(meta.left_child.expect("interior cells have children"));
                keys.push(key);
            }
            children.push(
                hdr.rightmost
                    .expect("interior pages have a rightmost pointer"),
            );
            Ok(Node::InteriorTable {
                number,
                children,
                keys,
            })
        }
    }
}

/// How many payload bytes are stored on the b-tree page itself; the rest goes to overflow
/// pages (https://www.sqlite.org/fileformat.html#cellformat).
pub(crate) fn local_payload(total: usize, usable: usize, table_leaf: bool) -> usize {
    let max_local = if table_leaf {
        usable - 35
    } else {
        (usable - 12) * 64 / 255 - 23
    };
    if total <= max_local {
        return total;
    }
    let min_local = (usable - 12) * 32 / 255 - 23;
    let k = min_local + (total - min_local) % (usable - OVERFLOW_POINTER_LEN);
    if k <= max_local {
        k
    } else {
        min_local
    }
}

/// Everything about a cell that can be learned from its b-tree page alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CellMeta {
    pub offset: usize,
    pub left_child: Option<u32>,
    pub rowid: Option<i64>,
    /// Total payload bytes (local plus overflow); `None` for interior table cells.
    pub payload_total: Option<usize>,
    pub payload_start: usize,
    pub local: usize,
    pub overflow_page: Option<u32>,
    /// Offset one past the last byte of the cell.
    pub end: usize,
}

fn varint_at(page: &[u8], at: usize, limit: usize, what: &str) -> Result<(u64, usize)> {
    page.get(at..limit).and_then(read_varint).ok_or_else(|| {
        corrupt(CorruptionKind::Varint, format!("{what} is truncated")).with_offset(at)
    })
}

/// Parses a cell header. `max_payload` rejects payload sizes that cannot fit in the database.
pub(crate) fn cell_meta(
    page: &[u8],
    usable: usize,
    kind: PageType,
    offset: usize,
    max_payload: usize,
) -> Result<CellMeta> {
    let mut at = offset;
    let mut left_child = None;
    if !kind.is_leaf() {
        left_child = be32(page, at).filter(|_| at + 4 <= usable);
        if left_child.is_none() {
            return Err(corrupt(
                CorruptionKind::CellPointer,
                "child pointer extends past the page",
            )
            .with_offset(at));
        }
        at += 4;
    }

    if kind == PageType::InteriorTable {
        let (key, n) = varint_at(page, at, usable, "interior key")?;
        return Ok(CellMeta {
            offset,
            left_child,
            rowid: Some(key as i64),
            payload_total: None,
            payload_start: at + n,
            local: 0,
            overflow_page: None,
            end: at + n,
        });
    }

    let (total, n) = varint_at(page, at, usable, "payload size")?;
    at += n;
    let total = match usize::try_from(total) {
        Ok(t) if t <= max_payload => t,
        _ => {
            return Err(corrupt(
                CorruptionKind::CellPointer,
                "payload is larger than the database",
            )
            .with_offset(offset)
            .with_expected(format!("at most {max_payload} bytes"), total))
        }
    };
    let mut rowid = None;
    if kind == PageType::LeafTable {
        let (id, n) = varint_at(page, at, usable, "rowid")?;
        rowid = Some(id as i64);
        at += n;
    }

    let local = local_payload(total, usable, kind == PageType::LeafTable);
    let local_end = at.checked_add(local).filter(|&e| e <= usable);
    let Some(local_end) = local_end else {
        return Err(
            corrupt(CorruptionKind::CellPointer, "payload extends past the page")
                .with_offset(at)
                .with_expected(format!("at most {usable}"), at.saturating_add(local)),
        );
    };
    let (overflow_page, end) = if local < total {
        match be32(page, local_end).filter(|_| local_end + OVERFLOW_POINTER_LEN <= usable) {
            Some(p) => (Some(p), local_end + OVERFLOW_POINTER_LEN),
            None => {
                return Err(corrupt(
                    CorruptionKind::Overflow,
                    "overflow pointer extends past the page",
                )
                .with_offset(local_end))
            }
        }
    } else {
        (None, local_end)
    };
    Ok(CellMeta {
        offset,
        left_child,
        rowid,
        payload_total: Some(total),
        payload_start: at,
        local,
        overflow_page,
        end,
    })
}

/// Payload bytes: a view into the cached page when the cell has no overflow (no copy), or an
/// owned buffer assembled from the overflow chain.
pub(crate) enum Bytes {
    Page {
        page: Arc<[u8]>,
        start: usize,
        end: usize,
    },
    Owned(Vec<u8>),
}

impl std::ops::Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Page { page, start, end } => &page[*start..*end],
            Bytes::Owned(v) => v,
        }
    }
}

/// A cell's complete payload.
pub(crate) struct Payload {
    pub bytes: Bytes,
    /// Overflow pages that were read, in chain order.
    pub overflow: Vec<u32>,
    /// The next-page pointer of the last overflow page read; 0 in a well-formed chain.
    pub tail: u32,
}

/// Assembles a payload from the local part and its overflow chain.
pub(crate) fn read_payload<R: Read + Seek>(
    pager: &mut Pager<R>,
    page: &Arc<[u8]>,
    meta: &CellMeta,
) -> Result<Payload> {
    let total = meta.payload_total.expect("cell has a payload");
    if meta.overflow_page.is_none() {
        let (start, end) = (meta.payload_start, meta.payload_start + total);
        return Ok(Payload {
            bytes: Bytes::Page {
                page: Arc::clone(page),
                start,
                end,
            },
            overflow: Vec::new(),
            tail: 0,
        });
    }
    let usable = pager.usable();
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&page[meta.payload_start..meta.payload_start + meta.local]);
    let mut overflow = Vec::new();

    let mut next = meta.overflow_page;
    let mut seen = HashSet::new();
    let mut previous: Option<u32> = None;
    while bytes.len() < total {
        let Some(current) = next.filter(|&n| n != 0) else {
            return Err(corrupt(
                CorruptionKind::Overflow,
                "overflow chain ends before the payload is complete",
            )
            .with_expected(format!("{total} bytes"), bytes.len()));
        };
        if current > pager.page_count() {
            return Err(corrupt(
                CorruptionKind::Overflow,
                format!("overflow chain points to page {current} outside the database"),
            )
            .with_expected(format!("1..={}", pager.page_count()), current));
        }
        if previous == Some(current) {
            return Err(
                corrupt(CorruptionKind::Overflow, "overflow page points to itself")
                    .with_page(current),
            );
        }
        if !seen.insert(current) {
            return Err(
                corrupt(CorruptionKind::Overflow, "overflow chain contains a cycle")
                    .with_page(current),
            );
        }
        let data = pager.read_page(current)?;
        let following = be32(&data, 0).expect("pages are at least 512 bytes");
        let take = (total - bytes.len()).min(usable - OVERFLOW_POINTER_LEN);
        bytes.extend_from_slice(&data[OVERFLOW_POINTER_LEN..OVERFLOW_POINTER_LEN + take]);
        overflow.push(current);
        previous = Some(current);
        next = Some(following);
    }
    let tail = if overflow.is_empty() {
        0
    } else {
        next.unwrap_or(0)
    };
    Ok(Payload {
        bytes: Bytes::Owned(bytes),
        overflow,
        tail,
    })
}

/// Parses a cell and assembles its payload, attaching page and cell context to any error.
pub(crate) fn read_cell<R: Read + Seek>(
    pager: &mut Pager<R>,
    page: &Arc<[u8]>,
    number: u32,
    index: usize,
    kind: PageType,
    offset: usize,
) -> Result<(CellMeta, Payload)> {
    let usable = pager.usable();
    let max = max_payload(pager);
    let ctx = |e: crate::error::Error| e.with_page(number).with_cell(index);
    let meta = cell_meta(page, usable, kind, offset, max).map_err(ctx)?;
    let payload = read_payload(pager, page, &meta).map_err(|e| {
        let e = ctx(e);
        match meta.rowid {
            Some(r) => e.with_rowid(r),
            None => e,
        }
    })?;
    Ok((meta, payload))
}

/// Largest payload a cell can claim: nothing larger fits in the database.
pub(crate) fn max_payload<R: Read + Seek>(pager: &Pager<R>) -> usize {
    (pager.page_count() as usize).saturating_mul(pager.usable())
}

/// Returns an error unless `node` is a table page; used when a table b-tree is expected.
pub(crate) fn expect_table(node: &Node) -> Result<()> {
    expect_kind(node, true)
}

/// Returns an error unless `node` is an index page.
pub(crate) fn expect_index(node: &Node) -> Result<()> {
    expect_kind(node, false)
}

fn expect_kind(node: &Node, table: bool) -> Result<()> {
    if node.page_type().is_table() == table {
        return Ok(());
    }
    let number = match node {
        Node::InteriorTable { number, .. }
        | Node::LeafTable { number, .. }
        | Node::InteriorIndex { number, .. }
        | Node::LeafIndex { number, .. } => *number,
    };
    Err(corrupt(
        CorruptionKind::PageType,
        format!(
            "expected a {} b-tree page",
            if table { "table" } else { "index" }
        ),
    )
    .with_page(number)
    .with_expected(
        if table { "table page" } else { "index page" },
        node.page_type().name(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_leaf_split_matches_the_documented_formula() {
        // usable = 4096: X = 4061, M = 489. P=4062 -> K = 489 + (4062-489) % 4092 = 4062 > X, so M.
        assert_eq!(local_payload(100, 4096, true), 100);
        assert_eq!(local_payload(4061, 4096, true), 4061);
        assert_eq!(local_payload(4062, 4096, true), 489);
        // P=5000 -> K = 489 + 4511 % 4092 = 908 <= X
        assert_eq!(local_payload(5000, 4096, true), 908);
    }

    #[test]
    fn index_split_uses_the_smaller_maximum() {
        // usable = 4096: X = 4084*64/255 - 23 = 1002, M = 489.
        assert_eq!(local_payload(1002, 4096, false), 1002);
        assert_eq!(local_payload(1003, 4096, false), 489);
        // P=2000 -> K = 489 + (2000-489) % 4092 = 2000 > X, so M.
        assert_eq!(local_payload(2000, 4096, false), 489);
    }
}
