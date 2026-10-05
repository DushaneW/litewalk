use std::collections::HashSet;
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use crate::btree::{
    cell_meta, expect_index, expect_table, load_node, max_payload, read_cell, Bytes, Node,
    PageType, PageView, MAX_DEPTH,
};
use crate::error::{corrupt, CorruptionKind, Error, Result};
use crate::header::{raw_page_size, Encoding, Header, HEADER_SIZE};
use crate::pager::{CacheStats, Pager};
use crate::record::{decode_record, decode_record_ref, Value, ValueRef};
use crate::schema::{
    parse_create_index, parse_create_table, Affinity, Column, ColumnDefault, IndexColumn,
};
use crate::wal::Wal;

/// Pages kept in the cache unless [`Options`] says otherwise.
pub const DEFAULT_CACHE_PAGES: usize = 256;

/// Settings for opening a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Maximum pages held in the LRU page cache; 0 disables the cache.
    pub cache_pages: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            cache_pages: DEFAULT_CACHE_PAGES,
        }
    }
}

impl Options {
    /// Options with the page cache disabled.
    pub fn no_cache() -> Self {
        Options { cache_pages: 0 }
    }
}

/// Kind of a `sqlite_schema` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// A table.
    Table,
    /// An index.
    Index,
    /// A view.
    View,
    /// A trigger.
    Trigger,
}

impl ObjectKind {
    /// Lowercase name as stored in `sqlite_schema.type`.
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Table => "table",
            ObjectKind::Index => "index",
            ObjectKind::View => "view",
            ObjectKind::Trigger => "trigger",
        }
    }
}

/// One row of `sqlite_schema`.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaObject {
    /// Object kind.
    pub kind: ObjectKind,
    /// Object name.
    pub name: String,
    /// Table the object belongs to (same as `name` for tables).
    pub table_name: String,
    /// Root b-tree page; 0 for views, triggers and virtual tables.
    pub root_page: u32,
    /// The `CREATE` statement; `None` for automatic indexes.
    pub sql: Option<String>,
}

/// A table described by the schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    /// Table name.
    pub name: String,
    /// Root b-tree page.
    pub root_page: u32,
    /// Columns in declaration order.
    pub columns: Vec<Column>,
    /// True for `WITHOUT ROWID` tables (stored in an index-style b-tree, no rowids).
    pub without_rowid: bool,
    /// Set when the table cannot be read, with the reason.
    pub unsupported: Option<String>,
    rowid_alias: Option<usize>,
    pk_columns: Vec<usize>,
}

impl Table {
    /// Column names in declaration order.
    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }
}

/// An index described by the schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Index {
    /// Index name.
    pub name: String,
    /// Indexed table.
    pub table: String,
    /// Root b-tree page.
    pub root_page: u32,
    /// True for `UNIQUE` indexes.
    pub unique: bool,
    /// Indexed columns; empty for automatic indexes, whose columns come from table constraints.
    pub columns: Vec<IndexColumn>,
    /// True for partial indexes (`WHERE` clause).
    pub partial: bool,
    /// True for `sqlite_autoindex_*` indexes created by UNIQUE/PRIMARY KEY constraints.
    pub automatic: bool,
}

/// A table row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Rowid; `None` in `WITHOUT ROWID` tables.
    pub rowid: Option<i64>,
    /// One value per column, in declaration order. A column declared `INTEGER PRIMARY KEY` holds
    /// the rowid. Columns missing from a short record (added later by `ALTER TABLE`) take their
    /// literal `DEFAULT`; a non-literal default is reported as [`Error::Unsupported`].
    pub values: Vec<Value>,
}

/// An index entry: the indexed column values followed, in ordinary indexes, by the rowid.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    /// Decoded record.
    pub values: Vec<Value>,
}

impl IndexEntry {
    /// The rowid of the indexed row (the last value), if it is an integer.
    pub fn rowid(&self) -> Option<i64> {
        match self.values.last() {
            Some(Value::Integer(r)) => Some(*r),
            _ => None,
        }
    }
}

/// An open database file. All access is read-only.
pub struct Database<R> {
    pub(crate) pager: Pager<R>,
    /// Length of the main database file in bytes.
    pub(crate) file_len: u64,
}

