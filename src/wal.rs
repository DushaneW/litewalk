//! Read-only parsing of SQLite write-ahead logs.
//!
//! A WAL is a 32-byte header followed by frames (24-byte frame header plus one page). A frame
//! is valid only if its salts match the header and its checksum, which chains from the previous
//! frame, is correct; the first invalid frame ends the log. A transaction is committed when its
//! last frame carries a non-zero "database size after commit".

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::error::{corrupt, CorruptionKind, Error, Result};

const WAL_HEADER_SIZE: usize = 32;
const FRAME_HEADER_SIZE: usize = 24;
const MAGIC_LITTLE_ENDIAN_CHECKSUMS: u32 = 0x377f_0682;
const MAGIC_BIG_ENDIAN_CHECKSUMS: u32 = 0x377f_0683;
const WAL_VERSION: u32 = 3_007_000;

/// Decoded WAL header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalHeader {
    /// True when checksums are computed over big-endian words (magic 0x377f0683).
    pub big_endian_checksums: bool,
    /// File format version (3007000).
    pub version: u32,
    /// Page size; must equal the database's.
    pub page_size: u32,
    /// Incremented at each checkpoint.
    pub checkpoint_sequence: u32,
    /// First salt; frames must repeat it.
    pub salt1: u32,
    /// Second salt; frames must repeat it.
    pub salt2: u32,
    /// Checksum stored in the header.
    pub checksum: (u32, u32),
}

/// One valid frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalFrame {
    /// Position in the log, starting at 0.
    pub index: usize,
    /// Database page this frame holds.
    pub page: u32,
    /// For commit frames, the database size in pages after the transaction.
    pub commit_size: Option<u32>,
}

/// Why the log ended before the end of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalStop {
    /// The file ends exactly after the last valid frame.
    EndOfFile,
    /// The file ends inside a frame.
    TruncatedFrame,
    /// A frame's salts differ from the header's: it belongs to an older generation of the log.
    SaltMismatch,
    /// A frame's checksum is wrong.
    ChecksumMismatch,
    /// A frame references page 0.
    InvalidPageNumber,
}

/// A parsed WAL.
#[derive(Debug, Clone)]
pub struct Wal {
    /// Header.
    pub header: WalHeader,
    /// Valid frames in log order. Parsing stops at the first invalid frame.
    pub frames: Vec<WalFrame>,
    /// Number of leading frames that belong to committed transactions.
    pub committed_frames: usize,
    /// Database size in pages after the last commit; `None` if nothing was committed.
    pub db_size_after_commit: Option<u32>,
    /// Why parsing stopped.
    pub stop: WalStop,
    /// Bytes after the last valid frame.
    pub trailing_bytes: usize,
    data: Arc<Vec<u8>>,
}

