#![warn(missing_docs)]
//! Read-only reader for the SQLite database file format, with no dependencies.
//!
//! Reads table and index b-trees (including overflow pages), the schema and records. It does
//! not execute SQL and never writes. See `docs/` in the repository for what is supported.
//!
//! ```no_run
//! use litewalk::Database;
//!
//! # fn main() -> litewalk::Result<()> {
//! let mut db = Database::open("app.db")?;
//! let users = db.table("users")?;
//! for row in db.scan(&users)? {
//!     let row = row?; // the first corruption ends the scan with an error
//!     println!("{:?} {:?}", row.rowid, row.values);
//! }
//! let report = db.check()?;
//! assert!(report.is_valid());
//! # Ok(())
//! # }
//! ```
//!
//! Input that is not a database is an error, never a panic:
//!
//! ```
//! use litewalk::{Database, Error};
//!
//! assert!(matches!(Database::from_bytes(b"hello".to_vec()), Err(Error::NotSqlite)));
//! ```

mod btree;
mod check;
mod db;
mod error;
mod forensic;
mod header;
mod inspect;
mod pager;
mod record;
mod schema;
mod varint;
mod wal;

pub use btree::PageType;
pub use check::{check_reader, CheckReport, Freelist, Issue, MapEntry, Section, Usage, SECTIONS};
pub use db::{
    Database, Index, IndexEntry, IndexScan, ObjectKind, Options, Row, SchemaObject, Table,
    TableScan, DEFAULT_CACHE_PAGES,
};
pub use error::{Corruption, CorruptionKind, Error, Result};
pub use forensic::{ForensicReport, FreeRegion, FreeRegionKind, FreeSpaceSummary, ObjectSize};
pub use header::{AutoVacuum, Encoding, Header};
pub use inspect::{hexdump, CellInfo, FreeBlock, PageInfo, Region};
pub use pager::CacheStats;
pub use record::{decode_record, decode_record_ref, Value, ValueRef};
pub use schema::{affinity_of, Affinity, Column, ColumnDefault, Generated, IndexColumn};
pub use varint::read_varint;
pub use wal::{Wal, WalFrame, WalHeader, WalStop};
