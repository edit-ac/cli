//! Authorized hosted acquisition. No transport attaches to the returned replica.
//! First bootstrap is an initialized CRDT checkpoint, never Markdown reimport.
use crate::{
    auth::Credential,
    auth::CredentialStore,
    engine::document::{Document, DocumentIdentity, InitializedCheckpoint},
    storage::{FailureKind, NativeStorage, Recovery},
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedError {
    InvalidConfiguration,
    UnsupportedTarget,
    OfflineUnavailable,
    CredentialUnavailable,
    Authorization,
    ServiceUnavailable,
    IncompatibleIdentity,
    InvalidBootstrap,
    CacheRecoveryRequired,
    CacheUnavailable,
}
impl std::fmt::Display for HostedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidConfiguration => "invalid explicit hosted service configuration",
            Self::UnsupportedTarget => "target is not a document URL for the configured service",
            Self::OfflineUnavailable => "no cached document is available offline",
            Self::CredentialUnavailable => "saved service credential is unavailable",
            Self::Authorization => "document authorization failed; retain local work",
            Self::ServiceUnavailable => {
                "configured service unavailable; reopen cached work offline"
            }
            Self::IncompatibleIdentity => {
                "document/schema/epoch mismatch; retain the existing cache"
            }
            Self::InvalidBootstrap => "invalid initialized checkpoint; no replacement was created",
            Self::CacheRecoveryRequired => {
                "existing cache requires recovery; no replacement was created"
            }
            Self::CacheUnavailable => "native cache unavailable; retain recovery files",
        })
    }
}
impl std::error::Error for HostedError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceError {
    Authorization,
    Unavailable,
}

/// Existing service protocol adapters return qualified engine envelopes, not a
/// new CLI wire format. The adapter owns authentication, endpoint I/O and bounds.
pub trait ServiceAdapter {
    fn resolve(
        &mut self,
        endpoint: &str,
        document_id: &str,
        credential: &Credential,
    ) -> Result<DocumentIdentity, ServiceError>;
    fn bootstrap(
        &mut self,
        endpoint: &str,
        identity: &DocumentIdentity,
        credential: &Credential,
    ) -> Result<InitializedCheckpoint, ServiceError>;
}

/// Host-authorized endpoint and full expected identity (including schema/epoch).
/// Constructing configuration performs no credential or service access.
pub struct ServiceConfig {
    host: String,
    endpoint: String,
    identity: DocumentIdentity,
}

fn dns_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}

impl ServiceConfig {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn identity(&self) -> &DocumentIdentity {
        &self.identity
    }
    pub fn new(
        host: String,
        endpoint: String,
        identity: DocumentIdentity,
    ) -> Result<Self, HostedError> {
        let authority = endpoint
            .strip_prefix("https://")
            .and_then(|s| s.split('/').next())
            .unwrap_or("");
        let valid_identity = [DocumentIdentity::v4, DocumentIdentity::v5]
            .into_iter()
            .any(|make| {
                make(identity.document_id.clone(), identity.epoch.clone())
                    .is_ok_and(|candidate| candidate == identity)
            });
        if !dns_host(&host)
            || !dns_host(authority)
            || !valid_identity
            || endpoint.len() > 2048
            || endpoint
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            || endpoint.contains(['?', '#', '@', '%', '\\'])
        {
            return Err(HostedError::InvalidConfiguration);
        }
        Ok(Self {
            host,
            endpoint,
            identity,
        })
    }

    pub fn resolve_target(&self, url: &str) -> Result<(), HostedError> {
        if url != format!("https://{}/ac/{}", self.host, self.identity.document_id) {
            return Err(HostedError::UnsupportedTarget);
        }
        Ok(())
    }

    /// Epoch/schema intentionally excluded: incompatible reopening must inspect
    /// and reject the old cache, rather than silently hiding it in a new path.
    pub fn cache_path(&self, private_cache_parent: &Path) -> PathBuf {
        let key = serde_json::to_vec(&(&self.endpoint, &self.identity.document_id)).unwrap();
        private_cache_parent.join(format!("hosted-{:x}", Sha256::digest(key)))
    }
}

pub struct HostedSession {
    pub recovery: Recovery,
    pub storage: NativeStorage,
    /// False for an offline open. Authorization is never inferred from a cache.
    pub authorized_online: bool,
}

