use std::fmt;
use std::io;

/// Crate-wide error type.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The underlying reader failed.
    Io(io::Error),
    /// The data does not start with the SQLite magic string.
    NotSqlite,
    /// The data is a SQLite database but violates the file format.
    Corrupt(Box<Corruption>),
    /// The data is valid but uses a feature this crate does not read.
    Unsupported(String),
    /// No table with this name exists in the schema.
    NoSuchTable(String),
    /// No index with this name exists in the schema.
    NoSuchIndex(String),
    /// A page number outside `1..=page_count` was requested.
    NoSuchPage {
        /// Requested page.
        page: u32,
        /// Number of pages in the database.
        page_count: u32,
    },
}

/// Result alias using [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// What kind of structure a corruption was found in. Used to group findings in `check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CorruptionKind {
    /// The 100-byte database header.
    Header,
    /// The file is shorter than the header claims.
    Truncated,
    /// A page number outside the database.
    PageNumber,
    /// A page whose type byte is not valid where it is used.
    PageType,
    /// A cell pointer or cell that does not fit in its page.
    CellPointer,
    /// A malformed varint.
    Varint,
    /// A record whose header or body does not fit its payload.
    Record,
    /// An overflow chain that is cyclic, truncated or out of range.
    Overflow,
    /// B-tree shape: depth, shared pages, key ordering.
    BTree,
    /// Freelist trunk/leaf pages.
    Freelist,
    /// The schema table or a schema entry.
    Schema,
    /// The write-ahead log.
    Wal,
}

impl CorruptionKind {
    /// Short stable name, used in JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            CorruptionKind::Header => "header",
            CorruptionKind::Truncated => "truncated",
            CorruptionKind::PageNumber => "page-number",
            CorruptionKind::PageType => "page-type",
            CorruptionKind::CellPointer => "cell-pointer",
            CorruptionKind::Varint => "varint",
            CorruptionKind::Record => "record",
            CorruptionKind::Overflow => "overflow",
            CorruptionKind::BTree => "btree",
            CorruptionKind::Freelist => "freelist",
            CorruptionKind::Schema => "schema",
            CorruptionKind::Wal => "wal",
        }
    }
}

/// A violation of the file format, with as much location context as was known where it was
/// detected. Context is added as the error propagates, so callers higher up the stack fill in
/// fields (table, rowid) that the parser at the bottom could not know.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Corruption {
    /// Which structure is damaged.
    pub kind: CorruptionKind,
    /// What is wrong.
    pub reason: String,
    /// Page the problem was found on (1-based).
    pub page: Option<u32>,
    /// Byte offset within that page.
    pub offset: Option<usize>,
    /// Cell index within the page.
    pub cell: Option<usize>,
    /// Rowid of the row being decoded.
    pub rowid: Option<i64>,
    /// Table or index being read.
    pub table: Option<String>,
    /// B-tree depth (root = 0).
    pub depth: Option<usize>,
    /// The value the format requires.
    pub expected: Option<String>,
    /// The value that was found.
    pub actual: Option<String>,
}

impl Corruption {
    /// A corruption with no location context.
    pub fn new(kind: CorruptionKind, reason: impl Into<String>) -> Self {
        Corruption {
            kind,
            reason: reason.into(),
            page: None,
            offset: None,
            cell: None,
            rowid: None,
            table: None,
            depth: None,
            expected: None,
            actual: None,
        }
    }

    /// `(label, value)` pairs for every context field that is set, in display order.
    pub fn context(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(t) = &self.table {
            out.push(("table", t.clone()));
        }
        if let Some(p) = self.page {
            out.push(("page", p.to_string()));
        }
        if let Some(o) = self.offset {
            out.push(("offset", o.to_string()));
        }
        if let Some(c) = self.cell {
            out.push(("cell", c.to_string()));
        }
        if let Some(r) = self.rowid {
            out.push(("rowid", r.to_string()));
        }
        if let Some(d) = self.depth {
            out.push(("depth", d.to_string()));
        }
        if let Some(e) = &self.expected {
            out.push(("expected", e.clone()));
        }
        if let Some(a) = &self.actual {
            out.push(("actual", a.clone()));
        }
        out
    }
}

impl fmt::Display for Corruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)?;
        let ctx = self.context();
        if !ctx.is_empty() {
            let parts: Vec<String> = ctx.iter().map(|(k, v)| format!("{k} {v}")).collect();
            write!(f, " ({})", parts.join(", "))?;
        }
        Ok(())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::NotSqlite => write!(f, "not a SQLite database"),
            Error::Corrupt(c) => write!(f, "corrupt database: {c}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::NoSuchTable(t) => write!(f, "no such table: {t}"),
            Error::NoSuchIndex(t) => write!(f, "no such index: {t}"),
            Error::NoSuchPage { page, page_count } => {
                write!(f, "no such page: {page} (database has {page_count} pages)")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl Error {
    /// The corruption details, if this is a [`Error::Corrupt`].
    pub fn corruption(&self) -> Option<&Corruption> {
        match self {
            Error::Corrupt(c) => Some(c),
            _ => None,
        }
    }

    fn fill(mut self, f: impl FnOnce(&mut Corruption)) -> Self {
        if let Error::Corrupt(c) = &mut self {
            f(c);
        }
        self
    }

    /// Records the page, unless an inner layer already did.
    pub fn with_page(self, page: u32) -> Self {
        self.fill(|c| {
            c.page.get_or_insert(page);
        })
    }

    /// Records the byte offset within the page, unless already set.
    pub fn with_offset(self, offset: usize) -> Self {
        self.fill(|c| {
            c.offset.get_or_insert(offset);
        })
    }

    /// Records the cell index, unless already set.
    pub fn with_cell(self, cell: usize) -> Self {
        self.fill(|c| {
            c.cell.get_or_insert(cell);
        })
    }

    /// Records the rowid, unless already set.
    pub fn with_rowid(self, rowid: i64) -> Self {
        self.fill(|c| {
            c.rowid.get_or_insert(rowid);
        })
    }

    /// Records the table or index name, unless already set.
    pub fn with_table(self, table: &str) -> Self {
        self.fill(|c| {
            if c.table.is_none() {
                c.table = Some(table.to_owned());
            }
        })
    }

    /// Records the b-tree depth, unless already set.
    pub fn with_depth(self, depth: usize) -> Self {
        self.fill(|c| {
            c.depth.get_or_insert(depth);
        })
    }

    /// Records expected and actual values, unless already set.
    pub fn with_expected(self, expected: impl fmt::Display, actual: impl fmt::Display) -> Self {
        self.fill(|c| {
            if c.expected.is_none() {
                c.expected = Some(expected.to_string());
                c.actual = Some(actual.to_string());
            }
        })
    }
}

/// Builds a corruption error without location context.
pub(crate) fn corrupt(kind: CorruptionKind, reason: impl Into<String>) -> Error {
    Error::Corrupt(Box::new(Corruption::new(kind, reason)))
}