impl Database<File> {
    /// Opens a database file with default options. Data that is only in a `-wal` file is not
    /// visible; use [`open_with_wal`](Self::open_with_wal) for that.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, Options::default())
    }

    /// Opens a database file and overlays the committed frames of `<path>-wal`, if that file
    /// exists, so reads see what a SQLite reader would see. Neither file is modified.
    pub fn open_with_wal(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut wal_path = path.as_os_str().to_owned();
        wal_path.push("-wal");
        let wal = match std::fs::metadata(&wal_path) {
            Ok(m) if m.len() > 0 => Some(Wal::open(&wal_path)?),
            _ => None,
        };
        let file = File::open(path)?;
        match wal {
            Some(w) => Database::with_wal(file, &w, Options::default()),
            None => Database::with_options(file, Options::default()),
        }
    }

    /// Opens a database file with explicit options.
    pub fn open_with(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        Database::with_options(File::open(path)?, options)
    }
}

impl Database<Cursor<Vec<u8>>> {
    /// Reads a database from memory.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Database::new(Cursor::new(bytes))
    }
}

/// Column layout needed to turn a record into a row.
struct Layout<'a> {
    table: &'a Table,
    encoding: Encoding,
}

fn default_value(col: &Column) -> Result<Value> {
    let lit = match &col.default {
        None => return Ok(Value::Null),
        Some(ColumnDefault::Expression(e)) => {
            return Err(Error::Unsupported(format!(
                "column \"{}\" was added with a non-literal DEFAULT ({e}); litewalk does not evaluate expressions",
                col.name
            )))
        }
        Some(ColumnDefault::Literal(v)) => v.clone(),
    };
    let unsure = || {
        Error::Unsupported(format!(
            "cannot apply the column affinity of \"{}\" to its DEFAULT",
            col.name
        ))
    };
    Ok(match (lit, col.affinity()) {
        (Value::Integer(i), Affinity::Real) => Value::Real(i as f64),
        (Value::Integer(i), Affinity::Text) => Value::Text(i.to_string()),
        (Value::Real(f), Affinity::Real | Affinity::Blob) => Value::Real(f),
        (Value::Real(f), Affinity::Integer | Affinity::Numeric) if f.fract() != 0.0 => {
            Value::Real(f)
        }
        (Value::Real(f), Affinity::Integer | Affinity::Numeric)
            if f.abs() < 9_007_199_254_740_992.0 =>
        {
            Value::Integer(f as i64)
        }
        (Value::Real(_), _) => return Err(unsure()),
        (Value::Text(s), Affinity::Integer | Affinity::Real | Affinity::Numeric)
            if s.trim().parse::<f64>().is_ok() =>
        {
            return Err(unsure())
        }
        (v, _) => v,
    })
}

fn make_row(layout: &Layout<'_>, rowid: Option<i64>, payload: &[u8]) -> Result<Row> {
    let t = layout.table;
    let ncols = t.columns.len();
    let stored = decode_record(payload, layout.encoding)?;

    let mut slots: Vec<Option<Value>> = vec![None; ncols];
    if t.without_rowid {
        let non_pk: Vec<usize> = (0..ncols).filter(|i| !t.pk_columns.contains(i)).collect();
        for (j, v) in stored.into_iter().enumerate() {
            let target = if j < t.pk_columns.len() {
                t.pk_columns.get(j)
            } else {
                non_pk.get(j - t.pk_columns.len())
            };
            let Some(&target) = target else {
                return Err(corrupt(
                    CorruptionKind::Record,
                    "record has more columns than the table declares",
                )
                .with_expected(ncols, j + 1));
            };
            slots[target] = Some(v);
        }
    } else {
        for (slot, v) in slots.iter_mut().zip(stored) {
            *slot = Some(v);
        }
    }

    let mut values = Vec::with_capacity(ncols);
    for (col, slot) in t.columns.iter().zip(slots) {
        values.push(match slot {
            Some(Value::Integer(n)) if col.affinity() == Affinity::Real => Value::Real(n as f64),
            Some(v) => v,
            None => default_value(col)?,
        });
    }
    if let (Some(c), Some(id)) = (t.rowid_alias, rowid) {
        values[c] = Value::Integer(id);
    }
    Ok(Row { rowid, values })
}

impl<R: Read + Seek> Database<R> {
    /// Opens a database from any seekable reader with default options.
    pub fn new(src: R) -> Result<Self> {
        Self::with_options(src, Options::default())
    }

