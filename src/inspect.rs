//! Page-level inspection: decodes one b-tree page without following any pointers.

use crate::btree::{cell_meta, cell_offsets, parse_header, PageType};
use crate::error::Result;

/// A free block inside a b-tree page (a chain of unused regions among the cells).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeBlock {
    /// Offset of the block within the page.
    pub offset: usize,
    /// Size in bytes, including the 4-byte block header.
    pub size: usize,
}

/// One cell on a b-tree page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellInfo {
    /// Position in the cell pointer array.
    pub index: usize,
    /// Offset of the cell within the page.
    pub offset: usize,
    /// Bytes the cell occupies on the page (including the overflow pointer).
    pub size: usize,
    /// Left child page (interior pages).
    pub left_child: Option<u32>,
    /// Rowid (table leaf cells) or largest rowid of the left subtree (interior table cells).
    pub rowid: Option<i64>,
    /// Total payload size, including overflow.
    pub payload_size: Option<usize>,
    /// Payload bytes stored on this page.
    pub local_size: usize,
    /// First overflow page, if the payload spills.
    pub overflow_page: Option<u32>,
    /// Set when the cell could not be decoded.
    pub error: Option<String>,
}

/// A labelled byte range of a page, used to annotate hex dumps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    /// First byte.
    pub start: usize,
    /// One past the last byte.
    pub end: usize,
    /// What the range holds.
    pub label: String,
}

/// Decoded view of one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInfo {
    /// Page number (1-based).
    pub number: u32,
    /// Page size in bytes.
    pub page_size: usize,
    /// Usable bytes (page size minus the reserved region).
    pub usable: usize,
    /// First byte of the page header (100 on page 1, else 0).
    pub header_offset: usize,
    /// The page's first header byte. Meaningful only when `page_type` is `Some`.
    pub type_byte: u8,
    /// B-tree page type; `None` for overflow, freelist, pointer-map and other non-b-tree pages.
    pub page_type: Option<PageType>,
    /// Number of cells.
    pub cell_count: usize,
    /// Offset of the first free block, 0 if none.
    pub first_freeblock: usize,
    /// Start of the cell content area.
    pub content_start: usize,
    /// Fragmented free bytes (too small to be free blocks).
    pub fragmented_bytes: usize,
    /// Rightmost child pointer (interior pages).
    pub rightmost: Option<u32>,
    /// Bytes between the cell pointer array and the cell content area.
    pub unallocated: usize,
    /// Free blocks in chain order.
    pub freeblocks: Vec<FreeBlock>,
    /// `unallocated` + free blocks + fragmented bytes.
    pub free_bytes: usize,
    /// Decoded cells in pointer-array order.
    pub cells: Vec<CellInfo>,
    /// Problems found while decoding this page (the decoding continues past them).
    pub problems: Vec<String>,
}

/// Walks the free block chain. Returns the blocks and a description of the first violation.
pub(crate) fn freeblock_chain(
    page: &[u8],
    first: usize,
    usable: usize,
) -> (Vec<FreeBlock>, Option<String>) {
    let mut blocks: Vec<FreeBlock> = Vec::new();
    let mut at = first;
    while at != 0 {
        if blocks.len() > usable / 4 {
            return (blocks, Some("free block chain does not terminate".into()));
        }
        if at < 4 || at + 4 > usable {
            return (
                blocks,
                Some(format!("free block offset {at} is outside the page")),
            );
        }
        if blocks.last().is_some_and(|b| at <= b.offset) {
            return (
                blocks,
                Some(format!(
                    "free block at {at} does not follow the previous block"
                )),
            );
        }
        let next = usize::from(u16::from_be_bytes([page[at], page[at + 1]]));
        let size = usize::from(u16::from_be_bytes([page[at + 2], page[at + 3]]));
        if size < 4 || at + size > usable {
            return (
                blocks,
                Some(format!("free block at {at} has invalid size {size}")),
            );
        }
        blocks.push(FreeBlock { offset: at, size });
        at = next;
    }
    (blocks, None)
}

