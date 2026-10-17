use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::Arc;

pub const MIN_SLICE: u64 = 1 << 20;

pub struct Seg {
    pub start: u64,
    pub pos: AtomicU64,
    pub end: AtomicU64,
    pub written: AtomicU64,
    busy: AtomicBool,
}

impl Seg {
    fn new(start: u64, end: u64, busy: bool) -> Arc<Self> {
        Arc::new(Self {
            start,
            pos: AtomicU64::new(start),
            end: AtomicU64::new(end),
            written: AtomicU64::new(start),
            busy: AtomicBool::new(busy),
        })
    }

    pub fn left(&self) -> u64 {
        self.end.load(Acquire).saturating_sub(self.pos.load(Acquire))
    }

    pub fn unwritten(&self) -> Option<(u64, u64)> {
        let w = self.written.load(Acquire);
        let e = self.end.load(Acquire);
        (w < e).then_some((w, e))
    }

    pub fn advance(&self, to: u64) {
        self.pos.fetch_max(to, AcqRel);
    }

    pub fn wrote(&self, to: u64) {
        self.written.fetch_max(to, AcqRel);
    }
}

pub struct Plan {
    pub size: u64,
    segs: Mutex<Vec<Arc<Seg>>>,
}

impl Plan {
    pub fn new(size: u64) -> Self {
        Self::from_holes(size, &[(0, size)])
    }

    pub fn from_holes(size: u64, holes: &[(u64, u64)]) -> Self {
        let segs = holes
            .iter()
            .filter(|(s, e)| s < e && *e <= size)
            .map(|&(s, e)| Seg::new(s, e, false))
            .collect();
        Self { size, segs: Mutex::new(segs) }
    }

    pub fn claim(&self) -> Option<Arc<Seg>> {
        let mut segs = self.segs.lock();
        if let Some(s) = segs.iter().find(|s| s.left() > 0 && !s.busy.load(Acquire)) {
            s.busy.store(true, Release);
            return Some(s.clone());
        }
        let victim = segs
            .iter()
            .filter(|s| s.left() >= 2 * MIN_SLICE)
            .max_by_key(|s| s.left())?
            .clone();
        let pos = victim.pos.load(Acquire);
        let end = victim.end.load(Acquire);
        let mid = pos + (end - pos) / 2;
        victim.end.fetch_min(mid, AcqRel);
        let start = mid.max(victim.pos.load(Acquire));
        if start >= end {
            return None;
        }
        let seg = Seg::new(start, end, true);
        segs.push(seg.clone());
        Some(seg)
    }

    pub fn release(&self, seg: &Seg) {
        seg.busy.store(false, Release);
    }

    pub fn splittable(&self) -> bool {
        let segs = self.segs.lock();
        segs.iter().any(|s| s.left() > 0 && !s.busy.load(Acquire)) || segs.iter().any(|s| s.left() >= 2 * MIN_SLICE)
    }

    pub fn received(&self) -> u64 {
        self.size - self.segs.lock().iter().map(|s| s.left()).sum::<u64>()
    }

    pub fn holes(&self) -> Vec<(u64, u64)> {
        let mut h: Vec<_> = self.segs.lock().iter().filter_map(|s| s.unwritten()).collect();
        h.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(h.len());
        for (s, e) in h {
            match out.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => out.push((s, e)),
            }
        }
        out
    }

    pub fn complete(&self) -> bool {
        self.segs.lock().iter().all(|s| s.unwritten().is_none())
    }

    pub fn lanes(&self) -> Vec<[f32; 3]> {
        if self.size == 0 {
            return Vec::new();
        }
        let f = self.size as f64;
        let mut v: Vec<[f32; 3]> = self
            .segs
            .lock()
            .iter()
            .filter(|s| s.busy.load(Acquire) && s.left() > 0)
            .map(|s| {
                [
                    (s.start as f64 / f) as f32,
                    (s.pos.load(Relaxed) as f64 / f) as f32,
                    (s.end.load(Relaxed) as f64 / f) as f32,
                ]
            })
            .collect();
        v.sort_by(|a, b| a[0].total_cmp(&b[0]));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = MIN_SLICE;

    #[test]
    fn first_claim_takes_whole_file() {
        let p = Plan::new(10 * MB);
        let s = p.claim().unwrap();
        assert_eq!((s.start, s.end.load(Relaxed)), (0, 10 * MB));
    }

    #[test]
    fn steals_half_of_largest() {
        let p = Plan::new(8 * MB);
        let a = p.claim().unwrap();
        a.advance(2 * MB);
        let b = p.claim().unwrap();
        assert_eq!(a.end.load(Relaxed), 5 * MB);
        assert_eq!((b.start, b.end.load(Relaxed)), (5 * MB, 8 * MB));
        let c = p.claim().unwrap();
        assert_eq!(c.start, 6 * MB + MB / 2);
        assert_eq!(b.end.load(Relaxed), c.start);
    }

    #[test]
    fn no_steal_below_min_slice() {
        let p = Plan::new(MB + MB / 2);
        let _a = p.claim().unwrap();
        assert!(p.claim().is_none());
        assert!(!p.splittable());
    }

    #[test]
    fn holes_cover_everything_unwritten() {
        let p = Plan::new(8 * MB);
        let a = p.claim().unwrap();
        a.advance(3 * MB);
        a.wrote(3 * MB);
        let b = p.claim().unwrap();
        b.advance(6 * MB);
        b.wrote(6 * MB);
        assert_eq!(p.holes(), vec![(3 * MB, b.start), (6 * MB, 8 * MB)]);
        a.advance(b.start);
        a.wrote(b.start);
        b.advance(8 * MB);
        b.wrote(8 * MB);
        assert!(p.complete());
        assert!(p.holes().is_empty());
        assert_eq!(p.received(), 8 * MB);
    }

    #[test]
    fn resume_from_holes() {
        let p = Plan::from_holes(10 * MB, &[(2 * MB, 4 * MB), (7 * MB, 10 * MB)]);
        assert_eq!(p.received(), 5 * MB);
        let a = p.claim().unwrap();
        let b = p.claim().unwrap();
        assert_eq!(a.start, 2 * MB);
        assert_eq!(b.start, 7 * MB);
    }

    #[test]
    fn concurrent_claims_cover_file_exactly() {
        let size = 64 * MB + 12345;
        let p = Arc::new(Plan::new(size));
        let hits = Arc::new(Mutex::new(vec![0u8; (size / 4096 + 1) as usize]));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let p = p.clone();
                let hits = hits.clone();
                std::thread::spawn(move || {
                    while let Some(seg) = p.claim() {
                        loop {
                            let pos = seg.pos.load(Acquire);
                            let end = seg.end.load(Acquire);
                            if pos >= end {
                                break;
                            }
                            let take = (end - pos).min(64 * 1024);
                            seg.advance(pos + take);
                            seg.wrote(pos + take);
                            let mut h = hits.lock();
                            let mut o = pos;
                            while o < pos + take {
                                h[(o / 4096) as usize] = 1;
                                o += 4096;
                            }
                        }
                        p.release(&seg);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(p.complete());
        assert!(hits.lock()[..(size / 4096) as usize].iter().all(|&h| h == 1));
    }
}
