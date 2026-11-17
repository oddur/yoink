//! The age identity in the operating system's credential store.
//!
//! On macOS the store is the login keychain. Each identity is one
//! generic-password item: service [`SERVICE`], account = the age
//! recipient (`age1…`), value = the identity (`AGE-SECRET-KEY-1…`).
//! To decrypt, yoink asks for one item per recipient by name. It
//! searches the keychain only for item names (`key list`), never for
//! a secret.
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

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum StoreError {
    /// The user said no on the system prompt. Identity resolution stops
    /// here: a refusal must not be bypassed through a copy on disk.
    #[error(
        "access to the keychain item for {recipient} was denied — allow yoink in the \
         keychain prompt, or set YOINK_AGE_KEY / YOINK_AGE_KEY_FILE"
    )]
    Denied { recipient: String },
    /// The store cannot answer: a locked keychain, an item that needs a
    /// permission macOS cannot ask for (prompts are off in an SSH
    /// session), or a platform without a store. Identity resolution
    /// continues with the key files.
    #[error("the keychain is not available: {0}{note}", note = ssh_note())]
    Unavailable(String),
}

/// One item of the store, without its secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredItem {
    pub recipient: String,
    /// The name shown in Keychain Access. yoink writes the project
    /// there when it creates the item. A note for people; lookups go
    /// by recipient.
    pub label: String,
}

/// The item name for a project: what Keychain Access shows.
#[must_use]
pub fn label_for(project: &str) -> String {
    format!("yoink age key: {project}")
}

/// A place that holds age identities, addressed by recipient.
pub trait IdentityStore: Send + Sync {
    /// False on a platform without a store. The key commands use it to
    /// say "macOS only" instead of a confusing lookup failure.
    fn supported(&self) -> bool;
    /// The identity for `recipient`, or `None` when no item exists.
    fn get(&self, recipient: &str) -> Result<Option<Zeroizing<String>>, StoreError>;
    /// Create or replace the item for `recipient`. `label` names a new
    /// item; an item that exists keeps the name it has.
    fn set(&self, recipient: &str, identity: &str, label: &str) -> Result<(), StoreError>;
    /// Every yoink item in the store, without the secrets. Reads names
    /// only, so the store does not ask for access to a secret.
    fn list(&self) -> Result<Vec<StoredItem>, StoreError> {
        Ok(Vec::new())
    }
    /// Remove the item. `false` when there was none.
    fn delete(&self, recipient: &str) -> Result<bool, StoreError>;
}

/// Held while yoink must not put up a keychain prompt. See [`no_prompts`].
pub struct NoPrompts {
    active: bool,
}

impl NoPrompts {
    /// False when prompts could not be turned off. The caller must then
    /// stay away from the keychain: a read can still open a dialog.
    #[must_use]
    pub fn active(&self) -> bool {
        self.active
    }
}

/// How many [`NoPrompts`] values are alive, and the lock they share.
/// Guards nest (a whole run over SSH, plus `key list` inside it); only
/// the last one to go turns prompts back on.
#[cfg(target_os = "macos")]
static QUIET: std::sync::Mutex<(
    usize,
    Option<security_framework::os::macos::keychain::KeychainUserInteractionLock>,
)> = std::sync::Mutex::new((0, None));

/// Turn keychain prompts off until the returned value is dropped. A
/// read that would prompt fails instead, and identity resolution
/// treats that as "the keychain cannot answer".
///
/// Used where a system dialog must never open: shell completion, the
/// key list, and every command in an SSH session.
#[must_use]
pub fn no_prompts() -> NoPrompts {
    #[cfg(target_os = "macos")]
    {
        use security_framework::os::macos::keychain::SecKeychain;
        let mut quiet = QUIET
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if quiet.0 == 0 {
            quiet.1 = SecKeychain::disable_user_interaction().ok();
        }
        let active = quiet.1.is_some();
        if active {
            quiet.0 += 1;
        }
        NoPrompts { active }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // No store, so no prompt to turn off.
        NoPrompts { active: true }
    }
}

impl Drop for NoPrompts {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if self.active {
            let mut quiet = QUIET
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            quiet.0 = quiet.0.saturating_sub(1);
            if quiet.0 == 0 {
                quiet.1 = None;
            }
        }
    }
}

/// The store of this platform. It holds no state: every call goes to
/// the keychain. Identity resolution remembers its own result for the
/// run; see `sealed::resolve_identity`.
#[must_use]
pub fn system_store() -> &'static dyn IdentityStore {
    #[cfg(target_os = "macos")]
    {
        &macos::Keychain
    }
    #[cfg(not(target_os = "macos"))]
    {
        &NoStore
    }
}

/// True when the environment says this is an SSH session.
///
/// macOS cannot show the keychain prompt in one. yoink therefore turns
/// prompts off for the whole run (see `run` in main.rs) and still
/// tries the keychain: a read that needs no question works, and one
/// that does fails at once instead of waiting on a dialog nobody sees.
///
/// The variables can be stale, for example in a tmux session that was
/// started over SSH and is used at the Mac later. That is why this is
/// a reason to stay quiet, and not a reason to refuse.
#[must_use]
pub fn in_ssh_session() -> bool {
    ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

fn ssh_note() -> &'static str {
    if in_ssh_session() {
        " (this looks like an SSH session, where the macOS keychain does not work)"
    } else {
        ""
    }
}

/// What to do when the keychain did not hand out the key.
#[must_use]
pub fn advice() -> &'static str {
    advice_for(in_ssh_session())
}