/// Decodes `page` (page number `number`). Tolerant: problems are collected in
/// [`PageInfo::problems`] rather than aborting.
pub(crate) fn inspect_page(
    page: &[u8],
    number: u32,
    usable: usize,
    max_payload: usize,
) -> Result<PageInfo> {
    let base = if number == 1 {
        crate::header::HEADER_SIZE
    } else {
        0
    };
    let mut info = PageInfo {
        number,
        page_size: page.len(),
        usable,
        header_offset: base,
        type_byte: page[base],
        page_type: None,
        cell_count: 0,
        first_freeblock: 0,
        content_start: 0,
        fragmented_bytes: 0,
        rightmost: None,
        unallocated: 0,
        freeblocks: Vec::new(),
        free_bytes: 0,
        cells: Vec::new(),
        problems: Vec::new(),
    };
    let Ok(hdr) = parse_header(page, number) else {
        return Ok(info);
    };
    info.page_type = Some(hdr.kind);
    info.cell_count = hdr.ncells;
    info.first_freeblock = hdr.first_freeblock;
    info.content_start = hdr.content_start;
    info.fragmented_bytes = hdr.fragmented;
    info.rightmost = hdr.rightmost;

    let pointers_end = hdr.pointers_end();
    if pointers_end > usable {
        info.problems
            .push("cell pointer array extends past the page".into());
        return Ok(info);
    }
    info.unallocated = hdr.content_start.min(usable).saturating_sub(pointers_end);
    let (blocks, problem) = freeblock_chain(page, hdr.first_freeblock, usable);
    info.problems.extend(problem);
    info.free_bytes =
        info.unallocated + blocks.iter().map(|b| b.size).sum::<usize>() + hdr.fragmented;
    info.freeblocks = blocks;
    if hdr.content_start < pointers_end || hdr.content_start > usable {
        info.problems.push(format!(
            "cell content area starts at {} outside {pointers_end}..={usable}",
            hdr.content_start
        ));
    }

    let offsets = match cell_offsets(page, &hdr, usable, number) {
        Ok(o) => o,
        Err(e) => {
            info.problems.push(e.to_string());
            return Ok(info);
        }
    };
    for (index, &offset) in offsets.iter().enumerate() {
        info.cells.push(
            match cell_meta(page, usable, hdr.kind, offset, max_payload) {
                Ok(m) => CellInfo {
                    index,
                    offset,
                    size: m.end - offset,
                    left_child: m.left_child,
                    rowid: m.rowid,
                    payload_size: m.payload_total,
                    local_size: m.local,
                    overflow_page: m.overflow_page,
                    error: None,
                },
                Err(e) => CellInfo {
                    index,
                    offset,
                    size: 0,
                    left_child: None,
                    rowid: None,
                    payload_size: None,
                    local_size: 0,
                    overflow_page: None,
                    error: Some(e.to_string()),
                },
            },
        );
    }
    Ok(info)
}

impl PageInfo {
    /// Labelled regions of the page in offset order: header, pointer array, free space, free
    /// blocks, cells, reserved bytes.
    pub fn regions(&self) -> Vec<Region> {
        let mut out = Vec::new();
        let mut push = |start: usize, end: usize, label: String| {
            if end > start && end <= self.page_size {
                out.push(Region { start, end, label });
            }
        };
        if self.header_offset > 0 {
            push(0, self.header_offset, "database header".into());
        }
        let Some(kind) = self.page_type else {
            push(
                self.header_offset,
                self.page_size,
                "not a b-tree page".into(),
            );
            return out;
        };
        let header_len = if kind.is_leaf() { 8 } else { 12 };
        let pointers_start = self.header_offset + header_len;
        let pointers_end = pointers_start + 2 * self.cell_count;
        push(self.header_offset, pointers_start, "page header".into());
        push(pointers_start, pointers_end, "cell pointer array".into());
        if self.content_start > pointers_end {
            push(
                pointers_end,
                self.content_start.min(self.usable),
                "unallocated".into(),
            );
        }
        for b in &self.freeblocks {
            push(b.offset, b.offset + b.size, "free block".into());
        }
        for c in &self.cells {
            push(c.offset, c.offset + c.size, format!("cell {}", c.index));
        }
        if self.usable < self.page_size {
            push(self.usable, self.page_size, "reserved".into());
        }
        out.sort_by_key(|r| (r.start, r.end));
        out
    }
}

/// Classic hex dump: offset, 16 bytes, ASCII. Runs of identical lines collapse to `*`.
/// `label` is called with the offset of each printed line to add a trailing annotation.
pub fn hexdump(bytes: &[u8], label: impl Fn(usize) -> String) -> String {
    let mut out = String::new();
    let mut previous: Option<&[u8]> = None;
    let mut collapsed = false;
    for (n, chunk) in bytes.chunks(16).enumerate() {
        let offset = n * 16;
        if previous == Some(chunk) && offset + 16 < bytes.len() {
            if !collapsed {
                out.push_str("*\n");
                collapsed = true;
            }
            continue;
        }
        collapsed = false;
        previous = Some(chunk);
        let hex: Vec<String> = (0..16)
            .map(|i| chunk.get(i).map_or("  ".into(), |b| format!("{b:02x}")))
            .collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        let note = label(offset);
        out.push_str(&format!(
            "{offset:06x}  {} {}  |{ascii}|",
            hex[..8].join(" "),
            hex[8..].join(" ")
        ));
        if !note.is_empty() {
            out.push_str(&format!("  {note}"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hexdump_collapses_repeated_lines() {
        let mut data = vec![0u8; 64];
        data[0] = 0x0d;
        data[63] = 0xff;
        let text = hexdump(&data, |_| String::new());
        assert_eq!(text.lines().count(), 4); // first line, "*", and the last line... plus the zero run marker
        assert!(text.contains("\n*\n"));
        assert!(text.starts_with("000000  0d 00"));
    }

    #[test]
    fn freeblock_chain_detects_cycles_and_bad_sizes() {
        let mut page = vec![0u8; 512];
        // block at 100 -> next 100 (itself), size 8
        page[100..104].copy_from_slice(&[0, 100, 0, 8]);
        let (blocks, problem) = freeblock_chain(&page, 100, 512);
        assert_eq!(blocks.len(), 1);
        assert!(problem.unwrap().contains("does not follow"));
        page[100..104].copy_from_slice(&[0, 0, 0, 2]);
        assert!(freeblock_chain(&page, 100, 512)
            .1
            .unwrap()
            .contains("invalid size"));
    }
}