    /// Opens a database from any seekable reader.
    pub fn from_reader(src: R) -> Result<Self> {
        Self::new(src)
    }

    /// Opens a database from a reader with explicit options.
    pub fn with_options(src: R, options: Options) -> Result<Self> {
        Self::open_inner(src, None, options)
    }

    /// Opens a database and overlays the committed frames of a WAL on top of it.
    pub fn with_wal(src: R, wal: &Wal, options: Options) -> Result<Self> {
        Self::open_inner(src, Some(wal), options)
    }

    fn open_inner(mut src: R, wal: Option<&Wal>, options: Options) -> Result<Self> {
        let file_len = src.seek(SeekFrom::End(0))?;
        let overlay = wal.map(Wal::overlay);
        let mut buf = [0u8; HEADER_SIZE];
        let mut len = file_len;
        match overlay.as_ref().and_then(|o| o.page(1)) {
            // Page 1 as the WAL has it holds the current header.
            Some(page1) => {
                buf.copy_from_slice(&page1[..HEADER_SIZE]);
                let w = wal.expect("overlay implies a wal");
                len = len.max(
                    u64::from(w.db_size_after_commit.unwrap_or(0)) * u64::from(w.header.page_size),
                );
            }
            None => {
                if file_len < HEADER_SIZE as u64 {
                    return Err(Error::NotSqlite);
                }
                src.seek(SeekFrom::Start(0))?;
                src.read_exact(&mut buf)?;
            }
        }
        let header = Header::parse(&buf, len)?;
        if let Some(w) = wal {
            // The main file may be stale, so its own header must agree with the log too.
            let mut main = [0u8; HEADER_SIZE];
            let main_page_size = if file_len >= HEADER_SIZE as u64 {
                src.seek(SeekFrom::Start(0))?;
                src.read_exact(&mut main)?;
                raw_page_size(&main)
            } else {
                None
            };
            for found in [Some(header.page_size), main_page_size]
                .into_iter()
                .flatten()
            {
                if w.header.page_size as usize != found {
                    return Err(corrupt(
                        CorruptionKind::Wal,
                        "WAL page size differs from the database page size",
                    )
                    .with_expected(found, w.header.page_size));
                }
            }
        }
        Ok(Database {
            pager: Pager::new(src, header, options.cache_pages, overlay),
            file_len,
        })
    }

    /// Length of the main database file in bytes.
    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    /// The decoded database header.
    pub fn header(&self) -> &Header {
        &self.pager.header
    }

    /// Page cache counters since opening (or the last [`reset_cache_stats`](Self::reset_cache_stats)).
    pub fn cache_stats(&self) -> CacheStats {
        self.pager.stats()
    }

    /// Zeroes the cache counters.
    pub fn reset_cache_stats(&mut self) {
        self.pager.reset_stats();
    }

    fn schema_table() -> Table {
        let column = |name: &str| Column {
            name: name.into(),
            declared_type: String::new(),
            primary_key: false,
            not_null: false,
            default: None,
            generated: None,
        };
        Table {
            name: "sqlite_schema".into(),
            root_page: 1,
            columns: ["type", "name", "tbl_name", "rootpage", "sql"]
                .map(column)
                .to_vec(),
            without_rowid: false,
            unsupported: None,
            rowid_alias: None,
            pk_columns: Vec::new(),
        }
    }

    /// Every entry in `sqlite_schema`, in rowid order.
    pub fn schema(&mut self) -> Result<Vec<SchemaObject>> {
        match self.schema_lenient()? {
            (objects, None) => Ok(objects),
            (_, Some(e)) => Err(e),
        }
    }

    /// Reads schema rows until the first corruption. `Err` is only returned for I/O failures;
    /// a corruption is returned next to the rows read before it.
    pub(crate) fn schema_lenient(&mut self) -> Result<(Vec<SchemaObject>, Option<Error>)> {
        let schema = Self::schema_table();
        let mut objects = Vec::new();
        let mut scan = self.scan(&schema)?;
        for row in &mut scan {
            let row = match row {
                Ok(r) => r,
                Err(e @ Error::Io(_)) => return Err(e),
                Err(e) => return Ok((objects, Some(e))),
            };
            match schema_object(&row) {
                Ok(o) => objects.push(o),
                Err(e) => return Ok((objects, Some(e))),
            }
        }
        Ok((objects, None))
    }