fn advice_for(in_ssh_session: bool) -> &'static str {
    if in_ssh_session {
        "This looks like an SSH session, where macOS cannot ask for permission, so the \
         keychain does not work here. Set YOINK_AGE_KEY or YOINK_AGE_KEY_FILE for this \
         session, or run yoink on the Mac itself."
    } else {
        "If the key is in the keychain, unlock the keychain and allow yoink. Otherwise set \
         YOINK_AGE_KEY or YOINK_AGE_KEY_FILE, or create a key with `yoink secrets key generate`."
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
    fn set(&self, _recipient: &str, _identity: &str, _label: &str) -> Result<(), StoreError> {
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
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
        set_generic_password_options,
    };
    use security_framework::passwords_options::PasswordOptions;
    use zeroize::Zeroizing;

    use super::{IdentityStore, SERVICE, StoreError, StoredItem};

    // OSStatus values from <Security/SecBase.h>.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
    const ERR_SEC_USER_CANCELED: i32 = -128;
    const ERR_SEC_AUTH_FAILED: i32 = -25293;

    pub struct Keychain;

    /// A refusal on the access prompt stops identity resolution; every
    /// other failure lets it continue with the key files.
    ///
    /// Both codes below were observed on macOS with an ad-hoc signed
    /// binary that was not on the item's access list:
    ///
    /// - `errSecUserCanceled` (-128): the user selected "Deny" on the
    ///   prompt. A refusal. yoink stopped and did not read a key file
    ///   that held the same identity.
    /// - `errSecAuthFailed` (-25293): macOS needed the user's permission
    ///   and could not ask, because prompts were turned off
    ///   ([`super::no_prompts`]). Nobody refused, so this is "cannot
    ///   answer", and resolution falls through to the key files. yoink
    ///   turns prompts off in an SSH session, so this is the code an
    ///   SSH session gets for an item that needs a question.
    ///
    /// Every other code also means the store cannot answer.
    fn classify(err: Error, recipient: &str) -> StoreError {
        tracing::debug!(
            "keychain error for {recipient}: {err} (OSStatus {})",
            err.code()
        );
        match err.code() {
            ERR_SEC_USER_CANCELED => StoreError::Denied {
                recipient: recipient.to_string(),
            },
            ERR_SEC_AUTH_FAILED => StoreError::Unavailable(
                "macOS needs your permission for this item and could not ask".into(),
            ),
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
                    // An item that is not UTF-8 is not an identity. Hand
                    // it on as text anyway: the caller then skips this
                    // one item, the same way it skips an item that
                    // holds a different identity, and still asks for
                    // the other recipients.
                    let bytes = Zeroizing::new(bytes);
                    let text = Zeroizing::new(String::from_utf8_lossy(&bytes).into_owned());
                    Ok(Some(Zeroizing::new(text.trim().to_string())))
                }
                Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
                Err(e) => Err(classify(e, recipient)),
            }
        }

        fn set(&self, recipient: &str, identity: &str, label: &str) -> Result<(), StoreError> {
            // First a labelled write: it adds a new item with its name,
            // or updates an item that carries this same name.
            let mut options = PasswordOptions::new_generic_password(SERVICE, recipient);
            options.set_label(label);
            let labelled = set_generic_password_options(identity.as_bytes(), options);
            let result = match labelled {
                // The item exists under another name. The library's
                // update reuses the whole query, name included, so it
                // found nothing. Update by service and account only;
                // the item keeps the name it has.
                Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => {
                    set_generic_password(SERVICE, recipient, identity.as_bytes())
                }
                other => other,
            };
            result.map_err(|e| classify(e, recipient))
        }

        fn list(&self) -> Result<Vec<StoredItem>, StoreError> {
            // Attributes only (`load_data` stays off): names and
            // accounts are not protected by the item's access list.
            let found = ItemSearchOptions::new()
                .class(ItemClass::generic_password())
                .service(SERVICE)
                .load_attributes(true)
                .limit(Limit::All)
                .search();
            let results = match found {
                Ok(results) => results,
                Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => return Ok(Vec::new()),
                Err(e) => return Err(classify(e, "the yoink items")),
            };
            let mut items: Vec<StoredItem> = results
                .iter()
                .filter_map(security_framework::item::SearchResult::simplify_dict)
                .filter_map(|attrs| {
                    Some(StoredItem {
                        recipient: attrs.get("acct")?.clone(),
                        label: attrs.get("labl").cloned().unwrap_or_default(),
                    })
                })
                .collect();
            items.sort_by(|a, b| (&a.label, &a.recipient).cmp(&(&b.label, &b.recipient)));
            Ok(items)
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
        fn set(&self, recipient: &str, identity: &str, _label: &str) -> Result<(), StoreError> {
            self.items
                .lock()
                .unwrap()
                .insert(recipient.into(), identity.into());
            Ok(())
        }
        fn delete(&self, recipient: &str) -> Result<bool, StoreError> {
            Ok(self.items.lock().unwrap().remove(recipient).is_some())
        }
        fn list(&self) -> Result<Vec<super::StoredItem>, StoreError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .keys()
                .map(|recipient| super::StoredItem {
                    recipient: recipient.clone(),
                    label: String::new(),
                })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advice_names_ssh_only_in_an_ssh_session() {
        assert!(advice_for(true).contains("SSH session"));
        assert!(advice_for(true).contains("YOINK_AGE_KEY"));
        assert!(!advice_for(false).contains("SSH"));
        assert!(advice_for(false).contains("unlock the keychain"));
    }

    /// Guards nest: a whole run over SSH holds one, and `key list` takes
    /// a second. The inner one must not turn prompts back on.
    #[test]
    fn no_prompts_guards_nest() {
        let outer = no_prompts();
        let inner = no_prompts();
        assert_eq!(outer.active(), inner.active());
        drop(inner);
        // Still usable after the inner guard is gone.
        let again = no_prompts();
        assert_eq!(outer.active(), again.active());
    }
}
