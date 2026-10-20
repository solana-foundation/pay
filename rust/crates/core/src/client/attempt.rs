//! One logical paid request and the idempotency material its retries share.
//!
//! When a paid retry gets no definitive answer (a transport error, a timeout,
//! a server error), the payment may or may not have settled. Retrying the same
//! request must then reuse the same idempotency material — the x402
//! `payment-identifier`, or the exact signed `upto` header — so the server can
//! recognise it instead of charging again. A definitive answer clears it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::Result;

type Digest32 = [u8; 32];
/// Attempt identity and signing scope (payer and network).
type Slot = (Digest32, Digest32);

/// Material kept for unresolved attempts, and the keys being built now.
static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
/// Signalled whenever a build finishes, so callers waiting on it re-check.
static BUILT: Condvar = Condvar::new();

#[derive(Default)]
struct State {
    /// Pending material by slot, then by offer.
    pending: HashMap<Slot, HashMap<Digest32, Pending>>,
    /// Keys whose `make` is running: a concurrent caller for the same key
    /// waits for its result instead of signing a second payment.
    building: HashSet<(Slot, Digest32)>,
}

fn lock_state() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Pending {
    value: String,
    expires: Instant,
}

/// The identity of one logical paid request: method, URL, and body.
///
/// It remembers the signing scopes its keys were made for, so resolving it
/// clears only material this payer and network could have filed.
#[derive(Debug, Clone)]
pub struct PaymentAttempt {
    identity: Digest32,
    scopes: Arc<Mutex<HashSet<Digest32>>>,
}

impl PartialEq for PaymentAttempt {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

impl Eq for PaymentAttempt {}

impl PaymentAttempt {
    pub fn new(method: &str, url: &str, body: Option<&[u8]>) -> Self {
        let mut hasher = Sha256::new();
        for part in [method.as_bytes(), url.as_bytes()] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        match body {
            Some(body) => {
                hasher.update([1]);
                hasher.update(Sha256::digest(body));
            }
            None => hasher.update([0]),
        }
        Self {
            identity: hasher.finalize().into(),
            scopes: Arc::default(),
        }
    }

    /// The server answered definitively: forget the material this attempt's
    /// payers filed. Another wallet's pending material for the same request
    /// is left for its own retry.
    pub fn resolved(&self) {
        let scopes = std::mem::take(&mut *self.scopes.lock().unwrap());
        let mut state = lock_state();
        for scope in scopes {
            state.pending.remove(&(self.identity, scope));
        }
    }
}

/// Where pending material is filed: one request, signed by one payer on one
/// network, for one offer. Signed material is never shared across wallets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaymentCacheKey {
    identity: Digest32,
    scope: Digest32,
    offer: Digest32,
}

impl PaymentCacheKey {
    pub(crate) fn new(
        attempt: &PaymentAttempt,
        payer: &solana_pubkey::Pubkey,
        network: &str,
        kind: &str,
        terms: &[&str],
    ) -> Self {
        let mut scope = Sha256::new();
        scope.update(payer.to_bytes());
        scope.update((network.len() as u64).to_le_bytes());
        scope.update(network.as_bytes());
        let scope: Digest32 = scope.finalize().into();
        attempt.scopes.lock().unwrap().insert(scope);

        let mut offer = Sha256::new();
        for part in std::iter::once(kind).chain(terms.iter().copied()) {
            offer.update((part.len() as u64).to_le_bytes());
            offer.update(part.as_bytes());
        }
        Self {
            identity: attempt.identity,
            scope,
            offer: offer.finalize().into(),
        }
    }

