//! Admission to pay-connect's bounded in-memory tables.
//!
//! One rule for every table: a full table makes room from expired rows, or
//! from the newcomer's owner's own rows once that owner is at its quota;
//! never from another owner's rows. Otherwise the newcomer is refused.

use std::collections::HashMap;
use std::time::Instant;

/// A row of a bounded table.
pub(crate) trait Row {
    /// Who the row belongs to.
    fn owner(&self) -> &str;
    fn created_at(&self) -> Instant;
}

/// How much a table holds, in total and per owner.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub rows: usize,
    pub per_owner: usize,
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
    let owner = value.owner();
    let mut own: Vec<(&String, Instant)> = map
        .iter()
        .filter(|(_, row)| row.owner() == owner)
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
    if map.len() >= limits.rows {
        return Err(value);
    }
    map.insert(key, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct R(&'static str, Instant);

    impl Row for R {
        fn owner(&self) -> &str {
            self.0
        }
        fn created_at(&self) -> Instant {
            self.1
        }
    }

    const LIMITS: Limits = Limits {
        rows: 4,
        per_owner: 2,
    };

    #[test]
    fn an_owner_at_its_quota_recycles_its_own_oldest_row() {
        let t = Instant::now();
        let mut map = HashMap::new();
        for (i, at) in [0, 1, 2].into_iter().enumerate() {
            let at = t + Duration::from_secs(at);
            assert!(admit(&mut map, format!("a{i}"), R("a", at), LIMITS, |_| false).is_ok());
        }
        assert!(!map.contains_key("a0") && map.contains_key("a1") && map.contains_key("a2"));
    }

    #[test]
    fn a_full_table_refuses_rather_than_evict_another_owner() {
        let t = Instant::now();
        let mut map = HashMap::new();
        for (key, owner) in [("a0", "a"), ("a1", "a"), ("b0", "b"), ("b1", "b")] {
            assert!(admit(&mut map, key.into(), R(owner, t), LIMITS, |_| false).is_ok());
        }
        assert!(admit(&mut map, "c0".into(), R("c", t), LIMITS, |_| false).is_err());
        assert_eq!(map.len(), 4);
        // Expired rows make room.
        let expired = |row: &R| row.0 == "b";
        assert!(admit(&mut map, "c0".into(), R("c", t), LIMITS, expired).is_ok());
    }
}
