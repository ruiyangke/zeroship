//! Dynamic counter registry — frozen Vec<AtomicU64> with O(1) ResourceHandle access.
//!
//! Two-phase design per spec v3.0 §2.4.1:
//! - Registration phase (startup): plugins call register(), get a ResourceHandle(usize)
//! - Runtime phase: increment(handle, delta) is a single fetch_add — no lock, no hash

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Opaque handle to a counter slot. O(1) access, no lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceHandle(pub(crate) usize);

/// Metadata for a registered resource.
#[derive(Debug, Clone)]
pub struct ResourceMeta {
    pub name: String,
    pub unit: String,
    pub aggregation: Aggregation,
    pub category: String,
}

/// How values combine across events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregation {
    Sum,
    Max,
    Latest,
    Gauge,
}

/// Builder for CounterRegistry — mutable during registration, frozen for runtime.
pub struct RegistryBuilder {
    resources: Vec<ResourceMeta>,
    name_to_index: HashMap<String, usize>,
}

impl RegistryBuilder {
    pub fn new() -> Self {
        Self {
            resources: Vec::new(),
            name_to_index: HashMap::new(),
        }
    }

    /// Register a resource. Returns a handle for O(1) runtime access.
    /// Panics if name is already registered (name collision = fatal at startup).
    pub fn register(&mut self, meta: ResourceMeta) -> ResourceHandle {
        let name = meta.name.clone();
        assert!(
            !self.name_to_index.contains_key(&name),
            "Resource name collision: '{name}' already registered"
        );
        let index = self.resources.len();
        self.name_to_index.insert(name, index);
        self.resources.push(meta);
        ResourceHandle(index)
    }

    /// Register core resources (requests, cpu_us, wall_us, egress_bytes, ingress_bytes).
    /// Returns handles in order.
    pub fn register_core(&mut self) -> CoreHandles {
        CoreHandles {
            requests: self.register(ResourceMeta {
                name: "requests".into(),
                unit: "count".into(),
                aggregation: Aggregation::Sum,
                category: "compute".into(),
            }),
            cpu_us: self.register(ResourceMeta {
                name: "cpu_us".into(),
                unit: "microseconds".into(),
                aggregation: Aggregation::Sum,
                category: "compute".into(),
            }),
            wall_us: self.register(ResourceMeta {
                name: "wall_us".into(),
                unit: "microseconds".into(),
                aggregation: Aggregation::Sum,
                category: "compute".into(),
            }),
            egress_bytes: self.register(ResourceMeta {
                name: "egress_bytes".into(),
                unit: "bytes".into(),
                aggregation: Aggregation::Sum,
                category: "network".into(),
            }),
            ingress_bytes: self.register(ResourceMeta {
                name: "ingress_bytes".into(),
                unit: "bytes".into(),
                aggregation: Aggregation::Sum,
                category: "network".into(),
            }),
        }
    }

    /// Freeze the builder into an immutable CounterRegistry.
    pub fn build(self) -> CounterRegistry {
        let counters: Vec<AtomicU64> = (0..self.resources.len())
            .map(|_| AtomicU64::new(0))
            .collect();
        CounterRegistry {
            counters: counters.into_boxed_slice(),
            name_to_index: self.name_to_index,
            resources: self.resources,
        }
    }
}

/// Handles for the 5 core resources, always at indices 0..4.
#[derive(Debug, Clone, Copy)]
pub struct CoreHandles {
    pub requests: ResourceHandle,
    pub cpu_us: ResourceHandle,
    pub wall_us: ResourceHandle,
    pub egress_bytes: ResourceHandle,
    pub ingress_bytes: ResourceHandle,
}

/// Immutable counter registry. All mutation is via atomic operations on the counters.
pub struct CounterRegistry {
    /// Dense array of atomic counters indexed by ResourceHandle.
    counters: Box<[AtomicU64]>,
    /// Name → index mapping for slow-path lookups (enforcer, flusher).
    name_to_index: HashMap<String, usize>,
    /// Resource metadata (for display, flushing).
    resources: Vec<ResourceMeta>,
}

impl appbase_core::plugin::PluginMeter for CounterRegistry {
    fn increment(&self, resource_name: &str, delta: u64) {
        if let Some(&idx) = self.name_to_index.get(resource_name) {
            self.counters[idx].fetch_add(delta, Ordering::Release);
        }
        // Unknown resource names are silently ignored at runtime
        // (should have been caught at registration time)
    }
}