    /// The material pending under this key, or a new value from `make` that
    /// stays pending for `ttl` until [`PaymentAttempt::resolved`].
    ///
    /// Single-flight per key: while one caller's `make` runs, others for the
    /// same key wait and then receive its value, so concurrent duplicates of
    /// one request never sign two payments. A failed `make` files nothing and
    /// lets the next waiter build.
    pub(crate) fn reuse_or_insert(
        &self,
        ttl: Duration,
        make: impl FnOnce() -> Result<String>,
    ) -> Result<String> {
        let slot = (self.identity, self.scope);
        let building = (slot, self.offer);
        let mut state = lock_state();
        loop {
            let now = Instant::now();
            if let Some(pending) = state
                .pending
                .get(&slot)
                .and_then(|offers| offers.get(&self.offer))
                .filter(|pending| pending.expires > now)
            {
                return Ok(pending.value.clone());
            }
            if !state.building.contains(&building) {
                break;
            }
            state = BUILT
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.building.insert(building);
        drop(state);

        // Clears the reservation even if `make` panics, so waiters never hang.
        struct Reservation((Slot, Digest32));
        impl Drop for Reservation {
            fn drop(&mut self) {
                lock_state().building.remove(&self.0);
                BUILT.notify_all();
            }
        }
        let reservation = Reservation(building);

        let value = make()?;
        let now = Instant::now();
        let mut state = lock_state();
        state.pending.retain(|_, offers| {
            offers.retain(|_, entry| entry.expires > now);
            !offers.is_empty()
        });
        state.pending.entry(slot).or_default().insert(
            self.offer,
            Pending {
                value: value.clone(),
                expires: now + ttl,
            },
        );
        drop(state);
        drop(reservation);
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYER: solana_pubkey::Pubkey = solana_pubkey::Pubkey::new_from_array([1; 32]);

    fn counter() -> impl FnMut() -> Result<String> {
        let mut next = 0;
        move || {
            next += 1;
            Ok(format!("value-{next}"))
        }
    }

    fn key(attempt: &PaymentAttempt, kind: &str, terms: &[&str]) -> PaymentCacheKey {
        PaymentCacheKey::new(attempt, &PAYER, "mainnet", kind, terms)
    }

    #[test]
    fn a_pending_attempt_reuses_its_material_until_resolved() {
        let attempt = PaymentAttempt::new("POST", "https://a.example/pending", Some(b"{}"));
        let ttl = Duration::from_secs(60);
        let mut make = counter();
        let key = key(&attempt, "k", &["t"]);
        let first = key.reuse_or_insert(ttl, &mut make).unwrap();
        let retry = key.reuse_or_insert(ttl, &mut make).unwrap();
        assert_eq!(first, retry);

        attempt.resolved();
        let fresh = key.reuse_or_insert(ttl, &mut make).unwrap();
        assert_ne!(first, fresh);
        attempt.resolved();
    }

    #[test]
    fn material_is_scoped_to_request_identity_and_offer() {
        let ttl = Duration::from_secs(60);
        let mut make = counter();
        let attempt = PaymentAttempt::new("POST", "https://a.example/scoped", Some(b"a"));
        let base = key(&attempt, "k", &["t"])
            .reuse_or_insert(ttl, &mut make)
            .unwrap();

        let other_body = PaymentAttempt::new("POST", "https://a.example/scoped", Some(b"b"));
        let no_body = PaymentAttempt::new("POST", "https://a.example/scoped", None);
        let other_method = PaymentAttempt::new("PUT", "https://a.example/scoped", Some(b"a"));
        for other in [&other_body, &no_body, &other_method] {
            assert_ne!(other, &attempt);
            assert_ne!(
                key(other, "k", &["t"])
                    .reuse_or_insert(ttl, &mut make)
                    .unwrap(),
                base
            );
            other.resolved();
        }
        // Changed payment terms are a different offer.
        for other in [key(&attempt, "k", &["t2"]), key(&attempt, "k2", &["t"])] {
            assert_ne!(other.reuse_or_insert(ttl, &mut make).unwrap(), base);
        }
        // Field boundaries are length-prefixed.
        assert_ne!(
            key(&attempt, "k", &["ab", "c"]),
            key(&attempt, "k", &["a", "bc"])
        );
        attempt.resolved();
    }

    #[test]
    fn material_is_scoped_to_payer_and_network() {
        let ttl = Duration::from_secs(60);
        let mut make = counter();
        let attempt = PaymentAttempt::new("POST", "https://a.example/payer", Some(b"a"));
        let base = key(&attempt, "k", &["t"])
            .reuse_or_insert(ttl, &mut make)
            .unwrap();
        let other_payer = solana_pubkey::Pubkey::new_from_array([2; 32]);
        for other in [
            PaymentCacheKey::new(&attempt, &other_payer, "mainnet", "k", &["t"]),
            PaymentCacheKey::new(&attempt, &PAYER, "devnet", "k", &["t"]),
        ] {
            assert_ne!(other.reuse_or_insert(ttl, &mut make).unwrap(), base);
        }
        // Positive control: the same payer and network still reuse it.
        assert_eq!(
            key(&attempt, "k", &["t"])
                .reuse_or_insert(ttl, &mut make)
                .unwrap(),
            base
        );
        attempt.resolved();
    }

    #[test]
    fn resolving_one_wallet_keeps_another_wallets_pending_material() {
        let ttl = Duration::from_secs(60);
        let mut make = counter();
        let request = || PaymentAttempt::new("POST", "https://a.example/two-wallets", Some(b"a"));
        let other_payer = solana_pubkey::Pubkey::new_from_array([2; 32]);

        let wallet_a = request();
        let a_material = key(&wallet_a, "k", &["t"])
            .reuse_or_insert(ttl, &mut make)
            .unwrap();

        // Wallet B issues the same request and gets a definitive answer.
        let wallet_b = request();
        let b_key = PaymentCacheKey::new(&wallet_b, &other_payer, "mainnet", "k", &["t"]);
        let b_material = b_key.reuse_or_insert(ttl, &mut make).unwrap();
        assert_ne!(a_material, b_material);
        wallet_b.resolved();

        // A's retry still resends its own material; B's was cleared.
        let a_retry = request();
        assert_eq!(
            key(&a_retry, "k", &["t"])
                .reuse_or_insert(ttl, &mut make)
                .unwrap(),
            a_material
        );
        let b_retry = request();
        assert_ne!(
            PaymentCacheKey::new(&b_retry, &other_payer, "mainnet", "k", &["t"])
                .reuse_or_insert(ttl, &mut make)
                .unwrap(),
            b_material
        );
        a_retry.resolved();
        b_retry.resolved();
    }

    #[test]
    fn concurrent_misses_for_one_key_build_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempt = PaymentAttempt::new("POST", "https://a.example/race", Some(b"{}"));
        let key = key(&attempt, "k", &["t"]);
        let builds = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(std::sync::Barrier::new(4));
        let values: Vec<String> = (0..4)
            .map(|_| {
                let (key, builds, start) = (key.clone(), builds.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    key.reuse_or_insert(Duration::from_secs(60), || {
                        let n = builds.fetch_add(1, Ordering::SeqCst);
                        // Hold the build open long enough for the others to miss.
                        std::thread::sleep(Duration::from_millis(100));
                        Ok(format!("value-{n}"))
                    })
                    .unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(values.iter().all(|value| value == &values[0]));
        attempt.resolved();
    }

    #[test]
    fn a_failed_build_files_nothing_and_lets_the_next_caller_build() {
        let attempt = PaymentAttempt::new("POST", "https://a.example/failed", Some(b"{}"));
        let key = key(&attempt, "k", &["t"]);
        let ttl = Duration::from_secs(60);
        assert!(
            key.reuse_or_insert(ttl, || Err(crate::Error::Mpp("declined".into())))
                .is_err()
        );
        assert_eq!(
            key.reuse_or_insert(ttl, || Ok("signed".to_string()))
                .unwrap(),
            "signed"
        );
        attempt.resolved();
    }

    #[test]
    fn expired_material_is_replaced() {
        let attempt = PaymentAttempt::new("GET", "https://a.example/expired", None);
        let mut make = counter();
        let key = key(&attempt, "k", &["t"]);
        let first = key.reuse_or_insert(Duration::ZERO, &mut make).unwrap();
        let second = key
            .reuse_or_insert(Duration::from_secs(60), &mut make)
            .unwrap();
        assert_ne!(first, second);
        attempt.resolved();
    }
}