fn word(bytes: &[u8], at: usize, big_endian: bool) -> u32 {
    let b = [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
    if big_endian {
        u32::from_be_bytes(b)
    } else {
        u32::from_le_bytes(b)
    }
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The WAL checksum: Fletcher-like, over pairs of 32-bit words, chained through `seed`.
fn checksum(bytes: &[u8], big_endian: bool, seed: (u32, u32)) -> (u32, u32) {
    debug_assert_eq!(bytes.len() % 8, 0);
    let (mut s1, mut s2) = seed;
    for i in (0..bytes.len()).step_by(8) {
        s1 = s1.wrapping_add(word(bytes, i, big_endian)).wrapping_add(s2);
        s2 = s2
            .wrapping_add(word(bytes, i + 4, big_endian))
            .wrapping_add(s1);
    }
    (s1, s2)
}

fn wal_error(reason: impl Into<String>) -> Error {
    corrupt(CorruptionKind::Wal, reason)
}

impl Wal {
    /// Reads and parses a WAL file.
    pub fn open(path: impl AsRef<Path>) -> Result<Wal> {
        Wal::from_bytes(std::fs::read(path)?)
    }

    /// Parses a WAL held in memory. Fails only if the header is invalid; problems in frames
    /// end the log ([`Wal::stop`]) the way SQLite treats them.
    pub fn from_bytes(data: Vec<u8>) -> Result<Wal> {
        if data.len() < WAL_HEADER_SIZE {
            return Err(wal_error("file is shorter than the 32-byte WAL header"));
        }
        let magic = be32(&data, 0);
        let big_endian = match magic {
            MAGIC_LITTLE_ENDIAN_CHECKSUMS => false,
            MAGIC_BIG_ENDIAN_CHECKSUMS => true,
            other => {
                return Err(wal_error("bad WAL magic number")
                    .with_offset(0)
                    .with_expected("0x377f0682 or 0x377f0683", format!("{other:#010x}")))
            }
        };
        let header = WalHeader {
            big_endian_checksums: big_endian,
            version: be32(&data, 4),
            page_size: be32(&data, 8),
            checkpoint_sequence: be32(&data, 12),
            salt1: be32(&data, 16),
            salt2: be32(&data, 20),
            checksum: (be32(&data, 24), be32(&data, 28)),
        };
        if header.version != WAL_VERSION {
            return Err(Error::Unsupported(format!(
                "WAL format version {}",
                header.version
            )));
        }
        let ps = header.page_size;
        if !(512..=65_536).contains(&ps) || !ps.is_power_of_two() {
            return Err(wal_error("invalid WAL page size")
                .with_offset(8)
                .with_expected("a power of two from 512 to 65536", ps));
        }
        let computed = checksum(&data[..24], big_endian, (0, 0));
        if computed != header.checksum {
            return Err(wal_error("WAL header checksum mismatch")
                .with_offset(24)
                .with_expected(
                    format!("{:#x}/{:#x}", computed.0, computed.1),
                    format!("{:#x}/{:#x}", header.checksum.0, header.checksum.1),
                ));
        }

        let frame_size = FRAME_HEADER_SIZE + ps as usize;
        let mut frames = Vec::new();
        let mut running = header.checksum;
        let mut committed_frames = 0;
        let mut db_size_after_commit = None;
        let mut at = WAL_HEADER_SIZE;
        let stop = loop {
            if at == data.len() {
                break WalStop::EndOfFile;
            }
            if at + frame_size > data.len() {
                break WalStop::TruncatedFrame;
            }
            let fh = &data[at..at + FRAME_HEADER_SIZE];
            let (page, commit) = (be32(fh, 0), be32(fh, 4));
            if be32(fh, 8) != header.salt1 || be32(fh, 12) != header.salt2 {
                break WalStop::SaltMismatch;
            }
            let sum = checksum(
                &data[at + FRAME_HEADER_SIZE..at + frame_size],
                big_endian,
                checksum(&fh[..8], big_endian, running),
            );
            if sum != (be32(fh, 16), be32(fh, 20)) {
                break WalStop::ChecksumMismatch;
            }
            if page == 0 {
                break WalStop::InvalidPageNumber;
            }
            running = sum;
            let commit_size = (commit != 0).then_some(commit);
            frames.push(WalFrame {
                index: frames.len(),
                page,
                commit_size,
            });
            if let Some(size) = commit_size {
                committed_frames = frames.len();
                db_size_after_commit = Some(size);
            }
            at += frame_size;
        };
        let trailing_bytes = data.len() - (WAL_HEADER_SIZE + frames.len() * frame_size);
        Ok(Wal {
            header,
            frames,
            committed_frames,
            db_size_after_commit,
            stop,
            trailing_bytes,
            data: Arc::new(data),
        })
    }

    /// Page contents of frame `index`.
    pub fn frame_page(&self, index: usize) -> Option<&[u8]> {
        (index < self.frames.len()).then(|| {
            let ps = self.header.page_size as usize;
            let start = WAL_HEADER_SIZE + index * (FRAME_HEADER_SIZE + ps) + FRAME_HEADER_SIZE;
            &self.data[start..start + ps]
        })
    }

    /// Number of distinct pages in committed frames.
    pub fn committed_pages(&self) -> usize {
        self.frames[..self.committed_frames]
            .iter()
            .map(|f| f.page)
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// Maps each page to the data of its newest committed frame: what a reader sees on top of
    /// the main database file.
    pub(crate) fn overlay(&self) -> Overlay {
        let mut latest: HashMap<u32, usize> = HashMap::new();
        for f in &self.frames[..self.committed_frames] {
            latest.insert(f.page, f.index);
        }
        Overlay {
            wal: self.clone(),
            latest,
        }
    }
}

/// Newest committed version of each page present in the WAL.
pub(crate) struct Overlay {
    wal: Wal,
    latest: HashMap<u32, usize>,
}

impl Overlay {
    pub fn page(&self, number: u32) -> Option<&[u8]> {
        self.latest
            .get(&number)
            .and_then(|&i| self.wal.frame_page(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a WAL by hand, computing checksums with the same algorithm; tests of the algorithm
    /// itself against real SQLite output are in tests/wal.rs.
    fn build(page_size: u32, frames: &[(u32, u32, u8)], big: bool) -> Vec<u8> {
        let magic = if big {
            MAGIC_BIG_ENDIAN_CHECKSUMS
        } else {
            MAGIC_LITTLE_ENDIAN_CHECKSUMS
        };
        let (salt1, salt2) = (0xaabb_ccdd_u32, 0x1122_3344_u32);
        let mut out = Vec::new();
        for v in [magic, WAL_VERSION, page_size, 0, salt1, salt2] {
            out.extend(v.to_be_bytes());
        }
        let c = checksum(&out, big, (0, 0));
        out.extend(c.0.to_be_bytes());
        out.extend(c.1.to_be_bytes());
        let mut running = c;
        for &(page, commit, fill) in frames {
            let mut fh = Vec::new();
            for v in [page, commit, salt1, salt2] {
                fh.extend(v.to_be_bytes());
            }
            let data = vec![fill; page_size as usize];
            running = checksum(&data, big, checksum(&fh[..8], big, running));
            fh.extend(running.0.to_be_bytes());
            fh.extend(running.1.to_be_bytes());
            out.extend(fh);
            out.extend(data);
        }
        out
    }

    #[test]
    fn parses_frames_and_commit_boundaries_in_both_checksum_orders() {
        for big in [false, true] {
            let wal = Wal::from_bytes(build(512, &[(1, 0, 1), (2, 3, 2), (2, 0, 3)], big)).unwrap();
            assert_eq!(wal.frames.len(), 3);
            assert_eq!(wal.committed_frames, 2);
            assert_eq!(wal.db_size_after_commit, Some(3));
            assert_eq!(wal.stop, WalStop::EndOfFile);
            assert_eq!(wal.header.big_endian_checksums, big);
            assert_eq!(wal.committed_pages(), 2);
            let overlay = wal.overlay();
            assert_eq!(overlay.page(2).unwrap()[0], 2); // frame 2 is uncommitted and must not win
            assert!(overlay.page(7).is_none());
        }
    }

    #[test]
    fn a_bad_frame_ends_the_log() {
        let mut bytes = build(512, &[(1, 1, 1), (2, 2, 2), (3, 3, 3)], false);
        let frame_size = FRAME_HEADER_SIZE + 512;
        bytes[WAL_HEADER_SIZE + frame_size + FRAME_HEADER_SIZE + 10] ^= 1; // corrupt frame 1's data
        let wal = Wal::from_bytes(bytes).unwrap();
        assert_eq!((wal.frames.len(), wal.committed_frames), (1, 1));
        assert_eq!(wal.stop, WalStop::ChecksumMismatch);
        assert_eq!(wal.trailing_bytes, 2 * frame_size);
    }

    #[test]
    fn salt_mismatch_and_truncation_are_distinguished() {
        let mut bytes = build(512, &[(1, 1, 1), (2, 2, 2)], false);
        let frame_size = FRAME_HEADER_SIZE + 512;
        bytes[WAL_HEADER_SIZE + frame_size + 8] ^= 1;
        assert_eq!(Wal::from_bytes(bytes).unwrap().stop, WalStop::SaltMismatch);
        let mut bytes = build(512, &[(1, 1, 1), (2, 2, 2)], false);
        bytes.truncate(bytes.len() - 100);
        let wal = Wal::from_bytes(bytes).unwrap();
        assert_eq!((wal.frames.len(), wal.stop), (1, WalStop::TruncatedFrame));
    }

    #[test]
    fn invalid_headers_are_rejected() {
        let good = build(512, &[], false);
        assert!(Wal::from_bytes(good.clone()).unwrap().frames.is_empty());
        assert!(Wal::from_bytes(good[..20].to_vec()).is_err());
        let mut bad = good.clone();
        bad[3] = 0x00;
        assert!(Wal::from_bytes(bad).is_err());
        let mut bad = good.clone();
        bad[26] ^= 1; // header checksum
        assert!(Wal::from_bytes(bad).is_err());
        let mut bad = good;
        bad[11] = 3; // page size 3 (checksum no longer matches either)
        assert!(Wal::from_bytes(bad).is_err());
    }
}