#[derive(Debug)]
pub enum HostedSourceError {
    Draft(crate::storage::StorageError),
    Conflict(crate::reconcile::HostedConflict),
}
impl std::fmt::Display for HostedSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft(e) => write!(f, "draft persistence failed: {e}; retain editor source"),
            Self::Conflict(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for HostedSourceError {}

impl HostedSession {
    pub fn source_baseline(
        &self,
    ) -> Result<crate::reconcile::HostedBaseline, crate::reconcile::ReconcileError> {
        crate::reconcile::HostedBaseline::capture(&self.recovery.document)
    }
    /// Structured receives advance this exact replica. No parsing, bootstrap,
    /// history reset or Markdown import occurs. Transport/durability is separate.
    pub fn apply_received(
        &mut self,
        update: &crate::engine::document::ReplayUpdate,
    ) -> Result<(), crate::engine::document::DocumentError> {
        self.recovery.document.apply_replay(update)
    }
    /// Preserve the exact unresolved editor source at the native draft boundary
    /// before parsing or preparing any mutation. This is not a saved CRDT update.
    pub fn prepare_source(
        &self,
        baseline: &crate::reconcile::HostedBaseline,
        source: &str,
    ) -> Result<crate::reconcile::PreparedHostedEdit, HostedSourceError> {
        self.storage
            .persist_draft(&crate::storage::Draft {
                baseline: baseline.opening().source.clone(),
                source: source.into(),
            })
            .map_err(HostedSourceError::Draft)?;
        baseline
            .prepare(&self.recovery.document, source)
            .map_err(HostedSourceError::Conflict)
    }
}

fn service_error(error: ServiceError) -> HostedError {
    match error {
        ServiceError::Authorization => HostedError::Authorization,
        ServiceError::Unavailable => HostedError::ServiceUnavailable,
    }
}

fn private_parent(path: &Path) -> Result<(), HostedError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path).map_err(|_| HostedError::CacheUnavailable)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(HostedError::CacheUnavailable);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(HostedError::CacheUnavailable)
    }
}

/// Offline opens never call credentials or service. Online cached opens authorize
/// but never bootstrap/reinitialize. Every existing directory is recovery-only,
/// including an interrupted initialization: explicit recovery must resolve it.
pub fn open(
    url: &str,
    config: &ServiceConfig,
    private_cache_parent: &Path,
    offline: bool,
    credentials: &mut dyn CredentialStore,
    service: &mut dyn ServiceAdapter,
) -> Result<HostedSession, HostedError> {
    config.resolve_target(url)?;
    private_parent(private_cache_parent)?;
    let path = config.cache_path(private_cache_parent);
    let exists = match fs::symlink_metadata(&path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err(HostedError::CacheUnavailable),
    };
    if offline && !exists {
        return Err(HostedError::OfflineUnavailable);
    }
    let credential = if offline {
        None
    } else {
        let credential = credentials
            .load(&config.endpoint)
            .map_err(|_| HostedError::CredentialUnavailable)?
            .ok_or(HostedError::CredentialUnavailable)?;
        let identity = service
            .resolve(&config.endpoint, &config.identity.document_id, &credential)
            .map_err(service_error)?;
        if identity != config.identity {
            return Err(HostedError::IncompatibleIdentity);
        }
        Some(credential)
    };
    if exists {
        let storage = NativeStorage::open(&path, config.identity.clone())
            .map_err(|_| HostedError::CacheRecoveryRequired)?;
        let mut recovery = storage.recover().map_err(|error| {
            if error.kind == FailureKind::IncompatibleIdentity {
                HostedError::IncompatibleIdentity
            } else {
                HostedError::CacheRecoveryRequired
            }
        })?;
        recovery
            .document
            .enable_history(Default::default())
            .map_err(|_| HostedError::CacheRecoveryRequired)?;
        return Ok(HostedSession {
            recovery,
            storage,
            authorized_online: !offline,
        });
    }
    let checkpoint = service
        .bootstrap(
            &config.endpoint,
            &config.identity,
            credential.as_ref().unwrap(),
        )
        .map_err(service_error)?;
    if checkpoint.identity != config.identity {
        return Err(HostedError::IncompatibleIdentity);
    }
    let mut document = Document::restore(&config.identity, &checkpoint, &[])
        .map_err(|_| HostedError::InvalidBootstrap)?;
    document
        .enable_history(Default::default())
        .map_err(|_| HostedError::InvalidBootstrap)?;
    // Reserve creation exclusively. A concurrent opener cannot initialize over
    // this directory; it must retry recovery after the other publisher finishes.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| HostedError::CacheRecoveryRequired)?;
        fs::File::open(private_cache_parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| HostedError::CacheUnavailable)?;
    }
    let storage = NativeStorage::open(&path, config.identity.clone())
        .map_err(|_| HostedError::CacheUnavailable)?;
    storage
        .initialize(&document)
        .map_err(|_| HostedError::CacheRecoveryRequired)?;
    // Return the already admitted replica, not a second recovery/restore.
    let recovery = Recovery {
        document,
        point: crate::storage::RecoveryPoint::Latest,
        pending: vec![],
        tail: vec![],
        coverage: vec![],
        draft: None,
        rejected_latest: None,
        rejected_draft: None,
    };
    Ok(HostedSession {
        recovery,
        storage,
        authorized_online: true,
    })
}
