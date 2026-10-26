//! Admission to pay-connect's bounded in-memory tables.
//!
//! One rule for every table: a full table makes room from expired rows,
//! from the newcomer's owner's own rows once that owner is at its quota, or,
//! in tables of in-flight requests, from the oldest row nobody owns yet;
//! never from another owner's rows. Otherwise the newcomer is refused.

use std::collections::HashMap;
use std::time::Instant;

/// A row of a bounded table.
pub(crate) trait Row {
    /// Who the row belongs to; `None` for an anonymous request.
    fn owner(&self) -> Option<&str>;
    fn created_at(&self) -> Instant;
}

/// What a full table does with rows nobody owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Anonymous {
    /// Keep them: they are credentials, so a full table refuses instead.
    Keep,
    /// Drop the oldest: an in-flight request anyone can start again, so a
    /// burst of them displaces itself rather than lock everyone out.
    EvictOldest,
}

/// How much a table holds, in total and per owner.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub rows: usize,
    pub per_owner: usize,
    pub anonymous: Anonymous,
}

/// Insert `value` under `key` if room can be made without touching another
/// owner's rows; on refusal the value is handed back untouched.
pub(crate) fn admit<V: Row>(
    map: &mut HashMap<String, V>,
    key: String,
    value: V,
    limits: Limits,
    expired: impl Fn(&V) -> bool,
) -> Result<(), V> {
    map.retain(|_, row| !expired(row));
    if let Some(owner) = value.owner() {
        let mut own: Vec<(&String, Instant)> = map
            .iter()
            .filter(|(_, row)| row.owner() == Some(owner))
            .map(|(key, row)| (key, row.created_at()))
            .collect();
        own.sort_unstable_by_key(|(_, created_at)| *created_at);
        let surplus = (own.len() + 1).saturating_sub(limits.per_owner);
        let doomed: Vec<String> = own
            .into_iter()
            .take(surplus)
            .map(|(key, _)| key.clone())
            .collect();
        for key in doomed {
            map.remove(&key);
        }
    }
    if map.len() >= limits.rows {
        let oldest = (limits.anonymous == Anonymous::EvictOldest)
            .then(|| {
                map.iter()
                    .filter(|(_, row)| row.owner().is_none())
                    .min_by_key(|(_, row)| row.created_at())
                    .map(|(key, _)| key.clone())
            })
            .flatten();
        match oldest {
            Some(oldest) => {
                map.remove(&oldest);
            }
            None => return Err(value),
        }
    }
    map.insert(key, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct R(Option<&'static str>, Instant);

    impl Row for R {
        fn owner(&self) -> Option<&str> {
            self.0
        }
        fn created_at(&self) -> Instant {
            self.1
        }
    }

    const fn limits(anonymous: Anonymous) -> Limits {
        Limits {
            rows: 4,
            per_owner: 2,
            anonymous,
        }
    }

    #[test]
    fn an_owner_at_its_quota_recycles_its_own_oldest_row() {
        let t = Instant::now();
        let mut map = HashMap::new();
        for (i, at) in [0, 1, 2].into_iter().enumerate() {
            let row = R(Some("a"), t + Duration::from_secs(at));
            assert!(
                admit(
                    &mut map,
                    format!("a{i}"),
                    row,
                    limits(Anonymous::Keep),
                    |_| false
                )
                .is_ok()
            );
        }
        assert!(!map.contains_key("a0") && map.contains_key("a1") && map.contains_key("a2"));
    }

    #[test]
    fn a_full_table_refuses_rather_than_evict_another_owner() {
        let t = Instant::now();
        let mut map = HashMap::new();
        let keep = limits(Anonymous::Keep);
        for (key, owner) in [("a0", "a"), ("a1", "a"), ("b0", "b"), ("b1", "b")] {
            assert!(admit(&mut map, key.into(), R(Some(owner), t), keep, |_| false).is_ok());
        }
        for anonymous in [Anonymous::Keep, Anonymous::EvictOldest] {
            let row = R(Some("c"), t);
            assert!(admit(&mut map, "c0".into(), row, limits(anonymous), |_| false).is_err());
            assert!(
                admit(&mut map, "n0".into(), R(None, t), limits(anonymous), |_| {
                    false
                })
                .is_err()
            );
        }
        assert_eq!(map.len(), 4);
        // Expired rows make room.
        let expired = |row: &R| row.0 == Some("b");
        assert!(admit(&mut map, "c0".into(), R(Some("c"), t), keep, expired).is_ok());
    }

    #[test]
    fn requests_nobody_owns_displace_each_other_oldest_first() {
        let t = Instant::now();
        let mut map = HashMap::new();
        let evict = limits(Anonymous::EvictOldest);
        let at = |s| t + Duration::from_secs(s);
        assert!(admit(&mut map, "a0".into(), R(Some("a"), at(0)), evict, |_| false).is_ok());
        for i in 1..=4 {
            assert!(admit(&mut map, format!("n{i}"), R(None, at(i)), evict, |_| false).is_ok());
        }
        assert!(map.contains_key("a0"), "an owned row stays");
        assert!(!map.contains_key("n1") && map.contains_key("n4"));
        // An owner's request displaces an anonymous one too.
        assert!(admit(&mut map, "b0".into(), R(Some("b"), at(5)), evict, |_| false).is_ok());
        assert!(map.contains_key("a0") && !map.contains_key("n2"));
    }
}
