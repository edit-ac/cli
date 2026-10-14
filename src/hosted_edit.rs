//! External-editor hosted workflow. Source files are inert private projections;
//! only the existing restored replica owns semantics and durable update identity.
use crate::{
    auth::CredentialStore,
    editor::{self, EditorCommand, EditorOutcome},
    hosted::{self, HostedSession, HostedSourceError, ServiceAdapter, ServiceConfig},
    reconcile::HostedConflict,
    storage::Draft,
    sync::{self, RetryPolicy, Transport},
};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub struct Options<'a> {
    pub offline: bool,
    pub editor: Option<&'a str>,
    pub retry: RetryPolicy,
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "cannot create private hosted source evidence")?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "cannot persist private hosted source evidence")?;
    File::open(path.parent().unwrap())
        .and_then(|f| f.sync_all())
        .map_err(|_| "hosted source evidence directory flush failed")?;
    Ok(())
}
fn workspace(session: &HostedSession) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let path = session.storage.root().join(format!(
        "source-session-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(|_| "cannot create hosted source session")?;
    File::open(session.storage.root())
        .and_then(|f| f.sync_all())
        .map_err(|_| "hosted session directory flush failed")?;
    Ok(path)
}
fn conflict(root: &Path, error: HostedConflict) -> Result<Value, String> {
    let checkpoint = &error.current_checkpoint;
    let evidence = json!({"state":"conflict", "reason":error.error.to_string(),
        "base":error.base.source, "local":error.local,
        "current":error.current.as_ref().ok().map(|p| &p.source),
        "currentCheckpoint":{"identity":crate::storage::StoredIdentity::from(&checkpoint.identity),
            "initialized":checkpoint.initialized,"writerId":checkpoint.writer_id,"bytes":checkpoint.bytes}});
    let path = root.join("conflict.json");
    private_write(
        &path,
        &serde_json::to_vec(&evidence).map_err(|_| "conflict serialization failed")?,
    )?;
    Ok(
        json!({"state":"conflict", "conflict":true, "recovery":root, "evidence":path,
        "locallySaved":false,"remoteDurability":"unknown"}),
    )
}
/// Explicit online mode may attach only through the supplied adapter. Offline
/// mode never calls credentials, service or transport, including on cache misses.
#[allow(clippy::too_many_arguments)]
pub fn edit(
    url: &str,
    config: &ServiceConfig,
    cache_parent: &Path,
    options: Options<'_>,
    credentials: &mut dyn CredentialStore,
    service: &mut dyn ServiceAdapter,
    transport: &mut dyn Transport,
) -> Result<Value, String> {
    let command = EditorCommand::from_environment(options.editor).map_err(|e| e.to_string())?;
    let mut session = hosted::open(
        url,
        config,
        cache_parent,
        options.offline,
        credentials,
        service,
    )
    .map_err(|e| e.to_string())?;
    if session.recovery.draft.is_some() || session.recovery.rejected_draft.is_some() {
        return Err(format!(
            "unresolved hosted draft; recover/export source before editing: {}",
            session.storage.root().display()
        ));
    }
    let mut connection = crate::status::ConnectionState::Offline;
    if !options.offline {
        connection = sync::synchronize(&mut session, config, credentials, transport, options.retry)
            .map_err(String::from)?
            .status
            .connection;
    }
    let baseline = session.source_baseline().map_err(|e| e.to_string())?;
    let root = workspace(&session)?;
    let projection = root.join("projection.md");
    private_write(&projection, baseline.opening().source.as_bytes())?;
    // No import: resolve only captures the projection bytes for the qualified
    // private editor runner. Never save this file back to the hosted document.
    let target =
        crate::target::resolve(&projection, Default::default()).map_err(|e| e.to_string())?;
    let changed = match editor::run(&target, &command, &root).map_err(|e| e.to_string())? {
        EditorOutcome::Unchanged(_) => {
            return Ok(json!({"state":"unchanged", "recovery":root,
            "status":crate::status::inspect(&session).map_err(|e| e.to_string())?}));
        }
        EditorOutcome::Cancelled(_) => {
            return Ok(json!({"state":"cancelled", "recovery":root,
            "remoteDurability":"unknown"}));
        }
        EditorOutcome::Changed(buffer) => buffer,
    };
    // Keep source before any post-editor network failure or semantic rejection.
    let draft = Draft {
        baseline: baseline.opening().source.clone(),
        source: changed.source().into(),
    };
    session
        .storage
        .persist_draft(&draft)
        .map_err(|e| format!("{e}; recover source at {}", root.display()))?;
    if !options.offline {
        connection = sync::synchronize(&mut session, config, credentials, transport, options.retry)
            .map_err(String::from)?
            .status
            .connection;
    }
    let plan = match session.prepare_source(&baseline, changed.source()) {
        Ok(plan) => plan,
        Err(HostedSourceError::Conflict(error)) => return conflict(&root, error),
        Err(error) => return Err(format!("{error}; recovery: {}", root.display())),
    };
    let commit = match plan.commit(&mut session.recovery.document) {
        Ok(commit) => commit,
        Err(error) => return conflict(&root, error),
    };
    if let Some(commit) = commit.commit {
        sync::persist_local(&mut session, &commit.update)
            .map_err(|e| format!("{e}; recover source at {}", root.display()))?;
    }
    if !session
        .storage
        .durability_barrier(&session.recovery.document, &crate::storage::NativeIo)
        .map_err(|e| e.to_string())?
    {
        return Err(format!(
            "local durability unavailable; recovery: {}",
            root.display()
        ));
    }
    // Clear only this admitted, durably committed draft, never another writer's.
    session
        .storage
        .clear_draft(&draft)
        .map_err(|e| format!("{e}; recovery: {}", root.display()))?;
    let mut status = crate::status::inspect(&session).map_err(|e| e.to_string())?;
    status.connection = connection;
    if !options.offline {
        status = sync::synchronize(&mut session, config, credentials, transport, options.retry)
            .map_err(String::from)?
            .status;
    }
    Ok(json!({"state":"locally-saved", "recovery":root,"status":status}))
}
