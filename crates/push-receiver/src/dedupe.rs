use std::collections::{HashSet, VecDeque};

pub const MAX_DEDUPE_CAPACITY: usize = 16_384;

#[derive(Clone)]
pub struct PersistentIdWindow {
    capacity: usize,
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl std::fmt::Debug for PersistentIdWindow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistentIdWindow")
            .field("capacity", &self.capacity)
            .field("count", &self.order.len())
            .field("bytes", &self.bytes())
            .finish()
    }
}
impl PersistentIdWindow {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.min(MAX_DEDUPE_CAPACITY);
        Self {
            capacity,
            order: VecDeque::with_capacity(capacity.min(1024)),
            seen: HashSet::with_capacity(capacity.min(1024)),
        }
    }
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.order.iter().cloned().collect()
    }
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.order.iter().map(String::len).sum()
    }
    /// Returns true when `id` was already observed; new IDs are retained up to the configured bound.
    pub fn observe(&mut self, id: &str) -> bool {
        if self.capacity == 0 {
            return false;
        }
        if self.seen.contains(id) {
            return true;
        }
        self.seen.insert(id.to_owned());
        self.order.push_back(id.to_owned());
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        false
    }
    pub fn clear(&mut self) {
        self.order.clear();
        self.seen.clear();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redelivery_is_deduplicated_with_bounded_eviction() {
        let mut window = PersistentIdWindow::new(2);
        assert!(!window.observe("a"));
        assert!(window.observe("a"));
        assert!(!window.observe("b"));
        assert!(!window.observe("c"));
        assert!(!window.observe("a"));
    }
}
