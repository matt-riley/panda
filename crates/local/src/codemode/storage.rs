//! One script's view of the active path. Its journal is returned only on success.
use anyhow::{Result, ensure};
use panda_core::{Message, StoreWrites};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const VALUE_CHARS: usize = 256 * 1024;
pub(super) const TOTAL_CHARS: usize = 1024 * 1024;

#[derive(Default)]
pub(super) struct ScriptStore {
    values: BTreeMap<String, String>,
    initial_keys: BTreeSet<String>,
    chars: usize,
    pub writes: StoreWrites,
}

impl ScriptStore {
    pub fn from_history(history: &[Message]) -> Self {
        let mut current = BTreeMap::new();
        for msg in history {
            if let Message::CodemodeStore(delta) = msg {
                for key in &delta.delete {
                    current.remove(key);
                }
                current.extend(delta.set.iter());
            }
        }
        let mut store = Self::default();
        for (key, value) in current {
            let json = value.to_string();
            store.chars += key.encode_utf16().count() + json.encode_utf16().count();
            store.initial_keys.insert(key.clone());
            store.values.insert(key.clone(), json);
        }
        store
    }

    // Empty text is the wire sentinel for a missing value; even the empty JSON
    // string has two quote characters, and null remains a normal stored value.
    pub fn load(&self, key: &str) -> &str {
        self.values.get(key).map(String::as_str).unwrap_or("")
    }

    pub fn store(&mut self, key: String, json: Option<String>) -> Result<()> {
        let key_chars = key.encode_utf16().count();
        let old = self
            .values
            .get(&key)
            .map_or(0, |s| key_chars + s.encode_utf16().count());
        let retained = self.chars - old;
        if let Some(json) = json {
            let size = json.encode_utf16().count();
            ensure!(
                size <= VALUE_CHARS,
                "Codemode store value exceeds 256 Ki characters"
            );
            let next = retained + key_chars + size;
            ensure!(
                next <= TOTAL_CHARS,
                "Codemode store exceeds 1 Mi characters"
            );
            let value = serde_json::from_str(&json)?;
            self.writes.delete.remove(&key);
            self.writes.set.insert(key.clone(), value);
            self.values.insert(key, json);
            self.chars = next;
        } else {
            self.values.remove(&key);
            self.writes.set.remove(&key);
            if self.initial_keys.contains(&key) {
                self.writes.delete.insert(key);
            }
            self.chars = retained;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    proptest! {
        #[test]
        fn replay_matches_successful_scripts(
            scripts in prop::collection::vec((any::<bool>(), prop::collection::vec(
                (0u8..5, prop::option::of(any::<i16>())), 1..8)), 1..16)
        ) {
            let mut history = Vec::new();
            let mut expected = BTreeMap::new();
            for (committed, operations) in scripts {
                let mut store = ScriptStore::from_history(&history);
                let mut local = expected.clone();
                for (key, value) in operations {
                    let key = key.to_string();
                    let json = value.map(|n| n.to_string());
                    store.store(key.clone(), json.clone()).unwrap();
                    if let Some(json) = json { local.insert(key, json); } else { local.remove(&key); }
                    prop_assert_eq!(&store.values, &local);
                }
                if committed {
                    history.push(Message::CodemodeStore(store.writes));
                    expected = local;
                }
                prop_assert_eq!(ScriptStore::from_history(&history).values, expected.clone());
            }
        }
    }

    #[test]
    fn rejected_write_keeps_previous_values_and_budget() {
        let mut store = ScriptStore::default();
        let full = json!("🍎".repeat((VALUE_CHARS - 2) / 2)).to_string();
        assert_eq!(full.encode_utf16().count(), VALUE_CHARS);
        for key in ["a", "b", "c"] {
            store.store(key.into(), Some(full.clone())).unwrap();
        }
        let remaining = TOTAL_CHARS - store.chars - 1;
        store
            .store(
                "d".into(),
                Some(Value::String("x".repeat(remaining - 2)).to_string()),
            )
            .unwrap();
        assert_eq!(store.chars, TOTAL_CHARS);
        let original = store.writes.clone();
        assert!(store.store("e".into(), Some("null".into())).is_err());
        assert!(
            store
                .store("a".into(), Some(json!("x".repeat(VALUE_CHARS)).to_string()))
                .is_err()
        );
        assert!(store.store("a".into(), Some("{".into())).is_err());
        assert_eq!(store.writes, original);
        assert_eq!(store.chars, TOTAL_CHARS);
        store.store("a".into(), None).unwrap();
        store.store("e".into(), Some("null".into())).unwrap();
        assert_eq!(store.load("e"), "null");
    }
}
