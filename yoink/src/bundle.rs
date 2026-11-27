//! The resolved secrets: key → value, held in memory for one run.
//! Every provider (`secrets`) produces one; every consumer (config
//! host addresses, ssh keys, service env, Caddy certs) reads it.

use std::collections::BTreeMap;
use std::fmt;

use zeroize::Zeroizing;

/// In-memory map of resolved secret keys → values. Built once at the
/// start of a reconcile. Each service then picks the keys it needs.
/// Values are wrapped in `Zeroizing<String>` so they are wiped from
/// memory when the bundle is dropped.
#[derive(Clone, Default)]
pub struct SecretsBundle {
    values: BTreeMap<String, Zeroizing<String>>,
}

impl SecretsBundle {
    #[must_use]
    pub fn new(values: BTreeMap<String, Zeroizing<String>>) -> Self {
        Self { values }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|v| v.as_str())
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl fmt::Debug for SecretsBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretsBundle")
            .field("len", &self.values.len())
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}
