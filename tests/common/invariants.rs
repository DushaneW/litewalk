// Shared by the cargo-fuzz targets (via `include!`) and by tests/fuzz_invariants.rs, so that the
// deterministic mutation run and libFuzzer check exactly the same code. Uses only the public API.
//
// The invariant for every function: arbitrary input produces a value or a controlled error. It
// must never panic and every loop must terminate. A panic or a timeout is a bug.

/// Scan caps. A well-formed tree cannot yield more entries than bytes in the input, so reaching a
/// cap means the traversal did not terminate.
pub fn cap(len: usize) -> usize {
    len.saturating_mul(2) + 1000
}

pub fn header(data: &[u8]) {
    let len = data.len() as u64;
    let _ = litewalk::Header::parse(data, len);
    let _ = litewalk::Header::parse(data, len.saturating_mul(4096));
    let _ = litewalk::Header::parse(data, 0);
}

pub fn varint(data: &[u8]) {
    if let Some((_, n)) = litewalk::read_varint(data) {
        assert!((1..=9).contains(&n) && n <= data.len(), "varint consumed {n} of {} bytes", data.len());
    }
}

pub fn record(data: &[u8]) {
    use litewalk::Encoding::{Utf16Be, Utf16Le, Utf8};
    for enc in [Utf8, Utf16Le, Utf16Be] {
        let _ = litewalk::decode_record(data, enc);
    }
    // The borrowed decoder must agree with the owned one whenever both succeed.
    if let (Ok(owned), Ok(borrowed)) = (litewalk::decode_record(data, Utf8), litewalk::decode_record_ref(data)) {
        assert_eq!(owned.len(), borrowed.len());
        for (o, b) in owned.iter().zip(&borrowed) {
            assert_eq!(*o, b.to_owned());
        }
    }
}

pub fn wal(data: &[u8]) {
    if let Ok(w) = litewalk::Wal::from_bytes(data.to_vec()) {
        assert!(w.committed_frames <= w.frames.len());
        let _ = w.committed_pages();
        for i in 0..w.frames.len().min(8) {
            assert!(w.frame_page(i).is_some());
        }
    }
}

pub fn database(data: &[u8]) {
    let Ok(mut db) = litewalk::Database::from_bytes(data.to_vec()) else { return };
    let limit = cap(data.len());
    if let Ok(tables) = db.tables() {
        for t in tables.iter().take(64) {
            if let Ok(scan) = db.scan(t) {
                for (n, row) in scan.enumerate() {
                    assert!(n < limit, "table scan did not terminate");
                    if row.is_err() {
                        break;
                    }
                }
            }
            for rowid in [0, 1, 7, -1, i64::MAX, i64::MIN] {
                let _ = db.get(t, rowid);
            }
        }
    }
    if let Ok(indexes) = db.indexes() {
        for i in indexes.iter().take(64) {
            if let Ok(scan) = db.scan_index(i) {
                for (n, e) in scan.enumerate() {
                    assert!(n < limit, "index scan did not terminate");
                    if e.is_err() {
                        break;
                    }
                }
            }
        }
    }
    let pages = db.header().page_count.min(64);
    for p in 1..=pages {
        let _ = db.page(p);
        let _ = db.page_bytes(p);
    }
    let _ = db.check();
    let _ = db.map();
    let _ = db.freelist();
    let _ = db.forensic();
    let _ = db.free_regions();
}
