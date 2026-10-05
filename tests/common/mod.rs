#![allow(dead_code)]

pub mod corpus;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use litewalk::Value;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, Connection};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A database file in the temp directory, removed on drop.
pub struct TempDb(pub PathBuf);

impl TempDb {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        TempDb(std::env::temp_dir().join(format!("litewalk-test-{}-{n}.db", std::process::id())))
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn connect(&self) -> Connection {
        Connection::open(&self.0).expect("open sqlite")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = self.0.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}

/// xorshift64*: deterministic, dependency-free.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

pub fn gen_int(r: &mut Rng) -> i64 {
    match r.below(12) {
        0 => 0,
        1 => 1,
        2 => -1,
        3 => i64::MAX,
        4 => i64::MIN,
        5 => r.below(200) as i64 - 100,
        6 => r.below(60_000) as i64 - 30_000,
        7 => r.below(1 << 24) as i64 - (1 << 23),
        8 => r.next() as i32 as i64,
        9 => (r.next() as i64) >> 16, // up to 48 bits
        _ => r.next() as i64,
    }
}

fn gen_len(r: &mut Rng) -> usize {
    match r.below(1000) {
        0..=2 => 20_000 + r.below(80_000) as usize, // forces long overflow chains
        3..=40 => 500 + r.below(6_000) as usize,
        _ => r.below(60) as usize,
    }
}

pub fn gen_text(r: &mut Rng) -> String {
    const ALPHABET: [char; 12] = [
        'a', 'Z', '0', ' ', '\n', '\t', '\\', 'é', 'ß', '漢', '😀', '\'',
    ];
    (0..gen_len(r))
        .map(|_| ALPHABET[r.below(12) as usize])
        .collect()
}

pub fn gen_blob(r: &mut Rng) -> Vec<u8> {
    (0..gen_len(r)).map(|_| r.next() as u8).collect()
}

pub fn gen_real(r: &mut Rng) -> f64 {
    loop {
        let f = match r.below(6) {
            0 => 0.0,
            1 => -0.0,
            2 => r.below(1000) as f64 / 8.0,
            3 => f64::MAX,
            _ => f64::from_bits(r.next()),
        };
        if !f.is_nan() {
            return f;
        }
    }
}

pub fn gen_any(r: &mut Rng) -> SqlValue {
    match r.below(5) {
        0 => SqlValue::Null,
        1 => SqlValue::Integer(gen_int(r)),
        2 => SqlValue::Real(gen_real(r)),
        3 => SqlValue::Text(gen_text(r)),
        _ => SqlValue::Blob(gen_blob(r)),
    }
}

pub fn insert(conn: &Connection, sql: &str, values: Vec<SqlValue>) {
    conn.execute(sql, params_from_iter(values)).expect("insert");
}

pub fn from_sql(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef as V;
    match v {
        V::Null => Value::Null,
        V::Integer(i) => Value::Integer(i),
        V::Real(f) => Value::Real(f),
        V::Text(t) => Value::Text(String::from_utf8(t.to_vec()).expect("utf-8")),
        V::Blob(b) => Value::Blob(b.to_vec()),
    }
}

/// Bit-exact comparison (distinguishes 0.0 from -0.0).
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Real(x), Value::Real(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

pub fn reference_rows(conn: &Connection, table: &str, ncols: usize) -> Vec<(i64, Vec<Value>)> {
    let mut stmt = conn
        .prepare(&format!("SELECT rowid, * FROM \"{table}\" ORDER BY rowid"))
        .expect("prepare");
    stmt.query_map([], |row| {
        let rowid: i64 = row.get(0)?;
        let vals = (0..ncols)
            .map(|i| from_sql(row.get_ref(i + 1).expect("column")))
            .collect();
        Ok((rowid, vals))
    })
    .expect("query")
    .map(|r| r.expect("row"))
    .collect()
}

pub fn query_values(conn: &Connection, sql: &str, ncols: usize) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    stmt.query_map([], |row| {
        Ok((0..ncols)
            .map(|i| from_sql(row.get_ref(i).expect("column")))
            .collect())
    })
    .expect("query")
    .map(|r| r.expect("row"))
    .collect()
}

pub fn assert_same_rows(actual: &[Vec<Value>], expected: &[Vec<Value>], what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}: row count");
    for (n, (a, e)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(a.len(), e.len(), "{what}: column count in row {n}");
        for (i, (x, y)) in a.iter().zip(e).enumerate() {
            assert!(
                same(x, y),
                "{what}: row {n} column {i}: litewalk {x:?}, sqlite {y:?}"
            );
        }
    }
}
