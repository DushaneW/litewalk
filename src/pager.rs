use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::error::{corrupt, CorruptionKind, Result};
use crate::header::Header;
use crate::wal::Overlay;

/// Page cache counters. With a capacity of 0 every request is a miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Maximum number of cached pages (0 = cache disabled).
    pub capacity: usize,
    /// Requests served from memory.
    pub hits: u64,
    /// Requests that read the page from the source.
    pub misses: u64,
}

impl CacheStats {
    /// Total page requests.
    pub fn requests(&self) -> u64 {
        self.hits + self.misses
    }

    /// Fraction of requests served from memory, `None` before the first request.
    pub fn hit_ratio(&self) -> Option<f64> {
        (self.requests() > 0).then(|| self.hits as f64 / self.requests() as f64)
    }
}

/// Least-recently-used page cache. Recency is a monotonically increasing tick; the order map
/// finds the victim in O(log n).
struct Lru {
    capacity: usize,
    pages: HashMap<u32, (Arc<[u8]>, u64)>,
    order: BTreeMap<u64, u32>,
    tick: u64,
}

impl Lru {
    fn new(capacity: usize) -> Self {
        Lru {
            capacity,
            pages: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
        }
    }

    fn get(&mut self, number: u32) -> Option<Arc<[u8]>> {
        let (page, old) = self.pages.get_mut(&number)?;
        self.order.remove(old);
        self.tick += 1;
        *old = self.tick;
        self.order.insert(self.tick, number);
        Some(Arc::clone(page))
    }

    fn insert(&mut self, number: u32, page: Arc<[u8]>) {
        if self.capacity == 0 {
            return;
        }
        if self.pages.len() >= self.capacity {
            if let Some((_, victim)) = self.order.pop_first() {
                self.pages.remove(&victim);
            }
        }
        self.tick += 1;
        self.pages.insert(number, (page, self.tick));
        self.order.insert(self.tick, number);
    }
}

/// Reads whole pages by number through an optional cache.
pub(crate) struct Pager<R> {
    src: R,
    pub header: Header,
    cache: Lru,
    overlay: Option<Overlay>,
    hits: u64,
    misses: u64,
}

impl<R: Read + Seek> Pager<R> {
    pub fn new(src: R, header: Header, cache_pages: usize, overlay: Option<Overlay>) -> Self {
        Pager {
            src,
            header,
            cache: Lru::new(cache_pages),
            overlay,
            hits: 0,
            misses: 0,
        }
    }

    pub fn page_count(&self) -> u32 {
        self.header.page_count
    }

    pub fn usable(&self) -> usize {
        self.header.usable_size()
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            capacity: self.cache.capacity,
            hits: self.hits,
            misses: self.misses,
        }
    }

    pub fn reset_stats(&mut self) {
        self.hits = 0;
        self.misses = 0;
    }

    pub fn read_page(&mut self, number: u32) -> Result<Arc<[u8]>> {
        if number == 0 || number > self.header.page_count {
            return Err(corrupt(
                CorruptionKind::PageNumber,
                format!("page {number} is outside the database"),
            )
            .with_expected(format!("1..={}", self.header.page_count), number));
        }
        if let Some(page) = self.cache.get(number) {
            self.hits += 1;
            return Ok(page);
        }
        self.misses += 1;
        let page: Arc<[u8]> = match self.overlay.as_ref().and_then(|o| o.page(number)) {
            Some(frame) => frame.into(),
            None => {
                let mut buf = vec![0u8; self.header.page_size];
                self.src.seek(SeekFrom::Start(
                    u64::from(number - 1) * self.header.page_size as u64,
                ))?;
                self.src.read_exact(&mut buf)?;
                buf.into()
            }
        };
        self.cache.insert(number, Arc::clone(&page));
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_evicts_the_least_recently_used_page() {
        let mut c = Lru::new(2);
        let page = |b: u8| -> Arc<[u8]> { vec![b; 4].into() };
        c.insert(1, page(1));
        c.insert(2, page(2));
        assert!(c.get(1).is_some()); // 2 is now the oldest
        c.insert(3, page(3));
        assert!(c.get(2).is_none());
        assert!(c.get(1).is_some());
        assert!(c.get(3).is_some());
    }

    #[test]
    fn zero_capacity_stores_nothing() {
        let mut c = Lru::new(0);
        c.insert(1, vec![0; 4].into());
        assert!(c.get(1).is_none());
    }
}
