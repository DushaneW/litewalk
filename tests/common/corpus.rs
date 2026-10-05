//! Deterministic corruption corpus: one valid database with a known layout, plus byte patches
//! that each break exactly one rule of the file format.

#![allow(dead_code)]

use litewalk::{Database, PageType, Section, Usage};

use super::TempDb;

pub const PAGE_SIZE: usize = 512;

/// Page numbers of structures inside [`base_db`].
pub struct Layout {
    pub first_leaf: u32,
    pub second_leaf: u32,
    pub interior: u32,
    pub overflow_chain: Vec<u32>,
    pub index_leaf: u32,
    pub freelist_trunk: u32,
}

/// A 512-byte-page database with a table `t` (small rows first, then 1500-byte values that
/// need three-page overflow chains), an index on `v`, and a freelist left by deletes.
pub fn base_db() -> (Vec<u8>, Layout) {
    let tmp = TempDb::new();
    let conn = tmp.connect();
    conn.pragma_update(None, "page_size", PAGE_SIZE as u32)
        .unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); CREATE INDEX t_v ON t(v); CREATE TABLE junk (x);").unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 1..=120i64 {
        let v = if i <= 50 {
            format!("s{i}")
        } else if i % 20 == 0 {
            "x".repeat(1500)
        } else {
            format!("row-{i}-{}", "y".repeat(30))
        };
        conn.execute("INSERT INTO t VALUES (?1, ?2)", rusqlite::params![i, v])
            .unwrap();
    }
    for i in 0..200 {
        conn.execute(
            "INSERT INTO junk VALUES (?1)",
            [format!("{i}{}", "z".repeat(100))],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT; DELETE FROM junk;").unwrap();
    drop(conn);
    let bytes = std::fs::read(tmp.path()).unwrap();

    let mut db = Database::from_bytes(bytes.clone()).unwrap();
    assert!(
        db.header().freelist_count > 0,
        "the base database must have a freelist"
    );
    let map = db.map().unwrap();
    let leaves: Vec<u32> = map
        .iter()
        .filter(|e| {
            matches!(&e.usage, Usage::Table(n) if &**n == "t")
                && e.page_type == Some(PageType::LeafTable)
        })
        .map(|e| e.page)
        .collect();
    let interior = map
        .iter()
        .find(|e| {
            matches!(&e.usage, Usage::Table(n) if &**n == "t")
                && e.page_type == Some(PageType::InteriorTable)
        })
        .expect("t has an interior page")
        .page;
    let overflow_chain: Vec<u32> = map
        .iter()
        .filter(|e| matches!(&e.usage, Usage::Overflow { object, .. } if &**object == "t"))
        .map(|e| e.page)
        .collect();
    let index_leaf = map
        .iter()
        .find(|e| {
            matches!(&e.usage, Usage::Index { .. }) && e.page_type == Some(PageType::LeafIndex)
        })
        .expect("index leaf")
        .page;
    // The leaf with the smallest rowids is the one the interior page lists first.
    let first_by_rowid = {
        let mut best = (i64::MAX, 0);
        for &l in &leaves {
            let info = db.page(l).unwrap();
            let low = info.cells.iter().filter_map(|c| c.rowid).min().unwrap();
            if low < best.0 {
                best = (low, l);
            }
        }
        best.1
    };
    let second_leaf = *leaves.iter().find(|&&l| l != first_by_rowid).unwrap();
    let freelist_trunk = db.header().freelist_trunk;
    assert!(
        overflow_chain.len() >= 3,
        "expected an overflow chain of at least three pages"
    );
    (
        bytes,
        Layout {
            first_leaf: first_by_rowid,
            second_leaf,
            interior,
            overflow_chain,
            index_leaf,
            freelist_trunk,
        },
    )
}

pub fn patch(bytes: &mut [u8], page: u32, offset: usize, data: &[u8]) {
    let at = (page as usize - 1) * PAGE_SIZE + offset;
    bytes[at..at + data.len()].copy_from_slice(data);
}

pub fn page_bytes(bytes: &[u8], page: u32) -> &[u8] {
    &bytes[(page as usize - 1) * PAGE_SIZE..page as usize * PAGE_SIZE]
}

pub struct Case {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    /// The check section that must report an issue.
    pub section: Section,
}

/// Every deterministic corruption, each with the check section expected to catch it.
pub fn cases() -> Vec<Case> {
    let (base, l) = base_db();
    let mut out = Vec::new();
    let mut add = |name: &'static str, section: Section, f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = base.clone();
        f(&mut b);
        out.push(Case {
            name,
            bytes: b,
            section,
        });
    };
    let cell_offset = |bytes: &[u8], page: u32, cell: usize| -> usize {
        let p = page_bytes(bytes, page);
        let pointers = if p[0] == 0x05 || p[0] == 0x02 { 12 } else { 8 };
        usize::from(u16::from_be_bytes([
            p[pointers + 2 * cell],
            p[pointers + 2 * cell + 1],
        ]))
    };

    // --- header
    add("bad_magic", Section::Header, &|b| b[0] = b'X');
    add("invalid_page_size", Section::Header, &|b| {
        b[16..18].copy_from_slice(&[0, 3])
    });
    add("bad_payload_fractions", Section::Header, &|b| b[21] = 1);
    add("reserved_bytes_too_large", Section::Header, &|b| {
        b[20] = 200
    });
    add("invalid_text_encoding", Section::Header, &|b| {
        b[56..60].copy_from_slice(&9u32.to_be_bytes())
    });
    add("truncated_file", Section::PageBoundaries, &|b| {
        b.truncate(b.len() / 2)
    });
    add("bad_schema_format", Section::Header, &|b| {
        b[44..48].copy_from_slice(&9u32.to_be_bytes())
    });

    // --- page layout
    add("page_type_invalid", Section::PageTypes, &|b| {
        patch(b, l.first_leaf, 0, &[0x07])
    });
    add("cell_pointer_out_of_range", Section::CellPointers, &|b| {
        patch(b, l.first_leaf, 8, &[0xff, 0xff])
    });
    add("cell_pointers_overlap", Section::CellPointers, &|b| {
        let off = cell_offset(b, l.first_leaf, 0);
        patch(b, l.first_leaf, 10, &(off as u16).to_be_bytes());
    });
    add("cell_count_too_large", Section::CellPointers, &|b| {
        patch(b, l.first_leaf, 3, &[0x01, 0xf0])
    });
    add("fragmented_bytes_excessive", Section::CellPointers, &|b| {
        patch(b, l.first_leaf, 7, &[200])
    });
    add("content_area_start_invalid", Section::CellPointers, &|b| {
        patch(b, l.first_leaf, 5, &[0x00, 0x01])
    });
    add("freeblock_cycle", Section::CellPointers, &|b| {
        // A free block at offset 400 whose next pointer is itself.
        patch(b, l.second_leaf, 1, &[0x01, 0x90]);
        patch(b, l.second_leaf, 400, &[0x01, 0x90, 0x00, 0x04]);
    });
    add(
        "varint_truncated_at_page_end",
        Section::RecordPayloads,
        &|b| {
            patch(b, l.first_leaf, 8, &[0x01, 0xff]); // cell 0 now starts at the last byte
            patch(b, l.first_leaf, PAGE_SIZE - 1, &[0xff]);
        },
    );

    // --- b-tree structure
    let mut b = base.clone();
    let off = cell_offset(&b, l.interior, 0);
    patch(&mut b, l.interior, off, &999_999u32.to_be_bytes());
    out.push(Case {
        name: "interior_child_out_of_range",
        bytes: b,
        section: Section::PageBoundaries,
    });
    let mut b = base.clone();
    patch(&mut b, l.interior, off, &l.interior.to_be_bytes());
    out.push(Case {
        name: "interior_child_points_to_itself",
        bytes: b,
        section: Section::BTreeStructure,
    });
    let mut b = base.clone();
    patch(&mut b, l.interior, off + 4, &[0x00]); // first key becomes 0, below every rowid it covers
    out.push(Case {
        name: "interior_key_too_small",
        bytes: b,
        section: Section::BTreeOrdering,
    });
    let mut b = base.clone();
    let c0 = cell_offset(&b, l.first_leaf, 0);
    let c1 = cell_offset(&b, l.first_leaf, 1);
    let rowid0 = page_bytes(&b, l.first_leaf)[c0 + 1];
    patch(&mut b, l.first_leaf, c1 + 1, &[rowid0]); // cell 1 repeats cell 0's rowid
    out.push(Case {
        name: "rowids_out_of_order",
        bytes: b,
        section: Section::BTreeOrdering,
    });
    let mut b = base.clone();
    patch(&mut b, l.first_leaf, c0 + 2, &[0x7f]); // record header size larger than the payload
    out.push(Case {
        name: "record_header_oversized",
        bytes: b,
        section: Section::RecordPayloads,
    });

    // --- overflow chains
    // The first page of a chain is the one no other overflow page points to.
    let next = |b: &[u8], p: u32| u32::from_be_bytes(page_bytes(b, p)[..4].try_into().unwrap());
    let start = *l
        .overflow_chain
        .iter()
        .find(|&&p| !l.overflow_chain.iter().any(|&q| next(&base, q) == p))
        .unwrap();
    let second = u32::from_be_bytes(page_bytes(&base, start)[..4].try_into().unwrap());
    let third = u32::from_be_bytes(page_bytes(&base, second)[..4].try_into().unwrap());
    let mut b = base.clone();
    patch(&mut b, start, 0, &start.to_be_bytes());
    out.push(Case {
        name: "overflow_points_to_itself",
        bytes: b,
        section: Section::OverflowChains,
    });
    let mut b = base.clone();
    patch(&mut b, third, 0, &start.to_be_bytes());
    out.push(Case {
        name: "overflow_cycle",
        bytes: b,
        section: Section::OverflowChains,
    });
    let mut b = base.clone();
    patch(&mut b, start, 0, &999_999u32.to_be_bytes());
    out.push(Case {
        name: "overflow_out_of_range",
        bytes: b,
        section: Section::OverflowChains,
    });
    let mut b = base.clone();
    patch(&mut b, second, 0, &0u32.to_be_bytes());
    out.push(Case {
        name: "overflow_chain_ends_early",
        bytes: b,
        section: Section::OverflowChains,
    });
    let mut b = base.clone();
    patch(&mut b, start, 0, &l.interior.to_be_bytes());
    out.push(Case {
        name: "overflow_points_into_a_btree",
        bytes: b,
        section: Section::OverflowChains,
    });

    // --- freelist
    let mut b = base.clone();
    b[32..36].copy_from_slice(&999_999u32.to_be_bytes());
    out.push(Case {
        name: "freelist_trunk_out_of_range",
        bytes: b,
        section: Section::Freelist,
    });
    let mut b = base.clone();
    let count = u32::from_be_bytes(b[36..40].try_into().unwrap());
    b[36..40].copy_from_slice(&(count + 5).to_be_bytes());
    out.push(Case {
        name: "freelist_count_wrong",
        bytes: b,
        section: Section::Freelist,
    });
    let mut b = base.clone();
    patch(&mut b, l.freelist_trunk, 4, &0x00ff_ffffu32.to_be_bytes());
    out.push(Case {
        name: "freelist_leaf_count_absurd",
        bytes: b,
        section: Section::Freelist,
    });
    let mut b = base.clone();
    patch(&mut b, l.freelist_trunk, 0, &l.freelist_trunk.to_be_bytes());
    out.push(Case {
        name: "freelist_trunk_cycle",
        bytes: b,
        section: Section::Freelist,
    });
    let mut b = base.clone();
    b[32..40].copy_from_slice(&[0; 8]); // forget the freelist: its pages become unreferenced
    out.push(Case {
        name: "freelist_forgotten",
        bytes: b,
        section: Section::PageUsage,
    });

    // --- index pages
    let mut b = base.clone();
    patch(&mut b, l.index_leaf, 0, &[0x0d]); // an index page that claims to be a table page
    out.push(Case {
        name: "index_page_has_table_type",
        bytes: b,
        section: Section::PageTypes,
    });
    out
}

