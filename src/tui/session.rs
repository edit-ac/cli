//! Acquisition only. Catalog grants come from the host, never from locator text.
//! No transport or ambient credential discovery is performed here.
use crate::{
    engine::document::{CreationPermit, DocumentIdentity},
    markdown::reconcile::SourceMode,
    reconcile::LocalBaseline,
    storage::StoredIdentity,
    target::{self, Baseline, Format, ResolveOptions},
};
use editac_session::{
    edit_service::{EditService, HostPolicy},
    locator::{Catalog, Locator},
    ports::{CheckpointRecord, LocalStore, StoreLimits},
    session::{CachedPermission, Session, SessionStore},
    store::NativeStore,
};
use serde::Deserialize;
use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Representation {
    ExactSource,
    HostedProjection,
}

pub struct TerminalSession<S: SessionStore> {
    pub service: EditService<S>,
    pub source: String,
    pub mode: SourceMode,
    pub representation: Representation,
    /// Local publication retains the original guarded filesystem target.
    pub local_baseline: Option<LocalBaseline>,
}

/// A catalog hit is insufficient: restore and validate its actual checkpoint first.
pub fn open_cached<S: SessionStore>(
    store: S,
    catalog: &Catalog,
    locator: &Locator,
    permission: CachedPermission,
    policy: HostPolicy,
) -> Result<TerminalSession<S>, &'static str> {
    let session = Session::open_locator(store, catalog, locator, permission)
        .map_err(|_| "catalog session unavailable, unauthorized, locked or incompatible")?;
    let source = crate::reconcile::HostedProjection::capture(session.document())
        .map_err(|_| "cached source projection unavailable; retain recovery data")?;
    Ok(TerminalSession {
        source: source.source,
        service: EditService::new(session, policy),
        mode: SourceMode::Markdown(Default::default()),
        representation: Representation::HostedProjection,
        local_baseline: None,
    })
}

/// Explicit local import reuses the CLI source importer and the shared native store.
/// The store must be a new host-granted path. Existing stores are never overwritten.
pub fn open_local(
    path: &Path,
    options: ResolveOptions,
    store_path: &Path,
    identity: DocumentIdentity,
) -> Result<TerminalSession<NativeStore>, &'static str> {
    let target = target::resolve(path, options).map_err(
        |_| "local target unavailable; missing files require --create; unknown suffixes require --format",
    )?;
    let mode = match target.format() {
        Format::Markdown => SourceMode::Markdown(Default::default()),
        Format::Text => SourceMode::Literal,
    };
    let (doc, baseline) = LocalBaseline::import(
        target,
        CreationPermit::authorize_new(identity.clone()).map_err(|_| "invalid local identity")?,
    )
    .map_err(|_| "local source import rejected; retain original bytes")?;
    let checkpoint =
        CheckpointRecord::new(doc.checkpoint(), vec![]).map_err(|_| "invalid local checkpoint")?;
    let store = NativeStore::create_new(store_path, identity.clone(), StoreLimits::default())
        .map_err(|_| "new private session store unavailable; existing stores cannot be replaced")?;
    store
        .publish(0, checkpoint)
        .map_err(|_| "local initialization failed; retain session store for recovery")?;
    let writable = !matches!(
        baseline.target().baseline(),
        Baseline::Existing { readonly: true, .. }
    );
    let session = Session::open(
        store,
        &identity,
        CachedPermission {
            writable,
            expires_at: None,
        },
    )
    .map_err(|_| "local session restore failed; retain session store for recovery")?;
    Ok(TerminalSession {
        source: baseline.source().into(),
        service: EditService::new(session, HostPolicy::default()),
        mode,
        representation: Representation::ExactSource,
        local_baseline: Some(baseline),
    })
}

#[derive(Debug)]
pub struct Request {
    pub target: OsString,
    pub options: ResolveOptions,
    pub store: Option<PathBuf>,
    pub catalog: Option<PathBuf>,
}
impl Request {
    pub fn parse(args: &[OsString]) -> Result<Self, &'static str> {
        if args.first().is_none_or(|v| v != "tui") {
            return Err("tui command required");
        }
        let target = args.get(1).ok_or("target required")?.clone();
        let mut request = Self {
            target,
            options: ResolveOptions::default(),
            store: None,
            catalog: None,
        };
        let mut seen = std::collections::HashSet::new();
        let mut i = 2;
        while i < args.len() {
            let flag = args[i].to_str().ok_or("invalid option")?;
            if !seen.insert(flag) {
                return Err("duplicate option");
            }
            i += 1;
            match flag {
                "--create" => request.options.create = true,
                "--offline" => {}
                "--format" | "--store" | "--catalog" => {
                    let value = args.get(i).ok_or("option value required")?;
                    i += 1;
                    match flag {
                        "--format" => {
                            request.options.format = Some(
                                Format::parse(value.to_str().ok_or("invalid format")?)
                                    .map_err(|_| "invalid format")?,
                            )
                        }
                        "--store" => request.store = Some(PathBuf::from(value)),
                        _ => request.catalog = Some(PathBuf::from(value)),
                    }
                }
                _ => return Err("unexpected option"),
            }
        }
        if request.remote() {
            if request.options.create || request.options.format.is_some() || request.store.is_some()
            {
                return Err("remote targets cannot create or import local files");
            }
            if request.catalog.is_none() {
                return Err("host-granted catalog required");
            }
        } else if request.catalog.is_some() || request.store.is_none() {
            return Err("local targets require an explicit new --store path");
        }
        Ok(request)
    }
    fn remote(&self) -> bool {
        self.target.to_str().is_some_and(|s| s.contains("://"))
    }
    pub fn open(&self) -> Result<TerminalSession<NativeStore>, &'static str> {
        if self.remote() {
            open_catalog_file(
                self.catalog.as_deref().ok_or("catalog required")?,
                self.target.to_str().ok_or("invalid locator")?,
            )
        } else {
            let identity =
                DocumentIdentity::v5(uuid()?, uuid()?).map_err(|_| "invalid new identity")?;
            open_local(
                Path::new(&self.target),
                self.options,
                self.store.as_deref().ok_or("store required")?,
                identity,
            )
        }
    }
}
fn uuid() -> Result<String, &'static str> {
    let mut b = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|_| "local identity generator unavailable")?;
    b[6] = (b[6] & 15) | 64;
    b[8] = (b[8] & 63) | 128;
    let h: String = b.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// This is host configuration, separate from semantic document content. Unknown
