//! Deterministic stand-in for fuzzing: the same invariant functions the cargo-fuzz targets call
//! (tests/common/invariants.rs), driven by seeded mutations of real databases. This does not
//! replace coverage-guided fuzzing; it makes the invariants part of every `cargo test`.

mod common;

#[allow(dead_code)]
mod invariants {
    include!("common/invariants.rs");
}

use common::corpus::{base_db, PAGE_SIZE};
use common::*;
use rusqlite::types::Value as SqlValue;

const INTERESTING_U32: [u32; 12] = [
    0,
    1,
    2,
    3,
    0x7f,
    0xff,
    0x1ff,
    0xffff,
    0x1_0000,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_ffff,
];
const INTERESTING_U16: [u16; 8] = [0, 1, 4, 0x7f, 0x1ff, 0x200, 0x8000, 0xffff];

fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let build = |name: &'static str, setup: &str, fill: &dyn Fn(&rusqlite::Connection)| {
        let tmp = TempDb::new();
        let conn = tmp.connect();
        conn.execute_batch(setup).unwrap();
        fill(&conn);
        drop(conn);
        (name, std::fs::read(tmp.path()).unwrap())
    };
    let fill = |conn: &rusqlite::Connection| {
        let mut rng = Rng(11);
        conn.execute_batch("BEGIN").unwrap();
        for i in 0..150i64 {
            insert(
                conn,
                "INSERT INTO t VALUES (?,?,?)",
                vec![
                    SqlValue::Integer(i),
                    gen_any(&mut rng),
                    SqlValue::Text(gen_text(&mut rng)),
                ],
            );
        }
        conn.execute_batch("COMMIT; DELETE FROM t WHERE id % 3 = 0;")
            .unwrap();
    };
    vec![
        ("base", base_db().0),
        build("rowid+index", "PRAGMA page_size=512; CREATE TABLE t (id INTEGER PRIMARY KEY, a, b TEXT); CREATE INDEX ta ON t(a); CREATE INDEX tb ON t(b, a);", &fill),
        build("without-rowid", "PRAGMA page_size=512; CREATE TABLE t (id, a, b TEXT, PRIMARY KEY (id)) WITHOUT ROWID;", &fill),
        build("utf16", "PRAGMA encoding='UTF-16be'; PRAGMA page_size=512; CREATE TABLE t (id INTEGER PRIMARY KEY, a, b TEXT); CREATE INDEX tb ON t(b);", &fill),
        build("autovacuum", "PRAGMA auto_vacuum=FULL; PRAGMA page_size=512; CREATE TABLE t (id INTEGER PRIMARY KEY, a, b TEXT);", &fill),
    ]
}

fn boundary_u32(rng: &mut Rng, page_count: u32) -> u32 {
    match rng.below(4) {
        0 => INTERESTING_U32[rng.below(12) as usize],
        1 => page_count + 1,
        2 => rng.below(u64::from(page_count) + 2) as u32,
        _ => rng.next() as u32,
    }
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    if at + 4 <= b.len() {
        b[at..at + 4].copy_from_slice(&v.to_be_bytes());
    }
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    if at + 2 <= b.len() {
        b[at..at + 2].copy_from_slice(&v.to_be_bytes());
    }
}

/// Applies one randomly chosen structural mutation. Returns its name for the statistics.
fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>) -> &'static str {
    if bytes.len() < 1024 {
        // Already truncated below a plausible header plus page; further structural edits would
        // only index outside the file.
        return "random bytes";
    }
    let ps = u16::from_be_bytes([bytes[16], bytes[17]]);
    let page_size = if ps == 1 {
        65_536
    } else {
        usize::from(ps).max(512)
    };
    let pages = (bytes.len() / page_size).max(1) as u32;
    let page_at = |p: u32| (p as usize - 1) * page_size;
    let random_page = |rng: &mut Rng| 1 + rng.below(u64::from(pages)) as u32;
    match rng.below(10) {
        0 => {
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(bytes.len() as u64) as usize;
                bytes[at] = rng.next() as u8;
            }
            "random bytes"
        }
        1 => {
            bytes.truncate(rng.below(bytes.len() as u64) as usize);
            "truncation"
        }
        2 => {
            let at = 16 + rng.below(84) as usize;
            if rng.chance(50) {
                put32(bytes, at & !3, boundary_u32(rng, pages));
            } else {
                bytes[at] = rng.next() as u8;
            }
            "header field"
        }
        3 => {
            let at = page_at(random_page(rng)) + if rng.chance(20) { 100 } else { 0 };
            match rng.below(5) {
                0 => bytes[at] = [0x02, 0x05, 0x0a, 0x0d, 0x00, 0xff][rng.below(6) as usize],
                1 => put16(bytes, at + 3, INTERESTING_U16[rng.below(8) as usize]),
                2 => put16(bytes, at + 5, INTERESTING_U16[rng.below(8) as usize]),
                3 => put16(bytes, at + 1, INTERESTING_U16[rng.below(8) as usize]),
                _ => put32(bytes, at + 8, boundary_u32(rng, pages)),
            }
            "page header"
        }
        4 => {
            let at = page_at(random_page(rng)) + 8 + 2 * rng.below(40) as usize;
            put16(bytes, at, INTERESTING_U16[rng.below(8) as usize]);
            "cell pointer"
        }
        5 => {
            let at = page_at(random_page(rng)) + 4 * rng.below((page_size / 4) as u64) as usize;
            put32(bytes, at, boundary_u32(rng, pages));
            "page reference"
        }
        6 => {
            let at = page_at(random_page(rng)) + rng.below(page_size as u64 - 16) as usize;
            let run = 1 + rng.below(9) as usize;
            let fill = if rng.chance(70) { 0xff } else { 0x80 };
            for b in bytes[at..at + run].iter_mut() {
                *b = fill;
            }
            "varint stomp"
        }
        7 => {
            let at = page_at(random_page(rng));
            let fill = [0x00, 0xff, 0x0d, 0x05][rng.below(4) as usize];
            let end = (at + page_size).min(bytes.len());
            bytes[at..end].fill(fill);
            "page fill"
        }
        8 => {
            let (a, b) = (random_page(rng), random_page(rng));
            if a != b && page_at(a.max(b)) + page_size <= bytes.len() {
                for i in 0..page_size {
                    bytes.swap(page_at(a) + i, page_at(b) + i);
                }
            }
            "page swap"
        }
        _ => {
            // overflow-targeted: absurd payload length in the first cell of a random leaf
            let at = page_at(random_page(rng));
            let cell = usize::from(u16::from_be_bytes([bytes[at + 8], bytes[at + 9]]));
            if cell > 0 && at + cell + 9 <= bytes.len() {
                bytes[at + cell..at + cell + 5].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x7f]);
            }
            "payload length"
        }
    }
}

