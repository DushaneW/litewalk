use crate::error::{corrupt, CorruptionKind, Error, Result};

const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Size of the database header at the start of page 1.
pub const HEADER_SIZE: usize = 100;

/// Smallest and largest page sizes SQLite allows.
const MIN_PAGE_SIZE: u32 = 512;
const MAX_PAGE_SIZE: u32 = 32_768;
/// Page-size field value that stands for 65536.
const PAGE_SIZE_65536: u32 = 1;
/// Pages need at least this many usable bytes (page size minus reserved region).
const MIN_USABLE: usize = 480;

mod offset {
    pub const PAGE_SIZE: usize = 16;
    pub const WRITE_VERSION: usize = 18;
    pub const READ_VERSION: usize = 19;
    pub const RESERVED: usize = 20;
    pub const MAX_PAYLOAD_FRACTION: usize = 21;
    pub const MIN_PAYLOAD_FRACTION: usize = 22;
    pub const LEAF_PAYLOAD_FRACTION: usize = 23;
    pub const CHANGE_COUNTER: usize = 24;
    pub const PAGE_COUNT: usize = 28;
    pub const FREELIST_TRUNK: usize = 32;
    pub const FREELIST_COUNT: usize = 36;
    pub const SCHEMA_COOKIE: usize = 40;
    pub const SCHEMA_FORMAT: usize = 44;
    pub const LARGEST_ROOT_PAGE: usize = 52;
    pub const TEXT_ENCODING: usize = 56;
    pub const USER_VERSION: usize = 60;
    pub const INCREMENTAL_VACUUM: usize = 64;
    pub const APPLICATION_ID: usize = 68;
    pub const VERSION_VALID_FOR: usize = 92;
    pub const SQLITE_VERSION: usize = 96;
}

/// Text encoding of the database (header offset 56).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8.
    Utf8,
    /// UTF-16, little endian.
    Utf16Le,
    /// UTF-16, big endian.
    Utf16Be,
}

impl Encoding {
    /// Name as reported by `PRAGMA encoding`.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16le",
            Encoding::Utf16Be => "UTF-16be",
        }
    }
}

/// Auto-vacuum mode, derived from header offsets 52 and 64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoVacuum {
    /// Pages are not moved when freed.
    None,
    /// Freed pages are reclaimed at every commit.
    Full,
    /// Freed pages are reclaimed on `PRAGMA incremental_vacuum`.
    Incremental,
}

/// Decoded database header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Header {
    /// Page size in bytes.
    pub page_size: usize,
    /// Bytes reserved at the end of each page (e.g. for encryption extensions).
    pub reserved: usize,
    /// Number of pages.
    pub page_count: u32,
    /// First freelist trunk page, 0 if the freelist is empty.
    pub freelist_trunk: u32,
    /// Number of freelist pages the header claims.
    pub freelist_count: u32,
    /// Incremented on every schema change.
    pub schema_cookie: u32,
    /// Schema format number (1 to 4).
    pub schema_format: u32,
    /// `PRAGMA user_version`.
    pub user_version: u32,
    /// `PRAGMA application_id`.
    pub application_id: u32,
    /// Text encoding of all text values.
    pub encoding: Encoding,
    /// Auto-vacuum mode.
    pub auto_vacuum: AutoVacuum,
    /// Largest root b-tree page, non-zero only in auto-vacuum databases.
    pub largest_root_page: u32,
    /// Number of the SQLite release that last wrote the file (0 if unknown).
    pub sqlite_version: u32,
    /// File change counter.
    pub change_counter: u32,
    /// File format write version (1 legacy, 2 WAL).
    pub write_version: u8,
    /// File format read version (1 legacy, 2 WAL).
    pub read_version: u8,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn be16(b: &[u8], at: usize) -> u32 {
    u32::from(u16::from_be_bytes([b[at], b[at + 1]]))
}

/// The page size field of a header, without validating anything else. `None` if `buf` is not a
/// SQLite header or the field is invalid.
pub(crate) fn raw_page_size(buf: &[u8]) -> Option<usize> {
    if buf.len() < HEADER_SIZE || &buf[..16] != MAGIC {
        return None;
    }
    match be16(buf, offset::PAGE_SIZE) {
        PAGE_SIZE_65536 => Some(65_536),
        n if (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&n) && n.is_power_of_two() => {
            Some(n as usize)
        }
        _ => None,
    }
}

