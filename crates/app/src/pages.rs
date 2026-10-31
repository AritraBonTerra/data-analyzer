//! A small LRU of fetched row pages for one frame, plus the bookkeeping the
//! grid needs to decide which pages to request next.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use da_core::Page;

/// Rows per page. ~4 ms a page on a 30M-row table, small enough to arrive fast.
pub const PAGE_SIZE: u64 = 500;
/// Pages kept in memory; at 500 rows each that's 12k rows of text.
const CAPACITY: usize = 24;

#[derive(Default)]
pub struct PageCache {
    /// The frame (epoch) these pages belong to; pages from any other are ignored.
    frame_id: u64,
    pages: HashMap<u64, Arc<Page>>,
    /// Least recently used first.
    order: VecDeque<u64>,
    inflight: HashSet<u64>,
    last_range: Range<usize>,
}

impl PageCache {
    pub fn page_of(row: u64) -> u64 {
        row / PAGE_SIZE
    }

    /// Forgets everything and starts tracking `frame_id`.
    pub fn reset(&mut self, frame_id: u64) {
        self.frame_id = frame_id;
        self.pages.clear();
        self.order.clear();
        self.inflight.clear();
        self.last_range = 0..0;
    }

    /// The cell at `(row, col)`: `None` when its page isn't loaded, otherwise
    /// the text, or `Some(None)` for SQL NULL.
    pub fn cell(&self, row: u64, col: usize) -> Option<Option<&str>> {
        let page = self.pages.get(&Self::page_of(row))?;
        let local = (row - page.offset) as usize;
        let cell = page.rows.get(local)?.get(col)?;
        Some(cell.as_deref())
    }

    /// Whole loaded row, for copying.
    pub fn row(&self, row: u64) -> Option<&[Option<String>]> {
        let page = self.pages.get(&Self::page_of(row))?;
        page.rows
            .get((row - page.offset) as usize)
            .map(|r| r.as_slice())
    }

    #[cfg(test)]
    pub fn has(&self, page: u64) -> bool {
        self.pages.contains_key(&page)
    }

    /// Stores a fetched page if it belongs to the current frame.
    pub fn insert(&mut self, frame_id: u64, page: Page) {
        let ix = Self::page_of(page.offset);
        self.inflight.remove(&ix);
        if frame_id != self.frame_id {
            return;
        }
        self.touch(ix);
        self.pages.insert(ix, Arc::new(page));
        while self.pages.len() > CAPACITY {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.pages.remove(&old);
        }
    }

    /// A fetch failed or was dropped; allow it to be requested again.
    pub fn abandon(&mut self, frame_id: u64, page: u64) {
        if frame_id == self.frame_id {
            self.inflight.remove(&page);
        }
    }

    /// Pages to fetch for `visible` rows: the ones covering the range, plus one
    /// page ahead in the direction the user is scrolling. Already loaded or
    /// in-flight pages are skipped and the returned ones are marked in flight.
    pub fn plan(&mut self, visible: Range<usize>, total_rows: u64) -> Vec<u64> {
        if visible.is_empty() || total_rows == 0 {
            self.last_range = visible;
            return Vec::new();
        }
        let last_page = Self::page_of(total_rows - 1);
        let first = Self::page_of(visible.start as u64);
        let last = Self::page_of(visible.end.saturating_sub(1) as u64).min(last_page);
        let mut wanted: Vec<u64> = (first..=last).collect();
        if visible.start >= self.last_range.start {
            if last < last_page {
                wanted.push(last + 1);
            }
        } else if first > 0 {
            wanted.push(first - 1);
        }
        self.last_range = visible;

        wanted.retain(|p| !self.pages.contains_key(p) && !self.inflight.contains(p));
        for p in &wanted {
            self.inflight.insert(*p);
        }
        // Keep visible pages fresh in the LRU so prefetching can't evict them.
        for p in first..=last {
            if self.pages.contains_key(&p) {
                self.touch(p);
            }
        }
        wanted
    }

    fn touch(&mut self, page: u64) {
        if let Some(pos) = self.order.iter().position(|p| *p == page) {
            self.order.remove(pos);
        }
        self.order.push_back(page);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(ix: u64) -> Page {
        Page {
            offset: ix * PAGE_SIZE,
            rows: (0..PAGE_SIZE)
                .map(|r| vec![Some(format!("{}", ix * PAGE_SIZE + r)), None])
                .collect(),
        }
    }

    #[test]
    fn serves_cells_and_nulls() {
        let mut cache = PageCache::default();
        cache.reset(7);
        assert_eq!(cache.cell(3, 0), None);
        cache.insert(7, page(0));
        assert_eq!(cache.cell(3, 0), Some(Some("3")));
        assert_eq!(cache.cell(3, 1), Some(None));
        assert_eq!(cache.cell(PAGE_SIZE, 0), None);
    }

    #[test]
    fn ignores_pages_from_old_frames() {
        let mut cache = PageCache::default();
        cache.reset(1);
        cache.insert(2, page(0));
        assert!(!cache.has(0));
        cache.reset(2);
        cache.insert(2, page(0));
        assert!(cache.has(0));
    }

    #[test]
    fn plans_visible_pages_and_prefetch() {
        let mut cache = PageCache::default();
        cache.reset(1);
        let total = 10 * PAGE_SIZE;
        // Scrolling down from the top: visible pages plus the next one.
        assert_eq!(cache.plan(0..40, total), vec![0, 1]);
        // In flight now, so asking again plans nothing new.
        assert_eq!(cache.plan(0..40, total), Vec::<u64>::new());
        cache.insert(1, page(0));
        cache.insert(1, page(1));
        // Scrolling up: prefetch the page before.
        let start = (3 * PAGE_SIZE) as usize;
        assert_eq!(cache.plan(start..start + 40, total), vec![3, 4]);
        assert_eq!(cache.plan(start - 10..start + 30, total), vec![2]);
        // Never plans past the last page.
        let end = (total - 10) as usize;
        assert_eq!(cache.plan(end..total as usize, total), vec![9]);
    }

    #[test]
    fn evicts_least_recently_used() {
        let mut cache = PageCache::default();
        cache.reset(1);
        for ix in 0..CAPACITY as u64 + 2 {
            cache.insert(1, page(ix));
        }
        assert!(!cache.has(0));
        assert!(!cache.has(1));
        assert!(cache.has(2));
        assert!(cache.has(CAPACITY as u64 + 1));
    }
}
