//! Māori data sovereignty rules: where a kōrero may go.
//!
//! - A provider on a loopback address is `Device`: nothing leaves the computer.
//!   Pātaka works this out from the URL; it cannot be claimed for a remote host.
//! - A remote provider is `Offshore` unless the person or their kaitiaki marks
//!   it `Sovereign` (infrastructure they control, e.g. iwi- or NZ-hosted).
//! - A `Tapu` kōrero may only go to `Device` or `Sovereign` providers.
//! - Anything leaving the device goes through Tūmataiti redaction first.

use std::net::IpAddr;

use url::Url;

use crate::error::{Error, Result};
use crate::model::{Classification, Provider, Trust};

pub fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        None => false,
    }
}

/// Parse and check a provider base URL. Remote hosts must use HTTPS; plain
/// HTTP is only accepted for loopback addresses. No credentials in the URL.
pub fn validate_base_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw.trim()).map_err(|_| Error::Invalid("That is not a valid URL.".into()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Invalid("Put API keys in the key field, not in the URL.".into()));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::Invalid("The base URL must not have a query or fragment.".into()));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback_host(&url) => Ok(url),
        "http" => Err(Error::Invalid(
            "Remote providers must use https:// so keys and text are encrypted in transit.".into(),
        )),
        _ => Err(Error::Invalid("Only http(s) URLs are supported.".into())),
    }
}

/// The trust level Pātaka will actually use for a provider.
pub fn effective_trust(p: &Provider) -> Trust {
    match Url::parse(&p.base_url) {
        Ok(u) if is_loopback_host(&u) => Trust::Device,
        // A remote host can never be Device, whatever is stored.
        _ if p.trust == Trust::Device => Trust::Offshore,
        _ => p.trust,
    }
}

/// May this kōrero be sent to this provider?
pub fn check_destination(classification: Classification, p: &Provider) -> Result<()> {
    match (classification, effective_trust(p)) {
        (Classification::Tapu, Trust::Offshore) => Err(Error::Blocked(format!(
            "this kōrero is marked tapu, so it can only go to a model on this device or on infrastructure you have marked sovereign. {} processes data in {}",
            p.name, p.jurisdiction
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderKind;

    fn provider(url: &str, trust: Trust) -> Provider {
        Provider {
            id: "x".into(),
            name: "X".into(),
            kind: ProviderKind::OpenaiCompatible,
            base_url: url.into(),
            models: vec![],
            jurisdiction: "Somewhere".into(),
            trust,
            effort: None,
            max_tokens: 1000,
        }
    }

    #[test]
    fn urls() {
        assert!(validate_base_url("https://api.anthropic.com").is_ok());
        assert!(validate_base_url("http://localhost:11434/v1").is_ok());
        assert!(validate_base_url("http://127.0.0.1:1234/v1").is_ok());
        assert!(validate_base_url("http://[::1]:1234/v1").is_ok());
        assert!(validate_base_url("http://example.com/v1").is_err());
        assert!(validate_base_url("https://user:pw@example.com").is_err());
        assert!(validate_base_url("file:///etc/passwd").is_err());
        assert!(validate_base_url("http://localhost.evil.com").is_err());
    }

    #[test]
    fn device_trust_cannot_be_claimed_remotely() {
        assert_eq!(
            effective_trust(&provider("https://evil.example", Trust::Device)),
            Trust::Offshore
        );
        assert_eq!(
            effective_trust(&provider("http://localhost:11434/v1", Trust::Offshore)),
            Trust::Device
        );
    }

    #[test]
    fn tapu_stays_home() {
        assert!(check_destination(Classification::Tapu, &provider("https://api.example", Trust::Offshore)).is_err());
        assert!(check_destination(
            Classification::Tapu,
            &provider("https://iwi.example.nz", Trust::Sovereign)
        )
        .is_ok());
        assert!(check_destination(
            Classification::Tapu,
            &provider("http://localhost:11434/v1", Trust::Offshore)
        )
        .is_ok());
        assert!(check_destination(Classification::Noa, &provider("https://api.example", Trust::Offshore)).is_ok());
    }
}
