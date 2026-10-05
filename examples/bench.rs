//! Benchmark sweeps for litewalk, with SQLite (via rusqlite) as a reference point.
//!
//!     cargo run --release --example bench -- [scan|lookup|payload|pagesize|cache|all]
//!
//! Each figure is the median of several runs on a warm OS file cache, printed as a Markdown
//! table. A harness this simple has no outlier rejection or statistical analysis (that is why
//! Criterion is not used: it would add a large dev-dependency tree for numbers that are only
//! meaningful on the machine they were taken on). Treat results as orders of magnitude and
//! compare only rows of the same table.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use litewalk::{Database, Options};
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;

const RUNS: usize = 5;

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> Duration {
    median(
        (0..runs)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(f());
                t.elapsed()
            })
            .collect(),
    )
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1e3)
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn make_db(name: &str, rows: i64, payload: usize, page_size: u32) -> (TempFile, u64) {
    let path =
        std::env::temp_dir().join(format!("litewalk-bench-{}-{name}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "page_size", page_size).unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT); BEGIN")
        .unwrap();
    let text = "x".repeat(payload);
    let mut stmt = conn.prepare("INSERT INTO t VALUES (?1, ?2, ?3)").unwrap();
    for i in 0..rows {
        stmt.execute(rusqlite::params![i, i.wrapping_mul(2_654_435_761), text])
            .unwrap();
    }
    drop(stmt);
    conn.execute_batch("COMMIT").unwrap();
    drop(conn);
    let size = std::fs::metadata(&path).unwrap().len();
    (TempFile(path), size)
}

fn scan_litewalk(path: &Path, cache: usize) -> u64 {
    let mut db = Database::open_with(path, Options { cache_pages: cache }).unwrap();
    let t = db.table("t").unwrap();
    db.scan(&t)
        .unwrap()
        .map(|r| r.unwrap().values.len() as u64)
        .sum()
}

fn scan_borrowed(path: &Path, cache: usize) -> u64 {
    let mut db = Database::open_with(path, Options { cache_pages: cache }).unwrap();
    let t = db.table("t").unwrap();
    let mut n = 0;
    db.for_each_record(&t, |_, v| {
        n += v.len() as u64;
        true
    })
    .unwrap();
    n
}

fn scan_sqlite(path: &Path) -> u64 {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn.prepare("SELECT * FROM t").unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut n = 0;
    while let Some(row) = rows.next().unwrap() {
        for i in 0..3 {
            let _: SqlValue = row.get(i).unwrap();
            n += 1;
        }
    }
    n
}

fn probes(rows: i64, count: usize) -> Vec<i64> {
    let mut x = 0x9E37_79B9_7F4A_7C15_u64;
    (0..count)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) % rows as u64) as i64
        })
        .collect()
}

fn lookups_litewalk(path: &Path, cache: usize, ids: &[i64]) -> (Duration, litewalk::CacheStats) {
    let mut db = Database::open_with(path, Options { cache_pages: cache }).unwrap();
    let t = db.table("t").unwrap();
    db.reset_cache_stats();
    let start = Instant::now();
    for &id in ids {
        assert!(db.get(&t, id).unwrap().is_some());
    }
    (start.elapsed(), db.cache_stats())
}

fn lookups_sqlite(path: &Path, ids: &[i64]) -> Duration {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn.prepare("SELECT * FROM t WHERE id = ?1").unwrap();
    let start = Instant::now();
    for &id in ids {
        let _: SqlValue = stmt.query_row([id], |r| r.get(2)).unwrap();
    }
    start.elapsed()
}

fn scan_table() {
    println!("### Full scans (payload 100 B, page size 4096, cache 256 pages)\n");
    println!("| rows | file | litewalk `scan` | litewalk `for_each_record` | SQLite `SELECT *` |\n|---:|---:|---:|---:|---:|");
    for rows in [10_000i64, 100_000, 1_000_000] {
        let (f, size) = make_db("scan", rows, 100, 4096);
        let (a, b, c) = (
            time(RUNS, || scan_litewalk(&f.0, 256)),
            time(RUNS, || scan_borrowed(&f.0, 256)),
            time(RUNS, || scan_sqlite(&f.0)),
        );
        println!(
            "| {rows} | {:.1} MiB | {} | {} | {} |",
            size as f64 / 1_048_576.0,
            ms(a),
            ms(b),
            ms(c)
        );
    }
    println!();
}

