//! The age identity in the operating system's credential store.
//!
//! On macOS the store is the login keychain. Each identity is one
//! generic-password item: service [`SERVICE`], account = the age
//! recipient (`age1…`), value = the identity (`AGE-SECRET-KEY-1…`).
//! yoink asks for one item per recipient by name; it never lists or
//! searches the keychain.
//!
//! The point is narrow: no plaintext key on disk or in backups. An item
//! in the login keychain is encrypted with the login password, and
//! macOS prompts before another program can read it. It is not a
//! hardware boundary.
//!
//! Other platforms get [`NoStore`], which holds nothing and reports
//! that the feature is not available.

use thiserror::Error;
use zeroize::Zeroizing;

/// Keychain service name of every yoink identity item.
pub const SERVICE: &str = "is.yoink.age-identity";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    /// The user said no on the system prompt. Identity resolution stops
    /// here: a refusal must not be bypassed through a copy on disk.
    #[error(
        "access to the keychain item for {recipient} was denied — allow yoink in the \
         keychain prompt, or set YOINK_AGE_KEY / YOINK_AGE_KEY_FILE"
    )]
    Denied { recipient: String },
    /// The store cannot answer: locked keychain, no GUI session (SSH
    /// into the Mac), or a platform without a store. Identity
    /// resolution continues with the key files.
    #[error("the keychain is not available: {0}")]
    Unavailable(String),
}

/// A place that holds age identities, addressed by recipient.
pub trait IdentityStore: Send + Sync {
    /// False on a platform without a store. The key commands use it to
    /// say "macOS only" instead of a confusing lookup failure.
    fn supported(&self) -> bool;
    /// The identity for `recipient`, or `None` when no item exists.
    fn get(&self, recipient: &str) -> Result<Option<Zeroizing<String>>, StoreError>;
    /// Create or replace the item for `recipient`.
    fn set(&self, recipient: &str, identity: &str) -> Result<(), StoreError>;
    /// Remove the item. `false` when there was none.
    fn delete(&self, recipient: &str) -> Result<bool, StoreError>;
}

/// The store of this platform.
#[must_use]
pub fn system_store() -> Box<dyn IdentityStore> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::Keychain)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(NoStore)
    }
}

/// A store that holds nothing. Used on platforms without a keychain,
/// and by callers that must not touch the real one.
pub struct NoStore;

impl IdentityStore for NoStore {
    fn supported(&self) -> bool {
        false
    }
    fn get(&self, _recipient: &str) -> Result<Option<Zeroizing<String>>, StoreError> {
        Ok(None)
    }
    fn set(&self, _recipient: &str, _identity: &str) -> Result<(), StoreError> {
        Err(StoreError::Unavailable(
            "the keychain is supported on macOS only".into(),
        ))
    }
    fn delete(&self, _recipient: &str) -> Result<bool, StoreError> {
        Ok(false)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use security_framework::base::Error;
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };
    use zeroize::Zeroizing;

    use super::{IdentityStore, SERVICE, StoreError};

    // OSStatus values from <Security/SecBase.h>.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
    const ERR_SEC_USER_CANCELED: i32 = -128;

    pub struct Keychain;

    /// Only an explicit "Deny" on the prompt (`errSecUserCanceled`) is
    /// a refusal. Every other code means the store cannot answer:
    /// a locked keychain, no GUI session, no keychain. That includes
    /// `errSecAuthFailed`, which macOS can return with no prompt shown,
    /// so it must not block the fall-through to the key files.
    fn classify(err: Error, recipient: &str) -> StoreError {
        match err.code() {
            ERR_SEC_USER_CANCELED => StoreError::Denied {
                recipient: recipient.to_string(),
            },
            code => StoreError::Unavailable(format!("{err} (OSStatus {code})")),
        }
    }

    impl IdentityStore for Keychain {
        fn supported(&self) -> bool {
            true
        }

        fn get(&self, recipient: &str) -> Result<Option<Zeroizing<String>>, StoreError> {
            match get_generic_password(SERVICE, recipient) {
                Ok(bytes) => {
                    let bytes = Zeroizing::new(bytes);
                    let text = std::str::from_utf8(&bytes).map_err(|_| {
                        StoreError::Unavailable(format!(
                            "the keychain item for {recipient} is not valid UTF-8"
                        ))
                    })?;
                    Ok(Some(Zeroizing::new(text.trim().to_string())))
                }
                Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
                Err(e) => Err(classify(e, recipient)),
            }
        }

        fn set(&self, recipient: &str, identity: &str) -> Result<(), StoreError> {
            set_generic_password(SERVICE, recipient, identity.as_bytes())
                .map_err(|e| classify(e, recipient))
        }

        fn delete(&self, recipient: &str) -> Result<bool, StoreError> {
            match delete_generic_password(SERVICE, recipient) {
                Ok(()) => Ok(true),
                Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(false),
                Err(e) => Err(classify(e, recipient)),
            }
        }
    }
}

/// An in-memory store for tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use zeroize::Zeroizing;

    use super::{IdentityStore, StoreError};

    #[derive(Default)]
    pub(crate) struct FakeStore {
        pub(crate) items: Mutex<BTreeMap<String, String>>,
        /// When set, every `get` fails with this error.
        pub(crate) fail: Option<StoreError>,
    }

    impl FakeStore {
        pub(crate) fn with(recipient: &str, identity: &str) -> Self {
            let store = Self::default();
            store
                .items
                .lock()
                .unwrap()
                .insert(recipient.into(), identity.into());
            store
        }
    }

    impl IdentityStore for FakeStore {
        fn supported(&self) -> bool {
            true
        }
        fn get(&self, recipient: &str) -> Result<Option<Zeroizing<String>>, StoreError> {
            if let Some(err) = &self.fail {
                return Err(match err {
                    StoreError::Denied { .. } => StoreError::Denied {
                        recipient: recipient.into(),
                    },
                    StoreError::Unavailable(m) => StoreError::Unavailable(m.clone()),
                });
            }
            Ok(self
                .items
                .lock()
                .unwrap()
                .get(recipient)
                .cloned()
                .map(Zeroizing::new))
        }
        fn set(&self, recipient: &str, identity: &str) -> Result<(), StoreError> {
            self.items
                .lock()
                .unwrap()
                .insert(recipient.into(), identity.into());
            Ok(())
        }
        fn delete(&self, recipient: &str) -> Result<bool, StoreError> {
            Ok(self.items.lock().unwrap().remove(recipient).is_some())
        }
    }
}
