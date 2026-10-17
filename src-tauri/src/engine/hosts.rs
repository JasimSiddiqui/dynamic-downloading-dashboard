use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const WEEK_MS: u64 = 7 * 24 * 3600 * 1000;

#[derive(Serialize, Deserialize, Clone, Copy)]
struct Entry {
    cap: u32,
    at: u64,
}

pub struct Hosts {
    path: PathBuf,
    map: Mutex<HashMap<String, Entry>>,
}

impl Hosts {
    pub fn load(path: PathBuf) -> Self {
        let now = crate::store::now();
        let map: HashMap<String, Entry> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let map = map.into_iter().filter(|(_, e)| now.saturating_sub(e.at) < WEEK_MS).collect();
        Self { path, map: Mutex::new(map) }
    }

    pub fn cap(&self, host: &str) -> Option<u32> {
        self.map.lock().get(host).map(|e| e.cap)
    }

    pub fn limit(&self, host: &str, cap: u32) {
        let cap = cap.max(1);
        let mut map = self.map.lock();
        let e = map.entry(host.to_string()).or_insert(Entry { cap, at: 0 });
        e.cap = e.cap.min(cap);
        e.at = crate::store::now();
        let _ = std::fs::write(&self.path, serde_json::to_vec(&*map).unwrap_or_default());
    }

    pub fn raise(&self, host: &str, seen: u32) {
        let mut map = self.map.lock();
        if let Some(e) = map.get_mut(host) {
            if seen >= e.cap && crate::store::now().saturating_sub(e.at) > 3600 * 1000 {
                e.cap = seen + 1;
                e.at = crate::store::now();
                let _ = std::fs::write(&self.path, serde_json::to_vec(&*map).unwrap_or_default());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_off_only_lowers() {
        let dir = tempfile::tempdir().unwrap();
        let h = Hosts::load(dir.path().join("h.json"));
        assert_eq!(h.cap("a.com"), None);
        h.limit("a.com", 6);
        h.limit("a.com", 9);
        assert_eq!(h.cap("a.com"), Some(6));
        h.limit("a.com", 0);
        assert_eq!(h.cap("a.com"), Some(1));
        let again = Hosts::load(dir.path().join("h.json"));
        assert_eq!(again.cap("a.com"), Some(1));
    }
}
