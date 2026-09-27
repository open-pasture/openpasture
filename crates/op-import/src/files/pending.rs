//! Previews waiting for their commit, in memory for 30 minutes. A preview
//! holds either the paddock drafts or the uploaded position file; the whole
//! set is capped in bytes, oldest dropped first.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::read::Draft;

pub const TTL: Duration = Duration::from_secs(30 * 60);
/// Most bytes all waiting previews may hold together.
const MAX_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug)]
pub enum Pending {
    Paddocks { drafts: Vec<Draft> },
    Positions { file_name: String, bytes: Arc<Vec<u8>> },
}

impl Pending {
    fn size(&self) -> usize {
        match self {
            Pending::Paddocks { drafts } => drafts.iter().map(|d| 256 + d.geometry.coordinates.iter().map(Vec::len).sum::<usize>() * 16).sum(),
            Pending::Positions { bytes, .. } => bytes.len(),
        }
    }
}

struct Entry {
    at: Instant,
    item: Arc<Pending>,
}

fn map() -> &'static Mutex<HashMap<String, Entry>> {
    static MAP: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prune(m: &mut HashMap<String, Entry>, now: Instant) {
    m.retain(|_, e| now.duration_since(e.at) < TTL);
    let mut total: usize = m.values().map(|e| e.item.size()).sum();
    while total > MAX_BYTES {
        let Some(oldest) = m.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone()) else { break };
        if let Some(e) = m.remove(&oldest) {
            total -= e.item.size();
        }
    }
}

pub fn put(id: &str, item: Pending) {
    let mut m = map().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    m.insert(id.to_owned(), Entry { at: now, item: Arc::new(item) });
    prune(&mut m, now);
}

/// Put back a preview taken by a commit that failed.
pub fn restore(id: &str, item: Arc<Pending>) {
    let mut m = map().lock().unwrap_or_else(|e| e.into_inner());
    m.insert(id.to_owned(), Entry { at: Instant::now(), item });
}

pub fn get(id: &str) -> Option<Arc<Pending>> {
    let mut m = map().lock().unwrap_or_else(|e| e.into_inner());
    prune(&mut m, Instant::now());
    m.get(id).map(|e| e.item.clone())
}

pub fn take(id: &str) -> Option<Arc<Pending>> {
    let mut m = map().lock().unwrap_or_else(|e| e.into_inner());
    prune(&mut m, Instant::now());
    m.remove(id).map(|e| e.item)
}
