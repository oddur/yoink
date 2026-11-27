//! Leaves that every layer shares: values and their text forms (host,
//! image, run options, hashes, the secrets bundle), and thin wrappers
//! of local tools (git, the keychain, prompts, completion). Nothing
//! here uses another yoink module.

pub mod bundle;
pub mod completion;
pub mod git;
pub mod hash;
pub mod host;
pub mod image;
pub mod keychain;
pub mod prompt;
pub mod run_args;
