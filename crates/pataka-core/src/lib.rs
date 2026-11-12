//! Pātaka's engine, shared by the desktop app and the `pataka` command so
//! there is one implementation to audit:
//!
//! - [`redaction`]: Tūmataiti rules, kaitiaki lists and the name model, with a
//!   persistent placeholder vault per kōrero
//! - [`agent_proxy`]: the localhost proxy that redacts an agent's requests and
//!   restores its replies
//! - [`store`] and [`crypto`]: everything encrypted at rest, unlocked by a
//!   passphrase or the OS keychain
//! - [`audit`]: the hash-chained audit log
//! - [`guards`]: what an agent must always ask about
//! - [`student`] and [`model_install`]: the Tūmataiti name model
//! - [`providers`] and [`sovereignty`]: where text may go, and the clients that send it

#![forbid(unsafe_code)]

pub mod agent_proxy;
pub mod audit;
pub mod claude_hooks;
pub mod crypto;
pub mod error;
pub mod guards;
pub mod mail;
pub mod model;
pub mod model_install;
pub mod paths;
pub mod providers;
pub mod redaction;
pub mod sovereignty;
pub mod store;
pub mod student;