fn lookup_table() {
    let rows = 100_000;
    let (f, _) = make_db("lookup", rows, 100, 4096);
    println!("### Lookups by rowid ({rows} rows, payload 100 B, page size 4096)\n");
    println!(
        "| lookups | litewalk, no cache | litewalk, 256 pages | SQLite |\n|---:|---:|---:|---:|"
    );
    for n in [1usize, 100, 1_000, 100_000] {
        let ids = probes(rows, n);
        let runs = if n >= 100_000 { 3 } else { RUNS };
        let cold = median(
            (0..runs)
                .map(|_| lookups_litewalk(&f.0, 0, &ids).0)
                .collect(),
        );
        let warm = median(
            (0..runs)
                .map(|_| lookups_litewalk(&f.0, 256, &ids).0)
                .collect(),
        );
        let sql = median((0..runs).map(|_| lookups_sqlite(&f.0, &ids)).collect());
        let per = |d: Duration| {
            if n >= 1000 {
                format!(
                    "{} ({:.1} µs each)",
                    ms(d),
                    d.as_secs_f64() * 1e6 / n as f64
                )
            } else {
                format!("{:.0} µs total", d.as_secs_f64() * 1e6)
            }
        };
        println!("| {n} | {} | {} | {} |", per(cold), per(warm), per(sql));
    }
    println!();
}

fn payload_table() {
    println!(
        "### Payload size (about 32 MiB of text per table, page size 4096, cache 256 pages)\n"
    );
    println!("| payload | rows | file | litewalk `scan` | litewalk `for_each_record` | SQLite `SELECT *` |\n|---:|---:|---:|---:|---:|---:|");
    for (label, bytes) in [
        ("10 B", 10usize),
        ("1 KB", 1_000),
        ("10 KB", 10_000),
        ("100 KB", 100_000),
        ("1 MB", 1_000_000),
    ] {
        let rows = (32_000_000 / bytes.max(100)).clamp(32, 200_000) as i64;
        let (f, size) = make_db("payload", rows, bytes, 4096);
        let (a, b, c) = (
            time(RUNS, || scan_litewalk(&f.0, 256)),
            time(RUNS, || scan_borrowed(&f.0, 256)),
            time(RUNS, || scan_sqlite(&f.0)),
        );
        println!(
            "| {label} | {rows} | {:.1} MiB | {} | {} | {} |",
            size as f64 / 1_048_576.0,
            ms(a),
            ms(b),
            ms(c)
        );
    }
    println!();
}

fn pagesize_table() {
    println!("### Page size (100,000 rows, payload 100 B, cache 256 pages)\n");
    println!("| page size | file | pages | litewalk `scan` | SQLite `SELECT *` | 10,000 lookups litewalk | 10,000 lookups SQLite |\n|---:|---:|---:|---:|---:|---:|---:|");
    let ids = probes(100_000, 10_000);
    for ps in [512u32, 1024, 2048, 4096, 8192, 16384, 32768, 65536] {
        let (f, size) = make_db("pagesize", 100_000, 100, ps);
        let pages = size / u64::from(ps);
        let (a, c) = (
            time(RUNS, || scan_litewalk(&f.0, 256)),
            time(RUNS, || scan_sqlite(&f.0)),
        );
        let l = median(
            (0..3)
                .map(|_| lookups_litewalk(&f.0, 256, &ids).0)
                .collect(),
        );
        let s = median((0..3).map(|_| lookups_sqlite(&f.0, &ids)).collect());
        println!(
            "| {ps} | {:.1} MiB | {pages} | {} | {} | {} | {} |",
            size as f64 / 1_048_576.0,
            ms(a),
            ms(c),
            ms(l),
            ms(s)
        );
    }
    println!();
}

fn cache_table() {
    let rows = 100_000;
    let (f, size) = make_db("cache", rows, 100, 4096);
    println!(
        "### Page cache capacity (100,000 rows, {} pages of 4096 B, 100,000 random lookups)\n",
        size / 4096
    );
    println!("| cache pages | lookups | hit ratio | full scan |\n|---:|---:|---:|---:|");
    let ids = probes(rows, 100_000);
    for cap in [0usize, 64, 256, 1024, 4096] {
        let runs: Vec<_> = (0..3).map(|_| lookups_litewalk(&f.0, cap, &ids)).collect();
        let d = median(runs.iter().map(|r| r.0).collect());
        let ratio = runs[0]
            .1
            .hit_ratio()
            .map_or("n/a".into(), |r| format!("{:.1}%", r * 100.0));
        let scan = time(RUNS, || scan_litewalk(&f.0, cap));
        println!("| {cap} | {} | {ratio} | {} |", ms(d), ms(scan));
    }
    println!();
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build; run with --release for meaningful numbers");
    }
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let all = which == "all";
    if all || which == "scan" {
        scan_table();
    }
    if all || which == "lookup" {
        lookup_table();
    }
    if all || which == "payload" {
        payload_table();
    }
    if all || which == "pagesize" {
        pagesize_table();
    }
    if all || which == "cache" {
        cache_table();
    }
}
