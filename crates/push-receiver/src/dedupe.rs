use std::collections::{HashSet, VecDeque};

pub const MAX_DEDUPE_CAPACITY: usize = 16_384;
pub const MAX_DEDUPE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_PERSISTENT_ID_BYTES: usize = 4 * 1024;

#[derive(Clone)]
pub struct PersistentIdWindow {
    capacity: usize,
    byte_budget: usize,
    bytes: usize,
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
        let byte_budget = MAX_DEDUPE_BYTES;
        Self {
            capacity,
            byte_budget,
            bytes: 0,
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
        self.bytes
    }
    /// Returns true when `id` was already observed; new IDs are retained up to
    /// both the configured count and the hard byte bound.
    pub fn observe(&mut self, id: &str) -> bool {
        if self.capacity == 0 || id.is_empty() || id.len() > MAX_PERSISTENT_ID_BYTES {
            return false;
        }
        if self.seen.contains(id) {
            return true;
        }
        while self.order.len() >= self.capacity
            || self.bytes.saturating_add(id.len()) > self.byte_budget
        {
            if let Some(old) = self.order.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.len());
                self.seen.remove(&old);
            } else {
                return false;
            }
        }
        let value = id.to_owned();
        self.bytes = self.bytes.saturating_add(value.len());
        self.seen.insert(value.clone());
        self.order.push_back(value);
        false
    }
    pub fn clear(&mut self) {
        self.order.clear();
        self.seen.clear();
        self.bytes = 0;
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
    #[test]
    fn rejects_oversized_ids_and_keeps_byte_budget() {
        let mut window = PersistentIdWindow::new(MAX_DEDUPE_CAPACITY);
        assert!(!window.observe(&"x".repeat(MAX_PERSISTENT_ID_BYTES + 1)));
        for index in 0..MAX_DEDUPE_CAPACITY {
            let _ = window.observe(&format!("{index:04}-{}", "y".repeat(4090)));
        }
        assert!(window.bytes() <= MAX_DEDUPE_BYTES);
    }
}