impl Header {
    /// Bytes per page available to the b-tree (`page_size - reserved`).
    pub fn usable_size(&self) -> usize {
        self.page_size - self.reserved
    }

    /// True when the file is in WAL mode. Committed data may then live in the `-wal` file.
    pub fn wal(&self) -> bool {
        self.read_version == 2 || self.write_version == 2
    }

    /// Parses the first 100 bytes. `file_len` is the length of the main database file.
    pub fn parse(buf: &[u8], file_len: u64) -> Result<Header> {
        if buf.len() < HEADER_SIZE || &buf[..16] != MAGIC {
            return Err(Error::NotSqlite);
        }
        let bad = |reason: String| corrupt(CorruptionKind::Header, reason);

        let raw_size = be16(buf, offset::PAGE_SIZE);
        let page_size = match raw_size {
            PAGE_SIZE_65536 => 65_536,
            n if (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&n) && n.is_power_of_two() => n as usize,
            n => {
                return Err(bad(format!("invalid page size {n}"))
                    .with_offset(offset::PAGE_SIZE)
                    .with_expected("a power of two from 512 to 32768, or 1", n))
            }
        };
        let reserved = usize::from(buf[offset::RESERVED]);
        if page_size - reserved < MIN_USABLE {
            return Err(bad("usable page size is below 480 bytes".into())
                .with_offset(offset::RESERVED)
                .with_expected(
                    format!("at most {} reserved bytes", page_size - MIN_USABLE),
                    reserved,
                ));
        }
        let fractions = [
            (offset::MAX_PAYLOAD_FRACTION, 64u8),
            (offset::MIN_PAYLOAD_FRACTION, 32),
            (offset::LEAF_PAYLOAD_FRACTION, 32),
        ];
        for (at, want) in fractions {
            if buf[at] != want {
                return Err(bad("payload fractions must be 64/32/32".into())
                    .with_offset(at)
                    .with_expected(want, buf[at]));
            }
        }
        let (write_version, read_version) = (buf[offset::WRITE_VERSION], buf[offset::READ_VERSION]);
        if write_version > 2 || read_version > 2 {
            return Err(Error::Unsupported(format!(
                "file format versions {write_version}/{read_version}"
            )));
        }
        let encoding = match be32(buf, offset::TEXT_ENCODING) {
            0 | 1 => Encoding::Utf8,
            2 => Encoding::Utf16Le,
            3 => Encoding::Utf16Be,
            n => {
                return Err(bad(format!("invalid text encoding {n}"))
                    .with_offset(offset::TEXT_ENCODING)
                    .with_expected("1, 2 or 3", n))
            }
        };

        // The in-header page count is only trustworthy when the change counter matches the
        // "version-valid-for" field; otherwise the file length decides.
        let in_header = be32(buf, offset::PAGE_COUNT);
        let change_counter = be32(buf, offset::CHANGE_COUNTER);
        let page_count = if in_header != 0 && change_counter == be32(buf, offset::VERSION_VALID_FOR)
        {
            in_header
        } else {
            u32::try_from(file_len / page_size as u64).unwrap_or(u32::MAX)
        };
        if page_count == 0 {
            return Err(bad("database has no pages".into()));
        }
        let needed = u64::from(page_count) * page_size as u64;
        if needed > file_len {
            return Err(corrupt(
                CorruptionKind::Truncated,
                format!("file is shorter than its {page_count} pages of {page_size} bytes"),
            )
            .with_expected(format!("{needed} bytes"), format!("{file_len} bytes")));
        }

        let largest_root_page = be32(buf, offset::LARGEST_ROOT_PAGE);
        let auto_vacuum = match (largest_root_page, be32(buf, offset::INCREMENTAL_VACUUM)) {
            (0, _) => AutoVacuum::None,
            (_, 0) => AutoVacuum::Full,
            _ => AutoVacuum::Incremental,
        };
        Ok(Header {
            page_size,
            reserved,
            page_count,
            freelist_trunk: be32(buf, offset::FREELIST_TRUNK),
            freelist_count: be32(buf, offset::FREELIST_COUNT),
            schema_cookie: be32(buf, offset::SCHEMA_COOKIE),
            schema_format: be32(buf, offset::SCHEMA_FORMAT),
            user_version: be32(buf, offset::USER_VERSION),
            application_id: be32(buf, offset::APPLICATION_ID),
            encoding,
            auto_vacuum,
            largest_root_page,
            sqlite_version: be32(buf, offset::SQLITE_VERSION),
            change_counter,
            write_version,
            read_version,
        })
    }
}
