use std::collections::VecDeque;

/// Small LRU with explicit weight and entry limits. Oversized values stay uncached.
pub struct Cache<K, V> {
    entries: VecDeque<(K, V, usize)>,
    bytes: usize,
    budget: usize,
    capacity: usize,
}

impl<K: PartialEq, V: Clone> Cache<K, V> {
    pub fn new(budget: usize, capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            budget,
            capacity,
        }
    }
    pub fn get(&mut self, key: &K) -> Option<V> {
        let index = self.entries.iter().position(|(k, _, _)| k == key)?;
        let entry = self.entries.remove(index)?;
        let value = entry.1.clone();
        self.entries.push_back(entry);
        Some(value)
    }
    pub fn insert(&mut self, key: K, value: V, weight: usize) {
        if weight > self.budget || self.capacity == 0 {
            return;
        }
        if let Some(index) = self.entries.iter().position(|(k, _, _)| *k == key) {
            if let Some((_, _, old)) = self.entries.remove(index) {
                self.bytes -= old;
            }
        }
        while self.bytes + weight > self.budget || self.entries.len() >= self.capacity {
            if let Some((_, _, old)) = self.entries.pop_front() {
                self.bytes -= old;
            } else {
                break;
            }
        }
        self.bytes += weight;
        self.entries.push_back((key, value, weight));
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.entries.retain(|(key, _, _)| keep(key));
        self.bytes = self.entries.iter().map(|(_, _, weight)| *weight).sum();
    }
}