    /// Lists the tables recorded in the schema.
    pub fn tables(&mut self) -> Result<Vec<Table>> {
        Ok(self
            .schema()?
            .into_iter()
            .filter(|o| o.kind == ObjectKind::Table)
            .map(table_from_object)
            .collect())
    }

    /// Lists the indexes recorded in the schema.
    pub fn indexes(&mut self) -> Result<Vec<Index>> {
        Ok(self
            .schema()?
            .into_iter()
            .filter(|o| o.kind == ObjectKind::Index)
            .map(index_from_object)
            .collect())
    }

    /// Finds a table by name (ASCII case-insensitive, as in SQLite).
    pub fn table(&mut self, name: &str) -> Result<Table> {
        self.tables()?
            .into_iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::NoSuchTable(name.to_owned()))
    }

    /// Finds an index by name (ASCII case-insensitive).
    pub fn index(&mut self, name: &str) -> Result<Index> {
        self.indexes()?
            .into_iter()
            .find(|i| i.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::NoSuchIndex(name.to_owned()))
    }

    fn tree_scan(&mut self, root: u32, kind: TreeKind) -> TreeScan<'_, R> {
        let max_payload = max_payload(&self.pager);
        TreeScan {
            pager: &mut self.pager,
            kind,
            stack: Vec::new(),
            seen: HashSet::new(),
            root: Some(root),
            last_rowid: None,
            failed: false,
            max_payload,
        }
    }

    /// Iterates all rows: in rowid order, or in primary-key order for `WITHOUT ROWID` tables.
    /// Stops after the first error.
    pub fn scan<'a>(&'a mut self, table: &'a Table) -> Result<TableScan<'a, R>> {
        if let Some(reason) = &table.unsupported {
            return Err(Error::Unsupported(format!(
                "table \"{}\": {reason}",
                table.name
            )));
        }
        let encoding = self.pager.header.encoding;
        let kind = if table.without_rowid {
            TreeKind::Index
        } else {
            TreeKind::Table
        };
        Ok(TableScan {
            inner: self.tree_scan(table.root_page, kind),
            table,
            encoding,
        })
    }

    /// Iterates the entries of an index in index order. Stops after the first error.
    pub fn scan_index(&mut self, index: &Index) -> Result<IndexScan<'_, R>> {
        let encoding = self.pager.header.encoding;
        Ok(IndexScan {
            inner: self.tree_scan(index.root_page, TreeKind::Index),
            encoding,
            name: index.name.clone(),
        })
    }

    /// Calls `f` for every record of a table b-tree with values that borrow from the page
    /// cache or the assembled payload, so no per-value allocation happens. Return `false` from
    /// `f` to stop. Returns the number of records visited.
    ///
    /// These are the *stored* records: no `INTEGER PRIMARY KEY` substitution (that column is
    /// NULL), no REAL-affinity conversion, no defaults for columns missing from short records,
    /// and `WITHOUT ROWID` records in key order (primary key columns first). Use
    /// [`scan`](Self::scan) for rows with SQLite's logical semantics. Only UTF-8 databases
    /// are supported, because UTF-16 text cannot be borrowed.
    pub fn for_each_record(
        &mut self,
        table: &Table,
        mut f: impl FnMut(Option<i64>, &[ValueRef<'_>]) -> bool,
    ) -> Result<u64> {
        if let Some(reason) = &table.unsupported {
            return Err(Error::Unsupported(format!(
                "table \"{}\": {reason}",
                table.name
            )));
        }
        if self.pager.header.encoding != Encoding::Utf8 {
            return Err(Error::Unsupported(
                "borrowed records need a UTF-8 database".into(),
            ));
        }
        let kind = if table.without_rowid {
            TreeKind::Index
        } else {
            TreeKind::Table
        };
        let mut scan = self.tree_scan(table.root_page, kind);
        let mut count = 0;
        while let Some(entry) = scan.next_entry() {
            let entry = entry.map_err(|e| e.with_table(&table.name))?;
            let values =
                decode_record_ref(&entry.payload).map_err(|e| e.with_table(&table.name))?;
            count += 1;
            if !f(entry.rowid, &values) {
                break;
            }
        }
        Ok(count)
    }

    /// Looks a row up by rowid in O(depth) page reads. Not available for `WITHOUT ROWID` tables.
    pub fn get(&mut self, table: &Table, rowid: i64) -> Result<Option<Row>> {
        if let Some(reason) = &table.unsupported {
            return Err(Error::Unsupported(format!(
                "table \"{}\": {reason}",
                table.name
            )));
        }
        if table.without_rowid {
            return Err(Error::Unsupported(format!(
                "table \"{}\" is WITHOUT ROWID and has no rowids",
                table.name
            )));
        }
        let encoding = self.pager.header.encoding;
        let max = max_payload(&self.pager);
        let ctx = |e: Error, depth: usize| e.with_table(&table.name).with_depth(depth);
        let mut page_no = table.root_page;
        for depth in 0..MAX_DEPTH {
            let view = PageView::load(&mut self.pager, page_no).map_err(|e| ctx(e, depth))?;
            let n = view.hdr.ncells;
            // First cell whose key is >= rowid. Keys are sorted, so this is a binary search that
            // decodes O(log n) cells instead of every cell on the page.
            let first_at_or_after = |view: &PageView| -> Result<usize> {
                let (mut lo, mut hi) = (0, n);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    let key = view.cell(mid, max)?.rowid.expect("table cells have rowids");
                    if key < rowid {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                Ok(lo)
            };
            match view.hdr.kind {
                PageType::InteriorTable => {
                    let at = first_at_or_after(&view).map_err(|e| ctx(e, depth))?;
                    page_no = if at < n {
                        view.cell(at, max)
                            .map_err(|e| ctx(e, depth))?
                            .left_child
                            .expect("interior cells have children")
                    } else {
                        view.hdr
                            .rightmost
                            .expect("interior pages have a rightmost pointer")
                    };
                }
                PageType::LeafTable => {
                    let at = first_at_or_after(&view).map_err(|e| ctx(e, depth))?;
                    if at == n
                        || view.cell(at, max).map_err(|e| ctx(e, depth))?.rowid != Some(rowid)
                    {
                        return Ok(None);
                    }
                    let offset = view.cell_offset(at).map_err(|e| ctx(e, depth))?;
                    let (meta, payload) = read_cell(
                        &mut self.pager,
                        &view.page,
                        view.number,
                        at,
                        PageType::LeafTable,
                        offset,
                    )
                    .map_err(|e| ctx(e, depth))?;
                    let layout = Layout { table, encoding };
                    return make_row(&layout, meta.rowid, &payload.bytes)
                        .map(Some)
                        .map_err(|e| {
                            e.with_table(&table.name)
                                .with_rowid(rowid)
                                .with_page(view.number)
                        });
                }
                _ => {
                    return Err(
                        corrupt(CorruptionKind::PageType, "expected a table b-tree page")
                            .with_page(page_no)
                            .with_expected("table page", view.hdr.kind.name())
                            .with_table(&table.name)
                            .with_depth(depth),
                    )
                }
            }
        }
        Err(corrupt(
            CorruptionKind::BTree,
            "b-tree is deeper than the traversal limit",
        )
        .with_table(&table.name)
        .with_expected(MAX_DEPTH, "more"))
    }
}

fn schema_object(row: &Row) -> Result<SchemaObject> {
    let rowid = row.rowid.unwrap_or(0);
    let bad = |reason: String| {
        corrupt(CorruptionKind::Schema, reason)
            .with_page(1)
            .with_rowid(rowid)
            .with_table("sqlite_schema")
    };
    let [Value::Text(kind), Value::Text(name), Value::Text(table_name), root, sql] =
        &row.values[..]
    else {
        return Err(bad("sqlite_schema row has an unexpected shape".into()));
    };
    let kind = match kind.as_str() {
        "table" => ObjectKind::Table,
        "index" => ObjectKind::Index,
        "view" => ObjectKind::View,
        "trigger" => ObjectKind::Trigger,
        other => return Err(bad(format!("unknown schema object type \"{other}\""))),
    };
    let root_page = match root {
        Value::Integer(n) if (0..=i64::from(u32::MAX)).contains(n) => *n as u32,
        _ => return Err(bad("rootpage is not a page number".into())),
    };
    let sql = match sql {
        Value::Text(s) => Some(s.clone()),
        _ => None,
    };
    Ok(SchemaObject {
        kind,
        name: name.clone(),
        table_name: table_name.clone(),
        root_page,
        sql,
    })
}

fn table_from_object(o: SchemaObject) -> Table {
    let parsed = o.sql.as_deref().and_then(parse_create_table);
    let mut table = match parsed {
        Some(def) => Table {
            name: o.name,
            root_page: o.root_page,
            columns: def.columns,
            without_rowid: def.without_rowid,
            unsupported: def.unsupported,
            rowid_alias: def.rowid_alias,
            pk_columns: def.pk_columns,
        },
        None => Table {
            name: o.name,
            root_page: o.root_page,
            columns: Vec::new(),
            without_rowid: false,
            unsupported: Some("CREATE statement could not be parsed".into()),
            rowid_alias: None,
            pk_columns: Vec::new(),
        },
    };
    if o.root_page == 0 {
        table.unsupported = Some("virtual table".into());
    }
    table
}

fn index_from_object(o: SchemaObject) -> Index {
    let def = o.sql.as_deref().and_then(parse_create_index);
    let automatic = o.sql.is_none();
    match def {
        Some(d) => Index {
            name: o.name,
            table: d.table,
            root_page: o.root_page,
            unique: d.unique,
            columns: d.columns,
            partial: d.partial,
            automatic,
        },
        None => Index {
            name: o.name,
            table: o.table_name,
            root_page: o.root_page,
            unique: automatic,
            columns: Vec::new(),
            partial: false,
            automatic,
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TreeKind {
    Table,
    Index,
}

enum Frame {
    InteriorTable {
        children: Vec<u32>,
        next: usize,
    },
    LeafTable {
        number: u32,
        page: std::sync::Arc<[u8]>,
        cells: Vec<usize>,
        next: usize,
    },
    InteriorIndex {
        number: u32,
        page: std::sync::Arc<[u8]>,
        cells: Vec<usize>,
        rightmost: u32,
        step: usize,
    },
    LeafIndex {
        number: u32,
        page: std::sync::Arc<[u8]>,
        cells: Vec<usize>,
        next: usize,
    },
}

struct Entry {
    rowid: Option<i64>,
    payload: Bytes,
}

/// In-order traversal of one b-tree. Iterative, so depth is bounded by `MAX_DEPTH` rather than
/// the call stack; each page may be visited once, which also catches cycles.
struct TreeScan<'a, R> {
    pager: &'a mut Pager<R>,
    kind: TreeKind,
    stack: Vec<Frame>,
    seen: HashSet<u32>,
    root: Option<u32>,
    last_rowid: Option<i64>,
    failed: bool,
    max_payload: usize,
}

impl<R: Read + Seek> TreeScan<'_, R> {
    fn push(&mut self, page_no: u32) -> Result<()> {
        let depth = self.stack.len();
        if depth >= MAX_DEPTH {
            return Err(corrupt(
                CorruptionKind::BTree,
                "b-tree is deeper than the traversal limit",
            )
            .with_page(page_no)
            .with_expected(MAX_DEPTH, depth + 1));
        }
        if !self.seen.insert(page_no) {
            return Err(corrupt(
                CorruptionKind::BTree,
                "page is referenced more than once in the tree",
            )
            .with_page(page_no)
            .with_depth(depth));
        }
        let node = load_node(self.pager, page_no).map_err(|e| e.with_depth(depth))?;
        match self.kind {
            TreeKind::Table => expect_table(&node),
            TreeKind::Index => expect_index(&node),
        }
        .map_err(|e| e.with_depth(depth))?;
        self.stack.push(match node {
            Node::InteriorTable { children, .. } => Frame::InteriorTable { children, next: 0 },
            Node::LeafTable {
                number,
                page,
                cells,
            } => Frame::LeafTable {
                number,
                page,
                cells,
                next: 0,
            },
            Node::InteriorIndex {
                number,
                page,
                cells,
                rightmost,
            } => Frame::InteriorIndex {
                number,
                page,
                cells,
                rightmost,
                step: 0,
            },
            Node::LeafIndex {
                number,
                page,
                cells,
            } => Frame::LeafIndex {
                number,
                page,
                cells,
                next: 0,
            },
        });
        Ok(())
    }

    fn advance(&mut self) -> Result<Option<Entry>> {
        if let Some(root) = self.root.take() {
            self.push(root)?;
        }
        let usable = self.pager.usable();
        loop {
            let Some(frame) = self.stack.last_mut() else {
                return Ok(None);
            };
            match frame {
                Frame::InteriorTable { children, next } => {
                    if *next >= children.len() {
                        self.stack.pop();
                        continue;
                    }
                    let child = children[*next];
                    *next += 1;
                    self.push(child)?;
                }
                Frame::LeafTable {
                    number,
                    page,
                    cells,
                    next,
                } => {
                    if *next >= cells.len() {
                        self.stack.pop();
                        continue;
                    }
                    let i = *next;
                    *next += 1;
                    let (meta, payload) =
                        read_cell(self.pager, page, *number, i, PageType::LeafTable, cells[i])?;
                    let rowid = meta.rowid.expect("table leaf cells have rowids");
                    if self.last_rowid.is_some_and(|prev| prev >= rowid) {
                        return Err(corrupt(
                            CorruptionKind::BTree,
                            "rowids are not in increasing order",
                        )
                        .with_page(*number)
                        .with_cell(i)
                        .with_rowid(rowid)
                        .with_expected(format!("> {}", self.last_rowid.unwrap_or(0)), rowid));
                    }
                    self.last_rowid = Some(rowid);
                    return Ok(Some(Entry {
                        rowid: Some(rowid),
                        payload: payload.bytes,
                    }));
                }
                Frame::LeafIndex {
                    number,
                    page,
                    cells,
                    next,
                } => {
                    if *next >= cells.len() {
                        self.stack.pop();
                        continue;
                    }
                    let i = *next;
                    *next += 1;
                    let (_, payload) =
                        read_cell(self.pager, page, *number, i, PageType::LeafIndex, cells[i])?;
                    return Ok(Some(Entry {
                        rowid: None,
                        payload: payload.bytes,
                    }));
                }
                Frame::InteriorIndex {
                    number,
                    page,
                    cells,
                    rightmost,
                    step,
                } => {
                    if *step > 2 * cells.len() {
                        self.stack.pop();
                        continue;
                    }
                    let k = *step;
                    *step += 1;
                    let i = k / 2;
                    if k % 2 == 0 {
                        let child = if i < cells.len() {
                            cell_meta(
                                page,
                                usable,
                                PageType::InteriorIndex,
                                cells[i],
                                self.max_payload,
                            )
                            .map_err(|e| e.with_page(*number).with_cell(i))?
                            .left_child
                            .expect("interior cells have children")
                        } else {
                            *rightmost
                        };
                        self.push(child)?;
                    } else {
                        let (_, payload) = read_cell(
                            self.pager,
                            page,
                            *number,
                            i,
                            PageType::InteriorIndex,
                            cells[i],
                        )?;
                        return Ok(Some(Entry {
                            rowid: None,
                            payload: payload.bytes,
                        }));
                    }
                }
            }
        }
    }

    fn next_entry(&mut self) -> Option<Result<Entry>> {
        if self.failed {
            return None;
        }
        match self.advance() {
            Ok(entry) => entry.map(Ok),
            Err(e) => {
                self.failed = true;
                Some(Err(e))
            }
        }
    }
}

/// Iterator over a table's rows. See [`Database::scan`].
pub struct TableScan<'a, R> {
    inner: TreeScan<'a, R>,
    table: &'a Table,
    encoding: Encoding,
}

impl<R: Read + Seek> Iterator for TableScan<'_, R> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = match self.inner.next_entry()? {
            Ok(e) => e,
            Err(e) => return Some(Err(e.with_table(&self.table.name))),
        };
        let layout = Layout {
            table: self.table,
            encoding: self.encoding,
        };
        Some(make_row(&layout, entry.rowid, &entry.payload).map_err(|e| {
            let e = e.with_table(&self.table.name);
            match entry.rowid {
                Some(r) => e.with_rowid(r),
                None => e,
            }
        }))
    }
}

/// Iterator over an index's entries. See [`Database::scan_index`].
pub struct IndexScan<'a, R> {
    inner: TreeScan<'a, R>,
    encoding: Encoding,
    name: String,
}

impl<R: Read + Seek> Iterator for IndexScan<'_, R> {
    type Item = Result<IndexEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = match self.inner.next_entry()? {
            Ok(e) => e,
            Err(e) => return Some(Err(e.with_table(&self.name))),
        };
        Some(
            decode_record(&entry.payload, self.encoding)
                .map(|values| IndexEntry { values })
                .map_err(|e| e.with_table(&self.name)),
        )
    }
}
