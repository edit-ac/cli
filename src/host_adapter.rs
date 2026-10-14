//! Explicit host executable glue. JSON here is private local IPC, not a new
//! server protocol: the host maps qualified baseline frames/receipt envelopes
//! onto its existing authenticated service. No executable or credentials are
//! selected from ambient environment. Each invocation is bounded and reaped.
use crate::{
    auth::{AuthError, Credential, CredentialStore},
    storage::{JournalEntry, StoredIdentity},
    sync::{Delivery, Negotiation, Transport, TransportError},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
const MAX_IPC_BYTES: u64 = 96 * 1024 * 1024;
pub struct HostAdapter {
    executable: PathBuf,
    attachment: String,
}
impl HostAdapter {
    pub fn new(executable: PathBuf) -> Result<Self, &'static str> {
        if !executable.is_absolute() {
            return Err("adapter executable must be an explicit absolute path");
        }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Ok(Self {
            executable,
            attachment: format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
        })
    }
    fn call<T: DeserializeOwned>(&self, mut request: Value) -> Result<T, TransportError> {
        request
            .as_object_mut()
            .ok_or(TransportError::InvalidProtocol)?
            .insert("attachment".into(), Value::String(self.attachment.clone()));
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "editac-host-adapter-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .map_err(|_| TransportError::Unavailable)?;
        }
        #[cfg(not(unix))]
        {
            return Err(TransportError::Unavailable);
        }
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let run = || -> Result<T, TransportError> {
            let input = root.join("request.json");
            let output = root.join("response.json");
            let private = |path: &PathBuf| -> std::io::Result<File> {
                let mut options = OpenOptions::new();
                options.create_new(true).read(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options.open(path)
            };
            let mut input_file = private(&input).map_err(|_| TransportError::Unavailable)?;
            let bytes =
                serde_json::to_vec(&request).map_err(|_| TransportError::InvalidProtocol)?;
            if bytes.len() as u64 > MAX_IPC_BYTES {
                return Err(TransportError::InvalidProtocol);
            }
            input_file
                .write_all(&bytes)
                .map_err(|_| TransportError::Unavailable)?;
            let mut response_file = private(&output).map_err(|_| TransportError::Unavailable)?;
            let mut child = Command::new(&self.executable)
                .env_clear()
                .stdin(File::open(&input).map_err(|_| TransportError::Unavailable)?)
                .stdout(
                    response_file
                        .try_clone()
                        .map_err(|_| TransportError::Unavailable)?,
                )
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| TransportError::Unavailable)?;
            let start = Instant::now();
            let outcome = loop {
                if start.elapsed() > Duration::from_secs(3)
                    || response_file
                        .metadata()
                        .map_or(true, |m| m.len() > MAX_IPC_BYTES)
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(TransportError::Unavailable);
                }
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(TransportError::Unavailable);
                    }
                }
            };
            if !outcome.success() {
                return Err(TransportError::Unavailable);
            }
            response_file
                .seek(SeekFrom::Start(0))
                .map_err(|_| TransportError::Unavailable)?;
            let mut bytes = vec![];
            response_file
                .take(MAX_IPC_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| TransportError::Unavailable)?;
            if bytes.len() as u64 > MAX_IPC_BYTES {
                return Err(TransportError::InvalidProtocol);
            }
            let response: Value =
                serde_json::from_slice(&bytes).map_err(|_| TransportError::InvalidProtocol)?;
            if response.as_object().is_none_or(|o| o.len() != 1) {
                return Err(TransportError::InvalidProtocol);
            }
            if let Some(error) = response.get("error") {
                return Err(serde_json::from_value(error.clone())
                    .unwrap_or(TransportError::InvalidProtocol));
            }
            serde_json::from_value(
                response
                    .get("ok")
                    .ok_or(TransportError::InvalidProtocol)?
                    .clone(),
            )
            .map_err(|_| TransportError::InvalidProtocol)
        };
        run()
    }
}
impl CredentialStore for HostAdapter {
    fn load(&mut self, endpoint: &str) -> Result<Option<Credential>, AuthError> {
        let token: Option<String> = self
            .call(json!({"operation":"credential", "endpoint":endpoint}))
            .map_err(|_| AuthError::Unavailable)?;
        token.map(Credential::new).transpose()
    }
}
impl Transport for HostAdapter {
    fn connect(
        &mut self,
        endpoint: &str,
        identity: &StoredIdentity,
        credential: &Credential,
    ) -> Result<Negotiation, TransportError> {
        self.call(json!({"operation":"connect", "endpoint":endpoint, "identity":identity, "credential":credential.expose_to_adapter()}))
    }
    fn exchange(&mut self, frames: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, TransportError> {
        self.call(json!({"operation":"exchange", "frames":frames}))
    }
    fn upload(&mut self, entry: &JournalEntry) -> Result<Delivery, TransportError> {
        self.call(json!({"operation":"upload", "entry":entry}))
    }
    fn disconnect(&mut self) {
        let _: Result<(), _> = self.call(json!({"operation":"disconnect"}));
    }
}

impl crate::hosted::ServiceAdapter for HostAdapter {
    fn resolve(
        &mut self,
        endpoint: &str,
        document_id: &str,
        credential: &Credential,
    ) -> Result<crate::engine::document::DocumentIdentity, crate::hosted::ServiceError> {
        let stored: StoredIdentity = self
            .call(json!({"operation":"resolve", "endpoint":endpoint,
            "documentId":document_id, "credential":credential.expose_to_adapter()}))
            .map_err(service_error)?;
        identity(stored)
    }
    fn bootstrap(
        &mut self,
        endpoint: &str,
        identity: &crate::engine::document::DocumentIdentity,
        credential: &Credential,
    ) -> Result<crate::engine::document::InitializedCheckpoint, crate::hosted::ServiceError> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Checkpoint {
            identity: StoredIdentity,
            initialized: bool,
            writer_id: u32,
            bytes: Vec<u8>,
        }
        let c: Checkpoint = self.call(json!({"operation":"bootstrap", "endpoint":endpoint,
            "identity":StoredIdentity::from(identity), "credential":credential.expose_to_adapter()})).map_err(service_error)?;
        Ok(crate::engine::document::InitializedCheckpoint {
            identity: self::identity(c.identity)?,
            initialized: c.initialized,
            writer_id: c.writer_id,
            bytes: c.bytes,
        })
    }
}
fn service_error(error: TransportError) -> crate::hosted::ServiceError {
    match error {
        TransportError::Authorization | TransportError::Revoked => {
            crate::hosted::ServiceError::Authorization
        }
        _ => crate::hosted::ServiceError::Unavailable,
    }
}

fn identity(
    stored: StoredIdentity,
) -> Result<crate::engine::document::DocumentIdentity, crate::hosted::ServiceError> {
    [
        crate::engine::document::DocumentIdentity::v4,
        crate::engine::document::DocumentIdentity::v5,
    ]
    .into_iter()
    .filter_map(|make| make(stored.document_id.clone(), stored.epoch.clone()).ok())
    .find(|i| StoredIdentity::from(i) == stored)
    .ok_or(crate::hosted::ServiceError::Unavailable)
}
