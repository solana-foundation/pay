//! Durable MPP replay store shared by every pay-api replica.
//!
//! `mpp_store_url` selects the backend: `redis://` / `rediss://` for the
//! shared Redis store, or the explicit `memory://` opt-in for a single
//! process (local development only).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pay_api_core::Error;
use pay_kit::mpp::store::{MemoryStore, Store, StoreError};

/// Records outlive every challenge, blockhash and pending-confirmation window.
const RECORD_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Connection to the configured store backend; namespaces are cheap views.
#[derive(Clone)]
pub enum StoreBackend {
    Redis(redis::aio::ConnectionManager),
    Memory,
}

impl StoreBackend {
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let url = url.trim();
        if url == "memory://" {
            tracing::warn!("mpp_store_url is memory://: replay state is process-local");
            return Ok(Self::Memory);
        }
        if !url.starts_with("redis://") && !url.starts_with("rediss://") {
            return Err(Error::SendNotConfigured(
                "mpp_store_url must be a redis:// or rediss:// URL, or memory://".into(),
            ));
        }
        let client = redis::Client::open(url)
            .map_err(|e| Error::SendNotConfigured(format!("invalid mpp_store_url: {e}")))?;
        let connection = client
            .get_connection_manager()
            .await
            .map_err(|e| Error::SendNotConfigured(format!("mpp store unreachable: {e}")))?;
        Ok(Self::Redis(connection))
    }

    /// A store whose keys live under `namespace`.
    pub fn namespace(&self, namespace: &str) -> Arc<dyn Store> {
        match self {
            Self::Redis(connection) => Arc::new(RedisStore {
                connection: connection.clone(),
                prefix: format!("pay-api:{namespace}:"),
            }),
            Self::Memory => Arc::new(MemoryStore::new()),
        }
    }
}

/// Short-lived exclusive reservations (e.g. one in-flight cancellation per
/// subscription), shared by every replica on the Redis backend.
pub enum Reservations {
    Redis {
        connection: redis::aio::ConnectionManager,
        prefix: String,
    },
    Memory(std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>),
}

/// A held reservation; `token` proves ownership on release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    key: String,
    token: String,
}

impl StoreBackend {
    pub fn reservations(&self, namespace: &str) -> Reservations {
        match self {
            Self::Redis(connection) => Reservations::Redis {
                connection: connection.clone(),
                prefix: format!("pay-api:{namespace}:"),
            },
            Self::Memory => Reservations::Memory(Default::default()),
        }
    }
}

impl Reservations {
    /// Take `key` for `ttl` unless someone else holds it.
    pub async fn reserve(
        &self,
        key: &str,
        ttl: std::time::Duration,
    ) -> Result<Option<Reservation>, StoreError> {
        let token = uuid::Uuid::new_v4().to_string();
        let acquired = match self {
            Self::Redis { connection, prefix } => {
                let mut connection = connection.clone();
                let set: Option<String> = redis::cmd("SET")
                    .arg(format!("{prefix}{key}"))
                    .arg(&token)
                    .arg("NX")
                    .arg("PX")
                    .arg(ttl.as_millis().max(1) as u64)
                    .query_async(&mut connection)
                    .await
                    .map_err(internal)?;
                set.is_some()
            }
            Self::Memory(entries) => {
                let mut entries = entries.lock().expect("reservations lock");
                let now = std::time::Instant::now();
                match entries.get(key) {
                    Some((_, expires)) if *expires > now => false,
                    _ => {
                        entries.insert(key.to_string(), (token.clone(), now + ttl));
                        true
                    }
                }
            }
        };
        Ok(acquired.then(|| Reservation {
            key: key.to_string(),
            token,
        }))
    }

