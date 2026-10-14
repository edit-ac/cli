#![allow(dead_code)]
use editac_cli::{
    auth::{AuthError, Credential, CredentialStore},
    engine::document::{CreationPermit, DocumentIdentity, InitializedCheckpoint},
    hosted::{self, HostedError, ServiceAdapter, ServiceConfig, ServiceError},
    reconcile::LocalBaseline,
    target::{ResolveOptions, resolve},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};
pub(crate) const TOKEN: &str = "prepared-secret-never-recorded";
#[derive(Default)]
pub(crate) struct Credentials {
    pub(crate) calls: usize,
    pub(crate) absent: bool,
    pub(crate) failed: bool,
}
impl CredentialStore for Credentials {
    fn load(&mut self, endpoint: &str) -> Result<Option<Credential>, AuthError> {
        self.calls += 1;
        assert_eq!(endpoint, "https://api.example.test/documents");
        if self.failed {
            return Err(AuthError::Unavailable);
        }
        if self.absent {
            return Ok(None);
        }
        Ok(Some(Credential::new(TOKEN.into()).unwrap()))
    }
}
pub(crate) struct Service {
    pub(crate) identity: DocumentIdentity,
    pub(crate) checkpoint: InitializedCheckpoint,
    pub(crate) resolves: usize,
    pub(crate) bootstraps: usize,
    pub(crate) failure: Option<ServiceError>,
    pub(crate) bootstrap_failure: Option<ServiceError>,
}
impl ServiceAdapter for Service {
    fn resolve(
        &mut self,
        endpoint: &str,
        id: &str,
        credential: &Credential,
    ) -> Result<DocumentIdentity, ServiceError> {
        self.resolves += 1;
        assert_eq!(endpoint, "https://api.example.test/documents");
        assert!(!id.is_empty());
        assert_eq!(credential.expose_to_adapter(), TOKEN);
        if let Some(error) = self.failure {
            return Err(error);
        }
        Ok(self.identity.clone())
    }
    fn bootstrap(
        &mut self,
        _: &str,
        _: &DocumentIdentity,
        credential: &Credential,
    ) -> Result<InitializedCheckpoint, ServiceError> {
        self.bootstraps += 1;
        assert_eq!(credential.expose_to_adapter(), TOKEN);
        if let Some(error) = self.bootstrap_failure {
            return Err(error);
        }
        Ok(self.checkpoint.clone())
    }
}
pub(crate) struct Harness {
    pub(crate) root: PathBuf,
    pub(crate) server_projection: editac_cli::engine::projection::Snapshot,
    pub(crate) identity: DocumentIdentity,
    pub(crate) config: ServiceConfig,
    pub(crate) url: String,
    pub(crate) credentials: Credentials,
    pub(crate) service: Service,
}
impl Harness {
    pub(crate) fn new() -> Self {
        Self::with_source(include_str!("../fixtures/hosted/source.md"))
    }
    pub(crate) fn with_source(source: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("editac-hosted-{}-{n}", std::process::id()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        }
        let identity = DocumentIdentity::v5(
            format!("ac000000-0000-4000-8000-{n:012x}"),
            "ac000000-0000-4000-8000-000000000000",
        )
        .unwrap();
        let path = root.join("server.md");
        fs::write(&path, source).unwrap();
        let target = resolve(&path, ResolveOptions::default()).unwrap();
        let (document, _) = LocalBaseline::import(
            target,
            CreationPermit::authorize_new(identity.clone()).unwrap(),
        )
        .unwrap();
        let checkpoint = document.checkpoint();
        let server_projection = document.snapshot().unwrap();
        let config = configuration(identity.clone());
        let url = format!("https://app.example.test/ac/{}", identity.document_id);
        Self {
            root,
            server_projection,
            identity: identity.clone(),
            config,
            url,
            credentials: Credentials::default(),
            service: Service {
                identity,
                checkpoint,
                resolves: 0,
                bootstraps: 0,
                failure: None,
                bootstrap_failure: None,
            },
        }
    }
    pub(crate) fn open(&mut self, offline: bool) -> Result<hosted::HostedSession, HostedError> {
        hosted::open(
            &self.url,
            &self.config,
            &self.root,
            offline,
            &mut self.credentials,
            &mut self.service,
        )
    }
    pub(crate) fn cache(&self) -> PathBuf {
        self.config.cache_path(&self.root)
    }
    pub(crate) fn rejected(&mut self, offline: bool, expected: HostedError) {
        match self.open(offline) {
            Err(error) => assert_eq!(error, expected),
            Ok(_) => panic!("unexpected hosted admission"),
        }
    }
    pub(crate) fn bytes(&self) -> Vec<(String, Vec<u8>)> {
        tree(&self.cache())
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
pub(crate) fn configuration(identity: DocumentIdentity) -> ServiceConfig {
    ServiceConfig::new(
        "app.example.test".into(),
        "https://api.example.test/documents".into(),
        identity,
    )
    .unwrap()
}
pub(crate) fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    if !root.exists() {
        return vec![];
    }
    let mut result = vec![];
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.path().is_dir() {
            result.extend(tree(&entry.path()));
        } else {
            result.push((
                entry.file_name().to_string_lossy().into(),
                fs::read(entry.path()).unwrap(),
            ));
        }
    }
    result.sort();
    result
}
