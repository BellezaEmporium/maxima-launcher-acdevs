use std::{any::Any, borrow::Borrow, hash::Hash, sync::Arc, time::Duration};

use moka::sync::Cache;

/// A cache keyed by `K` whose values are type-erased via `dyn Any`.
///
/// Note that values are cloned when retrieved. Because the value type isn't
/// part of `K`, nothing prevents two call sites from using the same key with
/// different `T`s; `get` guards against this by returning `None` on a type
/// mismatch rather than panicking, but such mismatches still won't be caught
/// at compile time. Callers should keep cache keys unambiguous (e.g. by
/// namespacing them per value type).
pub struct DynamicCache<K> {
    cache: Cache<K, Arc<dyn Any + Sync + Send>>,
}

impl<K: Eq + Hash + Sync + Send + 'static> DynamicCache<K> {
    pub fn new(capacity: u64, time_to_live: Duration, time_to_idle: Duration) -> Self {
        let cache = Cache::builder()
            .max_capacity(capacity)
            .time_to_live(time_to_live)
            .time_to_idle(time_to_idle)
            .build();

        Self { cache }
    }

    pub fn insert<T>(&self, key: K, request: T)
    where
        T: Sync + Send + Clone + 'static,
    {
        self.cache.insert(key, Arc::new(request));
    }

    pub fn get<Q, T>(&self, key: &Q) -> Option<T>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
        T: Sync + Send + Clone + 'static,
    {
        self.cache
            .get(key)
            .and_then(|cached| cached.downcast_ref::<T>().cloned())
    }
}