impl CounterRegistry {
    /// Fast path: O(1) atomic increment by handle. No locks.
    #[inline]
    pub fn increment(&self, handle: ResourceHandle, delta: u64) {
        self.counters[handle.0].fetch_add(delta, Ordering::Release);
    }

    /// Fast path: O(1) atomic read by handle.
    #[inline]
    pub fn load(&self, handle: ResourceHandle) -> u64 {
        self.counters[handle.0].load(Ordering::Acquire)
    }

    /// Slow path: name-based lookup (for enforcer checking quotas by resource name).
    pub fn get(&self, name: &str) -> Option<u64> {
        self.name_to_index
            .get(name)
            .map(|&idx| self.counters[idx].load(Ordering::Acquire))
    }

    /// Snapshot all counters as name → value map (for flusher, billing).
    pub fn snapshot(&self) -> HashMap<String, u64> {
        let mut map = HashMap::with_capacity(self.resources.len());
        for (i, meta) in self.resources.iter().enumerate() {
            let val = self.counters[i].load(Ordering::Acquire);
            if val > 0 {
                map.insert(meta.name.clone(), val);
            }
        }
        map
    }

    /// Swap all counters to zero and return deltas (for flusher).
    pub fn swap_all(&self) -> HashMap<String, u64> {
        let mut map = HashMap::with_capacity(self.resources.len());
        for (i, meta) in self.resources.iter().enumerate() {
            let val = self.counters[i].swap(0, Ordering::AcqRel);
            if val > 0 {
                map.insert(meta.name.clone(), val);
            }
        }
        map
    }

    /// Number of registered resources.
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    /// Check if registry is empty.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// Get resource metadata by name.
    pub fn resource_meta(&self, name: &str) -> Option<&ResourceMeta> {
        self.name_to_index
            .get(name)
            .map(|&idx| &self.resources[idx])
    }

    /// Iterate all resource names.
    pub fn resource_names(&self) -> impl Iterator<Item = &str> {
        self.resources.iter().map(|m| m.name.as_str())
    }

    /// Reset all counters to zero (for period rollover).
    pub fn reset_all(&self) {
        for counter in self.counters.iter() {
            counter.store(0, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_increment() {
        let mut builder = RegistryBuilder::new();
        let h = builder.register(ResourceMeta {
            name: "requests".into(),
            unit: "count".into(),
            aggregation: Aggregation::Sum,
            category: "compute".into(),
        });
        let reg = builder.build();
        reg.increment(h, 5);
        assert_eq!(reg.load(h), 5);
        assert_eq!(reg.get("requests"), Some(5));
    }

    #[test]
    fn core_handles() {
        let mut builder = RegistryBuilder::new();
        let core = builder.register_core();
        assert_eq!(core.requests.0, 0);
        assert_eq!(core.cpu_us.0, 1);
        assert_eq!(core.wall_us.0, 2);
        assert_eq!(core.egress_bytes.0, 3);
        assert_eq!(core.ingress_bytes.0, 4);
    }

    #[test]
    fn snapshot_skips_zeros() {
        let mut builder = RegistryBuilder::new();
        let h1 = builder.register(ResourceMeta {
            name: "a".into(),
            unit: "x".into(),
            aggregation: Aggregation::Sum,
            category: "c".into(),
        });
        let _h2 = builder.register(ResourceMeta {
            name: "b".into(),
            unit: "x".into(),
            aggregation: Aggregation::Sum,
            category: "c".into(),
        });
        let reg = builder.build();
        reg.increment(h1, 10);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap["a"], 10);
    }

    #[test]
    fn swap_all_resets() {
        let mut builder = RegistryBuilder::new();
        let h = builder.register(ResourceMeta {
            name: "x".into(),
            unit: "y".into(),
            aggregation: Aggregation::Sum,
            category: "c".into(),
        });
        let reg = builder.build();
        reg.increment(h, 42);
        let deltas = reg.swap_all();
        assert_eq!(deltas["x"], 42);
        assert_eq!(reg.load(h), 0);
    }

    #[test]
    #[should_panic(expected = "name collision")]
    fn duplicate_name_panics() {
        let mut builder = RegistryBuilder::new();
        let meta = ResourceMeta {
            name: "dup".into(),
            unit: "x".into(),
            aggregation: Aggregation::Sum,
            category: "c".into(),
        };
        builder.register(meta.clone());
        builder.register(meta);
    }

    #[test]
    fn get_unknown_returns_none() {
        let builder = RegistryBuilder::new();
        let reg = builder.build();
        assert_eq!(reg.get("nonexistent"), None);
    }
}
