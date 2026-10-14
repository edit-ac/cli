//! Strict v1 document addressing. No transport, creation or filesystem IO occurs here.
use editac_core::document::DocumentIdentity;
use std::{collections::BTreeMap, fmt, path::PathBuf};

const VALIDATION_EPOCH: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocatorError {
    InvalidLocator,
    InvalidDocumentId,
    InvalidAuthority,
    UnknownAuthority,
    InvalidAppHost,
    UnknownAppHost,
    InvalidIdentity,
    ConflictingIdentity,
    DuplicateMapping,
    InvalidLocalPath,
}
impl fmt::Display for LocatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never echo credential-bearing input into logs.
        write!(f, "{self:?}")
    }
}
impl std::error::Error for LocatorError {}

/// Opaque engine ID. Parsing deliberately reuses the engine's admission grammar.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocumentId(String);
impl DocumentId {
    pub fn parse(value: &str) -> Result<Self, LocatorError> {
        DocumentIdentity::v1(value, VALIDATION_EPOCH)
            .map_err(|_| LocatorError::InvalidDocumentId)?;
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Configured host key, not a URL, hostname, credential or filesystem path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Authority(String);
impl Authority {
    pub fn parse(value: &str) -> Result<Self, LocatorError> {
        let bytes = value.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 63
            || !bytes[0].is_ascii_lowercase()
            || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
            || !bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        {
            return Err(LocatorError::InvalidAuthority);
        }
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Locator {
    authority: Option<Authority>,
    document_id: DocumentId,
}
impl Locator {
    /// Parse only canonical ac locators. Configured HTTPS links use Catalog::parse.
    pub fn parse(value: &str) -> Result<Self, LocatorError> {
        let rest = value
            .strip_prefix("ac://")
            .ok_or(LocatorError::InvalidLocator)?;
        // No URI decoding/case folding/path normalization: reject ambiguity at entry.
        if !value.is_ascii()
            || value
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
            || rest.contains(['%', '?', '#', '@', ':', '\\'])
        {
            return Err(LocatorError::InvalidLocator);
        }
        let (authority, id) = match rest.split_once('/') {
            Some((authority, id)) => (Some(Authority::parse(authority)?), id),
            None => (None, rest),
        };
        Ok(Self {
            authority,
            document_id: DocumentId::parse(id)?,
        })
    }
    pub fn authority(&self) -> Option<&Authority> {
        self.authority.as_ref()
    }
    pub fn document_id(&self) -> &DocumentId {
        &self.document_id
    }
}
impl fmt::Display for Locator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ac://")?;
        if let Some(authority) = &self.authority {
            write!(f, "{}/", authority.as_str())?;
        }
        write!(f, "{}", self.document_id.as_str())
    }
}

/// Host checks path permissions before constructing/inserting this binding.
/// Paths are never synthesized from a locator and need not be UTF-8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceBinding {
    LocalPath(PathBuf),
    Cached,
    Hosted { cached: bool },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessState {
    Allowed,
    Locked,
    Unauthorized,
    Incompatible,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub identity: DocumentIdentity,
    pub source: SourceBinding,
    pub access: AccessState,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connectivity {
    Offline,
    Online,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionState {
    Local,
    Cached,
    Hosted,
    Locked,
    Unavailable,
    Unauthorized,
    Incompatible,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolution<'a> {
    pub state: ResolutionState,
    pub entry: Option<&'a CatalogEntry>,
}

/// Prepared metadata catalog. Lookup is immutable and cannot fetch/create a doc.
#[derive(Default, Debug)]
pub struct Catalog {
    authorities: BTreeMap<Authority, ()>,
    app_hosts: BTreeMap<String, Authority>,
    entries: BTreeMap<Locator, CatalogEntry>,
}
impl Catalog {
    pub fn configure_authority(&mut self, name: &str) -> Result<Authority, LocatorError> {
        let authority = Authority::parse(name)?;
        self.authorities.insert(authority.clone(), ());
        Ok(authority)
    }
    /// Exact HTTPS host allowlist. Configuration grants no network/OS activation.
    pub fn configure_app_host(&mut self, host: &str, authority: &str) -> Result<(), LocatorError> {
        let authority = Authority::parse(authority)?;
        self.require_authority(&authority)?;
        if !valid_host(host) {
            return Err(LocatorError::InvalidAppHost);
        }
        if self.app_hosts.contains_key(host) {
            return Err(LocatorError::DuplicateMapping);
        }
        self.app_hosts.insert(host.into(), authority);
        Ok(())
    }
    pub fn parse(&self, value: &str) -> Result<Locator, LocatorError> {
        let locator = if let Some(rest) = value.strip_prefix("https://") {
            let (host, id) = rest
                .split_once("/ac/")
                .ok_or(LocatorError::InvalidLocator)?;
            if !valid_host(host) {
                return Err(LocatorError::InvalidAppHost);
            }
            let authority = self
                .app_hosts
                .get(host)
                .ok_or(LocatorError::UnknownAppHost)?;
            Locator::parse(&format!("ac://{}/{id}", authority.as_str()))?
        } else {
            Locator::parse(value)?
        };
        self.require_locator(&locator)?;
        Ok(locator)
    }
    /// Admit a complete identity without acquiring engine creation authority.
    /// An alias may link only to the same full identity, never another epoch/profile.
    pub fn insert(&mut self, locator: Locator, entry: CatalogEntry) -> Result<(), LocatorError> {
        self.require_locator(&locator)?;
        validate_identity(&entry.identity)?;
        if locator.document_id.as_str() != entry.identity.document_id {
            return Err(LocatorError::ConflictingIdentity);
        }
        if let SourceBinding::LocalPath(path) = &entry.source {
            if !path.is_absolute() {
                return Err(LocatorError::InvalidLocalPath);
            }
        }
        if matches!(entry.source, SourceBinding::Hosted { .. }) && locator.authority.is_none() {
            return Err(LocatorError::UnknownAuthority);
        }
        if self.entries.contains_key(&locator) {
            return Err(LocatorError::DuplicateMapping);
        }
        if self.entries.values().any(|existing| {
            existing.identity.document_id == entry.identity.document_id
                && existing.identity != entry.identity
        }) {
            return Err(LocatorError::ConflictingIdentity);
        }
        self.entries.insert(locator, entry);
        Ok(())
    }
    pub fn resolve(
        &self,
        locator: &Locator,
        connectivity: Connectivity,
    ) -> Result<Resolution<'_>, LocatorError> {
        self.require_locator(locator)?;
        let Some(entry) = self.entries.get(locator) else {
            return Ok(Resolution {
                state: ResolutionState::Unavailable,
                entry: None,
            });
        };
        let state = match entry.access {
            AccessState::Locked => ResolutionState::Locked,
            AccessState::Unauthorized => ResolutionState::Unauthorized,
            AccessState::Incompatible => ResolutionState::Incompatible,
            AccessState::Allowed => match &entry.source {
                SourceBinding::LocalPath(_) => ResolutionState::Local,
                SourceBinding::Cached | SourceBinding::Hosted { cached: true } => {
                    ResolutionState::Cached
                }
                SourceBinding::Hosted { cached: false } if connectivity == Connectivity::Online => {
                    ResolutionState::Hosted
                }
                SourceBinding::Hosted { cached: false } => ResolutionState::Unavailable,
            },
        };
        Ok(Resolution {
            state,
            entry: Some(entry),
        })
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    fn require_authority(&self, authority: &Authority) -> Result<(), LocatorError> {
        self.authorities
            .contains_key(authority)
            .then_some(())
            .ok_or(LocatorError::UnknownAuthority)
    }
    fn require_locator(&self, locator: &Locator) -> Result<(), LocatorError> {
        if let Some(authority) = &locator.authority {
            self.require_authority(authority)?;
        }
        Ok(())
    }
}
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            let b = label.as_bytes();
            !b.is_empty()
                && b.len() <= 63
                && b[0].is_ascii_alphanumeric()
                && b[b.len() - 1].is_ascii_alphanumeric()
                && b.iter()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        })
}
pub(crate) fn validate_identity(identity: &DocumentIdentity) -> Result<(), LocatorError> {
    // Compare the entire metadata tuple, preserving the engine's frozen profiles.
    let constructors: [fn(
        String,
        String,
    ) -> Result<DocumentIdentity, editac_core::document::DocumentError>; 5] = [
        DocumentIdentity::v1,
        DocumentIdentity::v2,
        DocumentIdentity::v3,
        DocumentIdentity::v4,
        DocumentIdentity::v5,
    ];
    if constructors.into_iter().any(|constructor| {
        constructor(identity.document_id.clone(), identity.epoch.clone()).as_ref() == Ok(identity)
    }) {
        Ok(())
    } else {
        Err(LocatorError::InvalidIdentity)
    }
}