    /// Release `reservation` if it is still the holder.
    pub async fn release(&self, reservation: &Reservation) -> Result<(), StoreError> {
        const RELEASE: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
end
return 0
"#;
        match self {
            Self::Redis { connection, prefix } => {
                let mut connection = connection.clone();
                redis::Script::new(RELEASE)
                    .key(format!("{prefix}{}", reservation.key))
                    .arg(&reservation.token)
                    .invoke_async::<i32>(&mut connection)
                    .await
                    .map_err(internal)?;
            }
            Self::Memory(entries) => {
                let mut entries = entries.lock().expect("reservations lock");
                if entries
                    .get(&reservation.key)
                    .is_some_and(|(token, _)| *token == reservation.token)
                {
                    entries.remove(&reservation.key);
                }
            }
        }
        Ok(())
    }
}

struct RedisStore {
    connection: redis::aio::ConnectionManager,
    prefix: String,
}

type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

fn internal(error: redis::RedisError) -> StoreError {
    StoreError::Internal(error.to_string())
}

fn encode(value: &serde_json::Value) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|e| StoreError::Serialization(e.to_string()))
}

// Every write goes through one script so retention is decided in one place.
//
// Append-only records (the charge replay chain) live as a root key `k` and
// generations `k:1`, `k:2`, ... A root that expired on its own while a
// descendant survived would read as never reserved and be recreated. So a
// record's keys expire together: each write refreshes the root and every
// generation to one deadline, and a root is never recreated while its first
// generation survives.
//
// Values are only ever written by this store as `serde_json::to_string`, which
// is canonical for a decoded value, so the stored bytes compare exactly.
const WRITE_SCRIPT: &str = r#"
local key, mode, expected, value, ttl = KEYS[1], ARGV[1], ARGV[2], ARGV[3], ARGV[4]
if mode == 'absent' then
  if redis.call('EXISTS', key) == 1 or redis.call('EXISTS', key .. ':1') == 1 then
    return 0
  end
elseif mode == 'eq' then
  if redis.call('GET', key) ~= expected then return 0 end
end
redis.call('SET', key, value, 'EX', ttl)
local root = string.match(key, '^(.*):%d+$')
if not root or redis.call('EXISTS', root) == 0 then root = key end
redis.call('EXPIRE', root, ttl)
local generation = 1
while generation <= 1024 and redis.call('EXPIRE', root .. ':' .. generation, ttl) == 1 do
  generation = generation + 1
end
return 1
"#;

impl RedisStore {
    /// Write `value` under `key` when `mode` allows: `put` always, `absent`
    /// only when the key (and any generation of it) is absent, `eq` only when
    /// it holds `expected`.
    async fn write(
        &self,
        key: &str,
        mode: &str,
        expected: Option<String>,
        value: &serde_json::Value,
    ) -> Result<bool, StoreError> {
        let mut connection = self.connection.clone();
        let written: i64 = redis::Script::new(WRITE_SCRIPT)
            .key(format!("{}{key}", self.prefix))
            .arg(mode)
            .arg(expected.unwrap_or_default())
            .arg(encode(value)?)
            .arg(RECORD_TTL_SECONDS)
            .invoke_async(&mut connection)
            .await
            .map_err(internal)?;
        Ok(written == 1)
    }
}

