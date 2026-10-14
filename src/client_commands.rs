//! Shared explicit command orchestration, also used by the native binary.
use crate::{
    host_adapter::HostAdapter,
    hosted::{HostedSession, ServiceConfig},
    status,
    storage::{NativeStorage, StoredIdentity},
    sync::{self, RetryPolicy},
};
use serde::Deserialize;
use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Configuration {
    host: String,
    endpoint: String,
    identity: StoredIdentity,
}
fn configuration(path: &Path) -> Result<ServiceConfig, String> {
    let mut bytes = vec![];
    fs::File::open(path)
        .map_err(|_| "service configuration unavailable")?
        .take(8193)
        .read_to_end(&mut bytes)
        .map_err(|_| "service configuration unreadable")?;
    if bytes.len() > 8192 {
        return Err("service configuration too large".into());
    }
    let c: Configuration =
        serde_json::from_slice(&bytes).map_err(|_| "invalid service configuration")?;
    let identity = [
        crate::engine::document::DocumentIdentity::v4,
        crate::engine::document::DocumentIdentity::v5,
    ]
    .into_iter()
    .filter_map(|make| make(c.identity.document_id.clone(), c.identity.epoch.clone()).ok())
    .find(|identity| StoredIdentity::from(identity) == c.identity)
    .ok_or("unsupported schema/epoch identity")?;
    ServiceConfig::new(c.host, c.endpoint, identity).map_err(|e| e.to_string())
}
/// Status never loads credentials; hosted edit bootstraps only through an explicit adapter.
pub fn execute(args: &[OsString]) -> Result<String, String> {
    let operation = args
        .first()
        .and_then(|s| s.to_str())
        .ok_or("command required")?;
    if !matches!(operation, "sync" | "status" | "edit") {
        return Err("unsupported client command".into());
    }
    let target = args.get(1).ok_or("target required")?;
    let mut config = None;
    let mut cache = None;
    let mut adapter = None;
    let mut attempts = None;
    let mut editor = None;
    let mut offline = false;
    let mut index = 2;
    while index < args.len() {
        let flag = args[index].to_str().ok_or("invalid option")?;
        index += 1;
        if flag == "--offline" && operation == "edit" && !offline {
            offline = true;
            continue;
        }
        let value = args.get(index).ok_or("option value required")?;
        match flag {
            "--service-config" if config.is_none() => config = Some(PathBuf::from(value)),
            "--cache-dir" if cache.is_none() => cache = Some(PathBuf::from(value)),
            "--editor" if operation == "edit" && editor.is_none() => {
                editor = Some(value.to_str().ok_or("invalid editor")?)
            }
            "--adapter" if operation != "status" && adapter.is_none() => {
                adapter = Some(PathBuf::from(value))
            }
            "--attempts" if operation != "status" && attempts.is_none() => {
                attempts = Some(
                    value
                        .to_str()
                        .ok_or("invalid attempts")?
                        .parse::<u8>()
                        .map_err(|_| "invalid attempts")?,
                )
            }
            _ => return Err("unexpected or duplicate option".into()),
        }
        index += 1;
    }
    if attempts.is_some_and(|n| !(1..=5).contains(&n)) {
        return Err("attempts must be between 1 and 5".into());
    }
    let url = target.to_str().ok_or("invalid target")?;
    if !url.starts_with("https://") {
        if operation != "status" || config.is_some() || cache.is_some() {
            return Err("sync requires configured hosted URL".into());
        }
        let opened = crate::target::resolve(Path::new(target), Default::default())
            .map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({"sourceAvailable":true,"sourceBytes":opened.source_bytes().len(),"localDurability":"unknown","pendingUpload":0,"remoteDurability":"unknown","connection":"offline"}).to_string());
    }
    let config = configuration(&config.ok_or("explicit service configuration required")?)?;
    config.resolve_target(url).map_err(|e| e.to_string())?;
    let cache = cache.ok_or("explicit private cache parent required")?;
    if operation == "edit" {
        if offline && adapter.is_some() {
            return Err("offline editing does not accept an adapter".into());
        }
        let executable = if offline {
            PathBuf::from("/offline-adapter-never-invoked")
        } else {
            adapter.ok_or("explicit host adapter executable required")?
        };
        let mut credentials = HostAdapter::new(executable.clone()).map_err(String::from)?;
        let mut service = HostAdapter::new(executable.clone()).map_err(String::from)?;
        let mut transport = HostAdapter::new(executable).map_err(String::from)?;
        let result = crate::hosted_edit::edit(
            url,
            &config,
            &cache,
            crate::hosted_edit::Options {
                offline,
                editor,
                retry: RetryPolicy {
                    attempts: attempts.unwrap_or(3),
                    ..Default::default()
                },
            },
            &mut credentials,
            &mut service,
            &mut transport,
        )?;
        return Ok(result.to_string());
    }
    let path = config.cache_path(&cache);
    if !path.try_exists().map_err(|_| "cache unavailable")? {
        return Err(
            "no cached hosted document; authorize bootstrap through configured host first".into(),
        );
    }
    let storage =
        NativeStorage::open(&path, config.identity().clone()).map_err(|e| e.to_string())?;
    let mut recovery = storage.recover().map_err(|e| e.to_string())?;
    recovery
        .document
        .enable_history(Default::default())
        .map_err(|_| "history restore failed")?;
    let mut session = HostedSession {
        recovery,
        storage,
        authorized_online: false,
    };
    if operation == "status" {
        return serde_json::to_string(&status::inspect(&session).map_err(|e| e.to_string())?)
            .map_err(|_| "status serialization failed".into());
    }
    let adapter = adapter.ok_or("explicit host adapter executable required")?;
    let mut credentials = HostAdapter::new(adapter.clone()).map_err(String::from)?;
    let mut transport = HostAdapter::new(adapter).map_err(String::from)?;
    let report = sync::synchronize(
        &mut session,
        &config,
        &mut credentials,
        &mut transport,
        RetryPolicy {
            attempts: attempts.unwrap_or(3),
            ..Default::default()
        },
    )
    .map_err(String::from)?;
    serde_json::to_string(&report).map_err(|_| "sync report serialization failed".into())
}