#[test]
fn mutated_databases_never_break_the_invariants() {
    let seeds = seeds();
    let mut rng = Rng(0xF00D_CAFE);
    let mut by_strategy: std::collections::BTreeMap<&str, (u32, u32)> = Default::default();
    let mut reached_schema = 0u32;
    const ITERATIONS: u32 = 6_000;
    for i in 0..ITERATIONS {
        let (name, seed) = &seeds[(i as usize) % seeds.len()];
        let mut bytes = seed.clone();
        let rounds = 1 + rng.below(3);
        let mut applied = Vec::new();
        for _ in 0..rounds {
            applied.push(mutate(&mut rng, &mut bytes));
        }
        // the corpus of 10 strategies must all appear even when a truncation cut the file short
        if bytes.len() < 1024 {
            bytes = seed.clone();
            applied = vec![mutate(&mut rng, &mut bytes)];
        }
        let outcome = std::panic::catch_unwind(|| {
            invariants::database(&bytes);
            litewalk::Database::from_bytes(bytes.clone())
                .and_then(|mut d| d.schema())
                .is_ok()
        });
        match outcome {
            Ok(schema_ok) => {
                reached_schema += u32::from(schema_ok);
                for s in applied {
                    let e = by_strategy.entry(s).or_default();
                    e.0 += 1;
                    e.1 += u32::from(schema_ok);
                }
            }
            Err(_) => {
                let path = std::env::temp_dir().join(format!("litewalk-crash-{i}.db"));
                std::fs::write(&path, &bytes).unwrap();
                panic!("invariant violated on iteration {i} (seed {name}, mutations {applied:?}); input saved to {}", path.display());
            }
        }
    }
    eprintln!("mutation strategies: (applied, schema still readable)");
    for (s, (n, ok)) in &by_strategy {
        eprintln!("  {s:<16} {n:>6} {ok:>6}");
    }
    assert!(reached_schema > ITERATIONS / 5, "only {reached_schema} of {ITERATIONS} inputs got past the header; the mutations are too destructive to test anything");
    assert!(
        by_strategy.len() == 10,
        "every strategy must be exercised: {by_strategy:?}"
    );
    let _ = PAGE_SIZE;
}

#[test]
fn random_bytes_and_synthetic_headers_never_break_the_invariants() {
    let mut rng = Rng(0xBAD_5EED);
    let (valid, _) = (base_db().0, ());
    for _ in 0..4000 {
        let len = rng.below(3000) as usize;
        let mut data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        invariants::header(&data);
        invariants::varint(&data);
        invariants::record(&data);
        invariants::wal(&data);
        invariants::database(&data);
        // The same bytes behind a valid header and database layout reach the b-tree code.
        if data.len() >= 100 {
            let mut synthetic = valid[..PAGE_SIZE.min(valid.len())].to_vec();
            synthetic.extend_from_slice(&data);
            synthetic.resize(synthetic.len().next_multiple_of(PAGE_SIZE), 0);
            if rng.chance(50) {
                synthetic[100] = [0x0d, 0x05, 0x0a, 0x02][rng.below(4) as usize];
            }
            invariants::database(&synthetic);
        }
        data.clear();
    }
}

#[test]
fn record_decoder_survives_every_two_byte_prefix_of_crafted_headers() {
    // Exhaustive over the interesting corner: header size and first serial type, any body.
    for header_len in 0..=u8::MAX {
        for first in 0..=u8::MAX {
            for body in [&[][..], &[0xff; 3][..], &[0u8; 40][..]] {
                let mut rec = vec![header_len, first];
                rec.extend_from_slice(body);
                invariants::record(&rec);
            }
        }
    }
}
