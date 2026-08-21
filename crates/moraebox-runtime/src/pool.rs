use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    hash::Hash,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{sync::Mutex, task::JoinHandle, time::MissedTickBehavior};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PreparedKey {
    pub image_digest: String,
    pub workspace_digest: Option<String>,
    pub policy_digest: String,
}

#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    pub max_size: usize,
    pub idle_ttl: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: 4,
            idle_ttl: Duration::from_secs(5 * 60),
        }
    }
}

pub struct PreparedPool<K, V> {
    config: PoolConfig,
    state: Mutex<PoolState<K, V>>,
}

impl<K, V> std::fmt::Debug for PreparedPool<K, V> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedPool")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<K, V> PreparedPool<K, V>
where
    K: Eq + Hash + Clone,
{
    pub fn new(config: PoolConfig) -> Result<Self, PoolError> {
        if config.max_size == 0 {
            return Err(PoolError::ZeroCapacity);
        }
        if config.idle_ttl.is_zero() {
            return Err(PoolError::ZeroTtl);
        }
        Ok(Self {
            config,
            state: Mutex::new(PoolState::default()),
        })
    }

    pub async fn put(&self, key: K, value: V) -> Result<(), PoolError> {
        let mut state = self.state.lock().await;
        state.prune(Instant::now());
        if state.len >= self.config.max_size {
            return Err(PoolError::Full(self.config.max_size));
        }
        state.units.entry(key).or_default().push_back(PreparedUnit {
            value,
            expires_at: Instant::now() + self.config.idle_ttl,
        });
        state.len += 1;
        Ok(())
    }

    pub async fn lease(&self, key: &K) -> Option<PreparedLease<V>> {
        let mut state = self.state.lock().await;
        state.prune(Instant::now());
        let (unit, empty) = {
            let queue = state.units.get_mut(key)?;
            let unit = queue.pop_front()?;
            (unit, queue.is_empty())
        };
        state.len -= 1;
        if empty {
            state.units.remove(key);
        }
        Some(PreparedLease {
            value: Some(unit.value),
        })
    }

    pub async fn stats(&self) -> PoolStats {
        let mut state = self.state.lock().await;
        state.prune(Instant::now());
        PoolStats {
            ready: state.len,
            keys: state.units.len(),
            capacity: self.config.max_size,
        }
    }

    pub async fn replenish<F, Fut, E>(&self, key: K, target: usize, factory: F) -> Result<usize, E>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let current = {
            let mut state = self.state.lock().await;
            state.prune(Instant::now());
            state.units.get(&key).map_or(0, VecDeque::len)
        };
        let target = target.min(self.config.max_size);
        let mut added = 0;
        for _ in current..target {
            let value = factory().await?;
            if self.put(key.clone(), value).await.is_err() {
                break;
            }
            added += 1;
        }
        Ok(added)
    }

    pub fn spawn_replenisher<F, Fut, E>(
        pool: &Arc<Self>,
        key: K,
        target: usize,
        interval: Duration,
        factory: F,
    ) -> JoinHandle<()>
    where
        K: Send + Sync + 'static,
        V: Send + 'static,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<V, E>> + Send,
        E: Send + 'static,
    {
        let pool: Weak<Self> = Arc::downgrade(pool);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let Some(pool) = pool.upgrade() else {
                    return;
                };
                let _ = pool.replenish(key.clone(), target, &factory).await;
            }
        })
    }
}

pub struct PreparedLease<V> {
    value: Option<V>,
}

impl<V> PreparedLease<V> {
    pub fn get(&self) -> &V {
        self.value.as_ref().expect("lease value is present")
    }

    pub fn into_inner(mut self) -> V {
        self.value.take().expect("lease value is present")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolStats {
    pub ready: usize,
    pub keys: usize,
    pub capacity: usize,
}

struct PreparedUnit<V> {
    value: V,
    expires_at: Instant,
}

struct PoolState<K, V> {
    units: HashMap<K, VecDeque<PreparedUnit<V>>>,
    len: usize,
}

impl<K, V> Default for PoolState<K, V> {
    fn default() -> Self {
        Self {
            units: HashMap::new(),
            len: 0,
        }
    }
}

impl<K, V> PoolState<K, V>
where
    K: Eq + Hash,
{
    fn prune(&mut self, now: Instant) {
        self.units.retain(|_, queue| {
            let before = queue.len();
            queue.retain(|unit| unit.expires_at > now);
            self.len -= before - queue.len();
            !queue.is_empty()
        });
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("prepared pool capacity must be non-zero")]
    ZeroCapacity,
    #[error("prepared pool idle TTL must be non-zero")]
    ZeroTtl,
    #[error("prepared pool is full (capacity {0})")]
    Full(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lease_is_single_use_and_capacity_is_bounded() {
        let pool = PreparedPool::new(PoolConfig {
            max_size: 1,
            idle_ttl: Duration::from_secs(1),
        })
        .unwrap();
        pool.put("key", 7).await.unwrap();
        assert_eq!(pool.put("key", 8).await, Err(PoolError::Full(1)));
        let lease = pool.lease(&"key").await.unwrap();
        assert_eq!(lease.into_inner(), 7);
        assert!(pool.lease(&"key").await.is_none());
    }

    #[tokio::test]
    async fn lease_never_crosses_prepared_keys() {
        let pool = PreparedPool::new(PoolConfig {
            max_size: 2,
            idle_ttl: Duration::from_secs(1),
        })
        .unwrap();
        pool.put("image-a", 7).await.unwrap();

        assert!(pool.lease(&"image-b").await.is_none());
        assert_eq!(pool.lease(&"image-a").await.unwrap().into_inner(), 7);
    }

    #[tokio::test]
    async fn expired_units_are_destroyed() {
        let pool = PreparedPool::new(PoolConfig {
            max_size: 1,
            idle_ttl: Duration::from_millis(5),
        })
        .unwrap();
        pool.put("key", 7).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(pool.stats().await.ready, 0);
    }

    #[tokio::test]
    async fn replenishes_only_to_target() {
        let pool = PreparedPool::new(PoolConfig {
            max_size: 4,
            idle_ttl: Duration::from_secs(1),
        })
        .unwrap();
        let added = pool
            .replenish("key", 2, || async { Ok::<_, ()>(7) })
            .await
            .unwrap();
        assert_eq!(added, 2);
        assert_eq!(pool.stats().await.ready, 2);
    }
}
