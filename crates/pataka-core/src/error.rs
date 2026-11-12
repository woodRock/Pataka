//! One error type for every command. Messages are written for people and
//! never include message text, API keys or other secrets.

use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Pātaka is locked. Unlock it first.")]
    Locked,
    #[error("Pātaka has not been set up yet.")]
    NotInitialised,
    #[error("Pātaka is already set up.")]
    AlreadyInitialised,
    #[error("That passphrase did not unlock Pātaka.")]
    BadPassphrase,
    #[error("Stored data failed its integrity check. It may have been changed or damaged outside Pātaka.")]
    Integrity,
    #[error("The OS keychain is not available: {0}")]
    Keychain(String),
    #[error("{0}")]
    Invalid(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Blocked: {0}")]
    Blocked(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("The model provider returned an error: {0}")]
    Provider(String),
    #[error("File error: {0}")]
    Io(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        // The kind only: paths can contain user names.
        Error::Io(e.kind().to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Error::Integrity
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        let kind = if e.is_timeout() {
            "the request timed out"
        } else if e.is_connect() {
            "could not connect"
        } else if e.is_redirect() {
            "the server tried to redirect (redirects are refused)"
        } else if e.is_decode() || e.is_body() {
            "the response could not be read"
        } else {
            "the request failed"
        };
        Error::Network(kind.into())
    }
}

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