/// fields (including credentials) reject; diagnostics never echo configuration.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogFile {
    authorities: Vec<AuthorityGrant>,
    entries: Vec<EntryGrant>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthorityGrant {
    name: String,
    app_host: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EntryGrant {
    locator: String,
    identity: StoredIdentity,
    store: Option<PathBuf>,
    writable: bool,
    expires_at: Option<u64>,
    access: String,
}
fn open_catalog_file(
    path: &Path,
    target: &str,
) -> Result<TerminalSession<NativeStore>, &'static str> {
    use editac_session::locator::{AccessState, CatalogEntry, SourceBinding};
    let mut bytes = vec![];
    fs::File::open(path)
        .map_err(|_| "catalog unavailable")?
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| "catalog unreadable")?;
    if bytes.len() > 65_536 {
        return Err("catalog too large");
    }
    let config: CatalogFile =
        serde_json::from_slice(&bytes).map_err(|_| "invalid host catalog configuration")?;
    if config.entries.len() > 128 || config.authorities.len() > 64 {
        return Err("catalog resource limit");
    }
    let mut catalog = Catalog::default();
    for grant in config.authorities {
        catalog
            .configure_authority(&grant.name)
            .map_err(|_| "invalid catalog authority")?;
        if let Some(host) = grant.app_host {
            catalog
                .configure_app_host(&host, &grant.name)
                .map_err(|_| "invalid catalog app host")?;
        }
    }
    let mut stores = std::collections::BTreeMap::new();
    for grant in config.entries {
        let identity = [DocumentIdentity::v4, DocumentIdentity::v5]
            .into_iter()
            .filter_map(|make| {
                make(
                    grant.identity.document_id.clone(),
                    grant.identity.epoch.clone(),
                )
                .ok()
            })
            .find(|i| StoredIdentity::from(i) == grant.identity)
            .ok_or("invalid catalog identity")?;
        let locator = catalog
            .parse(&grant.locator)
            .map_err(|_| "invalid catalog locator")?;
        if grant.store.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err("catalog store must be host-granted absolute path");
        }
        let access = match grant.access.as_str() {
            "allowed" => AccessState::Allowed,
            "unauthorized" => AccessState::Unauthorized,
            "locked" => AccessState::Locked,
            "incompatible" => AccessState::Incompatible,
            _ => return Err("invalid catalog access"),
        };
        catalog
            .insert(
                locator.clone(),
                CatalogEntry {
                    identity,
                    source: if grant.store.is_some() {
                        SourceBinding::Cached
                    } else {
                        SourceBinding::Hosted { cached: false }
                    },
                    access,
                },
            )
            .map_err(|_| "conflicting catalog identity")?;
        if stores
            .insert(
                locator.to_string(),
                (
                    grant.store,
                    CachedPermission {
                        writable: grant.writable,
                        expires_at: grant.expires_at,
                    },
                ),
            )
            .is_some()
        {
            return Err("duplicate catalog grant");
        }
    }
    let locator = catalog
        .parse(target)
        .map_err(|_| "unknown or invalid target authority")?;
    // Resolve policy before opening any database. URI text can never select a path.
    let resolved = catalog
        .resolve(&locator, editac_session::locator::Connectivity::Offline)
        .map_err(|_| "catalog resolution rejected")?;
    if !matches!(
        resolved.state,
        editac_session::locator::ResolutionState::Cached
    ) {
        return Err("catalog target unavailable, unauthorized, locked or incompatible");
    }
    let entry = resolved
        .entry
        .ok_or("uncached target unavailable offline")?;
    let (path, permission) = stores
        .get(&locator.to_string())
        .ok_or("uncached target unavailable offline")?;
    let path = path.as_ref().ok_or("uncached target unavailable offline")?;
    let store = NativeStore::open_existing(path, entry.identity.clone(), StoreLimits::default())
        .map_err(|_| "cached session unavailable; no replacement created")?;
    open_cached(
        store,
        &catalog,
        &locator,
        *permission,
        HostPolicy::default(),
    )
}