impl Store for RedisStore {
    fn get(&self, key: &str) -> StoreFuture<'_, Option<serde_json::Value>> {
        let mut connection = self.connection.clone();
        let key = format!("{}{key}", self.prefix);
        Box::pin(async move {
            let raw: Option<String> = redis::cmd("GET")
                .arg(key)
                .query_async(&mut connection)
                .await
                .map_err(internal)?;
            raw.map(|raw| {
                serde_json::from_str(&raw).map_err(|e| StoreError::Serialization(e.to_string()))
            })
            .transpose()
        })
    }

    fn put(&self, key: &str, value: serde_json::Value) -> StoreFuture<'_, ()> {
        let key = key.to_string();
        Box::pin(async move { self.write(&key, "put", None, &value).await.map(|_| ()) })
    }

    fn delete(&self, key: &str) -> StoreFuture<'_, ()> {
        let mut connection = self.connection.clone();
        let key = format!("{}{key}", self.prefix);
        Box::pin(async move {
            redis::cmd("DEL")
                .arg(key)
                .query_async::<()>(&mut connection)
                .await
                .map_err(internal)
        })
    }

    fn put_if_absent(&self, key: &str, value: serde_json::Value) -> StoreFuture<'_, bool> {
        let key = key.to_string();
        Box::pin(async move { self.write(&key, "absent", None, &value).await })
    }

    fn put_if_eq(
        &self,
        key: &str,
        expected: Option<serde_json::Value>,
        value: serde_json::Value,
    ) -> StoreFuture<'_, bool> {
        let key = key.to_string();
        Box::pin(async move {
            let (mode, expected) = match expected {
                Some(expected) => ("eq", Some(encode(&expected)?)),
                None => ("absent", None),
            };
            self.write(&key, mode, expected, &value).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::redeem::tests::flow::spawn_redis;
    use serde_json::json;

    #[tokio::test]
    async fn rejects_unknown_store_urls() {
        for url in ["", "postgres://db", "http://redis"] {
            assert!(StoreBackend::connect(url).await.is_err(), "{url}");
        }
        assert!(matches!(
            StoreBackend::connect("memory://").await,
            Ok(StoreBackend::Memory)
        ));
    }

    #[test]
    fn standalone_build_supports_tls_store_urls() {
        redis::Client::open("rediss://127.0.0.1:6380").expect("rediss:// needs a TLS transport");
    }

    async fn exercise_reservations(a: &Reservations, b: &Reservations) {
        let ttl = std::time::Duration::from_secs(60);
        let held = a
            .reserve("pda", ttl)
            .await
            .unwrap()
            .expect("first reserves");
        assert_eq!(b.reserve("pda", ttl).await.unwrap(), None);
        assert!(b.reserve("other", ttl).await.unwrap().is_some());

        // A stale holder cannot release a newer reservation.
        a.release(&held).await.unwrap();
        let newer = b.reserve("pda", ttl).await.unwrap().expect("released");
        a.release(&held).await.unwrap();
        assert_eq!(a.reserve("pda", ttl).await.unwrap(), None);
        b.release(&newer).await.unwrap();

        let short = a
            .reserve("expiring", std::time::Duration::from_millis(50))
            .await
            .unwrap();
        assert!(short.is_some());
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert!(b.reserve("expiring", ttl).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn memory_reservations_are_exclusive_and_expire() {
        let reservations = StoreBackend::Memory.reservations("cancel");
        exercise_reservations(&reservations, &reservations).await;
    }

    #[tokio::test]
    async fn redis_reservations_are_exclusive_across_replicas() {
        let Some(redis) = spawn_redis().await else {
            return;
        };
        let url = format!("redis://127.0.0.1:{}/", redis.1);
        let a = StoreBackend::connect(&url)
            .await
            .unwrap()
            .reservations("cancel");
        let b = StoreBackend::connect(&url)
            .await
            .unwrap()
            .reservations("cancel");
        exercise_reservations(&a, &b).await;
    }

    #[tokio::test]
    async fn redis_put_if_eq_compares_and_writes_atomically_across_replicas() {
        let Some(redis) = spawn_redis().await else {
            return;
        };
        let url = format!("redis://127.0.0.1:{}/", redis.1);
        let a = StoreBackend::connect(&url).await.unwrap().namespace("cas");
        let b = StoreBackend::connect(&url).await.unwrap().namespace("cas");
        let v1 = json!({ "z": 1, "a": { "note": "café", "n": 1.5 } });
        let v2 = json!({ "v": 2 });

        assert!(
            !a.put_if_eq("k", Some(v1.clone()), v2.clone())
                .await
                .unwrap()
        );
        assert!(a.put_if_eq("k", None, v1.clone()).await.unwrap());
        assert!(!b.put_if_eq("k", None, v2.clone()).await.unwrap());

        // A value read back from the store is a valid expectation.
        let read = b.get("k").await.unwrap();
        assert_eq!(read.as_ref(), Some(&v1));
        assert!(
            !b.put_if_eq("k", Some(v2.clone()), v2.clone())
                .await
                .unwrap()
        );

        // Two writers racing from the same read: exactly one lands.
        let (wa, wb) = tokio::join!(
            a.put_if_eq("k", read.clone(), json!("a")),
            b.put_if_eq("k", read.clone(), json!("b")),
        );
        assert!(wa.unwrap() ^ wb.unwrap());
        let winner = a.get("k").await.unwrap().unwrap();
        assert!(winner == json!("a") || winner == json!("b"));
    }

    #[tokio::test]
    async fn redis_namespaces_share_state_across_connections_and_stay_disjoint() {
        let Some(redis) = spawn_redis().await else {
            return;
        };
        let url = format!("redis://127.0.0.1:{}/", redis.1);
        let replica_a = StoreBackend::connect(&url).await.unwrap();
        let replica_b = StoreBackend::connect(&url).await.unwrap();

        let a = replica_a.namespace("charge");
        let b = replica_b.namespace("charge");
        assert!(a.put_if_absent("sig", json!(1)).await.unwrap());
        assert!(!b.put_if_absent("sig", json!(2)).await.unwrap());
        assert_eq!(b.get("sig").await.unwrap(), Some(json!(1)));

        let other = replica_b.namespace("confidential:mainnet");
        assert!(other.put_if_absent("sig", json!(3)).await.unwrap());

        b.delete("sig").await.unwrap();
        assert_eq!(a.get("sig").await.unwrap(), None);
        a.put("k", json!({"x": 1})).await.unwrap();
        assert_eq!(b.get("k").await.unwrap(), Some(json!({"x": 1})));
    }

    #[tokio::test]
    async fn redis_charge_replay_generations_expire_with_their_root() {
        use pay_kit::mpp::store::{ChargeReplayStore, ChargeReservation};
        let Some(redis) = spawn_redis().await else {
            return;
        };
        let url = format!("redis://127.0.0.1:{}/", redis.1);
        let store = StoreBackend::connect(&url)
            .await
            .unwrap()
            .namespace("charge");
        let replay = ChargeReplayStore::new(store);
        let lease = std::time::Duration::from_secs(120);
        let client = redis::Client::open(url).unwrap();
        let mut connection = client.get_multiplexed_async_connection().await.unwrap();
        let reader = connection.clone();
        let root = "pay-api:charge:solana-charge:record:challenge";
        let ttl = |key: String| {
            let mut connection = reader.clone();
            async move {
                redis::cmd("TTL")
                    .arg(key)
                    .query_async::<i64>(&mut connection)
                    .await
                    .unwrap()
            }
        };

        assert_eq!(
            replay
                .reserve("challenge", "digest-a", lease)
                .await
                .unwrap(),
            ChargeReservation::Reserved { generation: 0 }
        );
        // Age the root, then append the confirmed generation: the whole
        // record shares the new deadline instead of the root lapsing first.
        redis::cmd("EXPIRE")
            .arg(root)
            .arg(5)
            .query_async::<()>(&mut connection)
            .await
            .unwrap();
        replay.mark_confirmed("challenge", "sig-a").await.unwrap();
        assert!(ttl(format!("{root}:1")).await > 100);
        assert!(ttl(root.to_string()).await >= ttl(format!("{root}:1")).await);

        // A root lost under a surviving generation (written before family
        // expiry) is never recreated: the chain fails closed.
        redis::cmd("DEL")
            .arg(root)
            .query_async::<()>(&mut connection)
            .await
            .unwrap();
        assert!(
            replay
                .reserve("challenge", "digest-b", lease)
                .await
                .is_err()
        );

        // Positive control: an untouched challenge still reserves.
        assert_eq!(
            replay.reserve("fresh", "digest-a", lease).await.unwrap(),
            ChargeReservation::Reserved { generation: 0 }
        );
        assert_eq!(
            replay.reserve("fresh", "digest-a", lease).await.unwrap(),
            ChargeReservation::InProgress
        );
    }
}
