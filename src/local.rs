//! Local command orchestration over the qualified native boundaries.
use crate::{
    editor::{self, EditorCommand, EditorOutcome},
    engine::document::{CreationPermit, DocumentIdentity},
    files,
    reconcile::LocalBaseline,
    storage::{Draft, NativeStorage},
    target::{self, Format, ResolveOptions},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    version: u32,
    document: String,
    epoch: String,
    format: String,
}

fn uuid() -> Result<String, String> {
    let mut b = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| e.to_string())?;
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
fn private_dir(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.to_string()),
        }
        let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !m.is_dir() || m.permissions().mode() & 0o077 != 0 {
            return Err("recovery directory must be private (0700) and not a symlink".into());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err("unsupported-native-platform".into())
    }
}

pub fn edit(
    path: &Path,
    options: ResolveOptions,
    configured: Option<&str>,
    root: Option<&Path>,
) -> Result<serde_json::Value, String> {
    let target = target::resolve(path, options).map_err(|e| e.to_string())?;
    let command = EditorCommand::from_environment(configured).map_err(|e| e.to_string())?;
    let document_id = uuid()?;
    let epoch = uuid()?;
    let identity = DocumentIdentity::v5(&document_id, &epoch).map_err(|e| e.to_string())?;
    let (mut document, baseline) = LocalBaseline::import(
        target.clone(),
        CreationPermit::authorize_new(identity.clone()).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let default_root = target
        .resolved_path()
        .parent()
        .unwrap()
        .join(".editac-recovery");
    let root = root.unwrap_or(&default_root);
    private_dir(root)?;
    let session = root.join(&document_id);
    private_dir(&session)?;
    let result = (|| {
        let metadata = Session {
            version: 1,
            document: document_id,
            epoch,
            format: if target.format() == Format::Text {
                "text"
            } else {
                "markdown"
            }
            .into(),
        };
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(session.join("session.json"))
            .map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec(&metadata).map_err(|e| e.to_string())?)
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        File::open(&session)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        File::open(root)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        let store =
            NativeStorage::open(session.join("state"), identity).map_err(|e| e.to_string())?;
        store.initialize(&document).map_err(|e| e.to_string())?;
        let changed = match editor::run(&target, &command, &session).map_err(|e| e.to_string())? {
            EditorOutcome::Unchanged(_) => {
                fs::remove_dir_all(&session).map_err(|e| e.to_string())?;
                return Ok(
                    serde_json::json!({"state":"unchanged","remoteDurability":"not-applicable"}),
                );
            }
            EditorOutcome::Cancelled(_) => {
                return Ok(
                    serde_json::json!({"state":"cancelled","recovery":session,"remoteDurability":"not-applicable"}),
                );
            }
            EditorOutcome::Changed(buffer) => buffer,
        };
        store
            .persist_draft(&Draft {
                baseline: baseline.source().into(),
                source: changed.source().into(),
            })
            .map_err(|e| e.to_string())?;
        let edit = baseline
            .prepare(&document, changed)
            .map_err(|e| e.to_string())?
            .commit(&mut document)
            .map_err(|e| e.to_string())?;
        if let Some(commit) = &edit.commit {
            store
                .persist_update(&document, &commit.update, 1, "local-source")
                .map_err(|e| e.to_string())?;
        }
        files::save(edit).map_err(|e| e.to_string())?;
        Ok(
            serde_json::json!({"state":"locally-saved","recovery":session,"remoteDurability":"not-applicable"}),
        )
    })();
    result.map_err(|error: String| format!("{error}; recover session {}", session.display()))
}

/// Recovery exports to a NEW explicit destination. It never reapplies a stale draft to the original.
pub fn recover(session: &Path, output: Option<&Path>) -> Result<serde_json::Value, String> {
    let m = fs::symlink_metadata(session).map_err(|e| e.to_string())?;
    if !m.is_dir() {
        return Err("invalid-recovery-directory".into());
    }
    private_dir(session)?;
    let bytes =
        crate::diagnostics::read(&session.join("session.json")).map_err(|e| e.to_string())?;
    let metadata: Session =
        serde_json::from_slice(&bytes).map_err(|_| "invalid-session-metadata")?;
    if metadata.version != 1 {
        return Err("unsupported-session-version".into());
    }
    let identity =
        DocumentIdentity::v5(metadata.document, metadata.epoch).map_err(|e| e.to_string())?;
    let state = session.join("state");
    if !fs::symlink_metadata(&state)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("missing-native-state; retained without initialization".into());
    }
    let store = NativeStorage::open(state, identity).map_err(|e| e.to_string())?;
    let (draft, pending, fallback, committed) = match store.recover() {
        Ok(recovered) => {
            if recovered.rejected_draft.is_some() {
                return Err("corrupt-draft; retained without export".into());
            }
            (
                recovered.draft,
                Some(recovered.pending.len()),
                recovered.rejected_latest,
                true,
            )
        }
        Err(error) => {
            // Drafts have an independent sealed identity/checksum boundary.
            let draft = store
                .read_draft()
                .map_err(|e| e.to_string())?
                .ok_or_else(|| error.to_string())?;
            (Some(draft), None, Some(error.to_string()), false)
        }
    };
    let source = if let Some(draft) = draft {
        draft.source
    } else {
        // An interruption while the editor runs leaves its private exact buffer.
        let name = match metadata.format.as_str() {
            "text" => "source.txt",
            "markdown" => "source.md",
            _ => return Err("invalid-session-format".into()),
        };
        let mut buffers = Vec::new();
        for entry in fs::read_dir(session).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_type().map_err(|e| e.to_string())?.is_dir()
                && entry.path().join(name).exists()
            {
                buffers.push(entry.path().join(name));
            }
        }
        if buffers.len() != 1 {
            return Err("missing-or-ambiguous-editor-buffer".into());
        }
        String::from_utf8(crate::diagnostics::read(&buffers[0]).map_err(|e| e.to_string())?)
            .map_err(|_| "invalid-draft-utf8")?
    };
    if let Some(path) = output {
        let target = target::resolve(
            path,
            ResolveOptions {
                create: true,
                format: Some(Format::Text),
            },
        )
        .map_err(|e| e.to_string())?;
        if !matches!(target.baseline(), target::Baseline::Missing { .. }) {
            return Err("recovery-output-exists; choose a new path".into());
        }
        files::publish_bytes(&target, source.as_bytes(), &mut files::NativeFilesystem)
            .map_err(|(kind, _)| format!("recovery-publication-{kind:?}"))?;
    }
    Ok(
        serde_json::json!({"state":if committed { "recovered" } else { "draft-recovered" },"committedState":if committed { "recovered" } else { "unavailable" },"sourceBytes":source.len(),"pendingUpdates":pending,"recovery":session,"output":output,"remoteDurability":"not-applicable","fallback":fallback}),
    )
}