/// Cases that need SQLite's `writable_schema` to produce a schema that lies about the file.
pub fn schema_cases() -> Vec<Case> {
    let make = |sql: &str| -> Vec<u8> {
        let tmp = TempDb::new();
        let conn = tmp.connect();
        conn.execute_batch("CREATE TABLE a (x); CREATE TABLE b (y); CREATE INDEX a_x ON a(x); INSERT INTO a VALUES (1),(2); INSERT INTO b VALUES (3);").unwrap();
        conn.execute_batch(&format!(
            "PRAGMA writable_schema=ON; {sql}; PRAGMA writable_schema=OFF;"
        ))
        .unwrap();
        drop(conn);
        std::fs::read(tmp.path()).unwrap()
    };
    vec![
        Case { name: "schema_root_out_of_range", bytes: make("UPDATE sqlite_schema SET rootpage = 999 WHERE name = 'a'"), section: Section::PageBoundaries },
        Case { name: "schema_two_objects_share_a_root", bytes: make("UPDATE sqlite_schema SET rootpage = (SELECT rootpage FROM sqlite_schema WHERE name = 'a') WHERE name = 'b'"), section: Section::BTreeStructure },
        Case { name: "schema_index_of_unknown_table", bytes: make("UPDATE sqlite_schema SET tbl_name = 'ghost' WHERE name = 'a_x'"), section: Section::Schema },
        Case { name: "schema_table_without_root", bytes: make("UPDATE sqlite_schema SET rootpage = 0 WHERE name = 'b'"), section: Section::Schema },
        Case { name: "schema_duplicate_name", bytes: make("INSERT INTO sqlite_schema (type, name, tbl_name, rootpage, sql) VALUES ('view', 'a', 'a', 0, 'CREATE VIEW a AS SELECT 1')"), section: Section::Schema },
    ]
}
