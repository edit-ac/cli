//! CLI-1.7 canonical prepared authorization -> real replica/native cache harness.
use editac_cli::{
    auth::Credential,
    engine::{
        Utf16Offset,
        anchors::Bias,
        commands::text::{TextCommand, TextOrigin},
        document::DocumentIdentity,
    },
    hosted::{self, HostedError, ServiceConfig, ServiceError},
    storage::Draft,
};
use std::fs;
#[path = "support/hosted.rs"]
mod harness;
use harness::*;
#[test]
fn authorized_bootstrap_restores_exact_shared_checkpoint_once() {
    let mut h = Harness::new();
    let a = h.open(false).unwrap();
    assert!(a.authorized_online);
    assert_eq!(a.recovery.document.identity(), &h.identity);
    // Yrs may reorder map/Any encoding on restore; compare retained shared
    // identities, sources, marks and opaque structure, not incidental bytes.
    same_projection(
        a.recovery.document.snapshot().unwrap(),
        h.server_projection.clone(),
    );
    assert!(a.recovery.pending.is_empty());
    let bytes = h.bytes();
    let b = h.open(false).unwrap();
    assert_eq!(
        (
            h.credentials.calls,
            h.service.resolves,
            h.service.bootstraps
        ),
        (2, 2, 1)
    );
    assert_ne!(
        a.recovery.document.writer_id(),
        b.recovery.document.writer_id()
    );
    assert_eq!(h.bytes(), bytes);
    same_projection(
        a.recovery.document.snapshot().unwrap(),
        b.recovery.document.snapshot().unwrap(),
    );
}
#[test]
fn cached_offline_open_has_no_auth_or_transport_access() {
    let mut h = Harness::new();
    drop(h.open(false).unwrap());
    h.credentials.failed = true;
    h.service.failure = Some(ServiceError::Unavailable);
    let session = h.open(true).unwrap();
    assert!(!session.authorized_online);
    assert_eq!(
        (
            h.credentials.calls,
            h.service.resolves,
            h.service.bootstraps
        ),
        (1, 1, 1)
    );
    assert_eq!(session.recovery.document.identity(), &h.identity);
}
#[test]
fn uncached_offline_never_creates_empty_replacement() {
    let mut h = Harness::new();
    h.rejected(true, HostedError::OfflineUnavailable);
    assert!(!h.cache().exists());
    assert_eq!(
        (
            h.credentials.calls,
            h.service.resolves,
            h.service.bootstraps
        ),
        (0, 0, 0)
    );
}
#[test]
fn authorization_failure_before_bootstrap_creates_nothing() {
    let mut h = Harness::new();
    h.service.failure = Some(ServiceError::Authorization);
    h.rejected(false, HostedError::Authorization);
    assert!(!h.cache().exists());
    assert_eq!(h.service.bootstraps, 0);
}
#[test]
fn service_unavailable_before_bootstrap_creates_nothing() {
    let mut h = Harness::new();
    h.service.failure = Some(ServiceError::Unavailable);
    h.rejected(false, HostedError::ServiceUnavailable);
    assert!(!h.cache().exists());
}
#[test]
fn missing_saved_credential_does_not_discover_ambient_keys() {
    let mut h = Harness::new();
    h.credentials.absent = true;
    h.rejected(false, HostedError::CredentialUnavailable);
    assert_eq!(h.service.resolves, 0);
    assert!(!h.cache().exists());
}
#[test]
fn credential_store_failure_does_not_attach_service() {
    let mut h = Harness::new();
    h.credentials.failed = true;
    h.rejected(false, HostedError::CredentialUnavailable);
    assert_eq!(h.service.resolves, 0);
}
#[test]
fn bootstrap_authorization_failure_creates_nothing() {
    let mut h = Harness::new();
    h.service.bootstrap_failure = Some(ServiceError::Authorization);
    h.rejected(false, HostedError::Authorization);
    assert!(!h.cache().exists());
}
#[test]
fn bootstrap_unavailable_creates_nothing() {
    let mut h = Harness::new();
    h.service.bootstrap_failure = Some(ServiceError::Unavailable);
    h.rejected(false, HostedError::ServiceUnavailable);
    assert!(!h.cache().exists());
}
#[test]
fn resolved_document_identity_mismatch_is_rejected() {
    let mut h = Harness::new();
    h.service.identity.document_id = "ac000000-0000-4000-8000-000000ffffff".into();
    h.rejected(false, HostedError::IncompatibleIdentity);
    assert!(!h.cache().exists());
}
#[test]
fn resolved_epoch_mismatch_is_rejected() {
    let mut h = Harness::new();
    h.service.identity.epoch = "ac000000-0000-4000-8000-000000ffffff".into();
    h.rejected(false, HostedError::IncompatibleIdentity);
    assert!(!h.cache().exists());
}
#[test]
fn resolved_schema_mismatch_is_rejected() {
    let mut h = Harness::new();
    h.service.identity.schema = "unsupported".into();
    h.rejected(false, HostedError::IncompatibleIdentity);
    assert!(!h.cache().exists());
}
#[test]
fn bootstrap_identity_mismatch_is_rejected_before_native_mutation() {
    let mut h = Harness::new();
    h.service.checkpoint.identity.epoch = "ac000000-0000-4000-8000-000000ffffff".into();
    h.rejected(false, HostedError::IncompatibleIdentity);
    assert!(!h.cache().exists());
}
#[test]
fn uninitialized_bootstrap_never_becomes_an_empty_document() {
    let mut h = Harness::new();
    h.service.checkpoint.initialized = false;
    h.rejected(false, HostedError::InvalidBootstrap);
    assert!(!h.cache().exists());
}
#[test]
fn corrupt_bootstrap_never_creates_cache() {
    let mut h = Harness::new();
    h.service.checkpoint.bytes = vec![255, 255];
    h.rejected(false, HostedError::InvalidBootstrap);
    assert!(!h.cache().exists());
}
#[test]
fn revoked_cached_open_preserves_bytes_and_can_reopen_offline() {
    let mut h = Harness::new();
    drop(h.open(false).unwrap());
    let bytes = h.bytes();
    h.service.failure = Some(ServiceError::Authorization);
    h.rejected(false, HostedError::Authorization);
    assert_eq!(h.bytes(), bytes);
    drop(h.open(true).unwrap());
    assert_eq!(h.service.bootstraps, 1);
}
#[test]
fn changed_remote_epoch_preserves_existing_cache() {
    let mut h = Harness::new();
    drop(h.open(false).unwrap());
    let bytes = h.bytes();
    h.service.identity.epoch = "ac000000-0000-4000-8000-000000ffffff".into();
    h.rejected(false, HostedError::IncompatibleIdentity);
    assert_eq!(h.bytes(), bytes);
    drop(h.open(true).unwrap());
}
#[test]
fn changed_remote_schema_preserves_existing_cache() {
    let mut h = Harness::new();
    drop(h.open(false).unwrap());
    let bytes = h.bytes();
    h.service.identity.schema = "unsupported".into();
    h.rejected(false, HostedError::IncompatibleIdentity);
    assert_eq!(h.bytes(), bytes);
}
#[test]
fn changed_config_epoch_cannot_hide_or_replace_old_cache() {
    let mut h = Harness::new();
    drop(h.open(false).unwrap());
    let bytes = h.bytes();
    let path = h.cache();
    let mut identity = h.identity.clone();
    identity.epoch = "ac000000-0000-4000-8000-000000ffffff".into();
    h.config = configuration(identity);
    assert_eq!(h.cache(), path);
    h.rejected(true, HostedError::IncompatibleIdentity);
    assert_eq!(h.bytes(), bytes);
}
#[test]
fn corrupt_existing_cache_is_not_rebootstrapped() {
    let mut h = Harness::new();
    drop(h.open(false).unwrap());
    fs::write(h.cache().join("latest.json"), b"corrupt").unwrap();
    fs::write(h.cache().join("previous.json"), b"corrupt").unwrap();
    let bytes = h.bytes();
    h.rejected(false, HostedError::CacheRecoveryRequired);
    assert_eq!(h.bytes(), bytes);
    assert_eq!(h.service.bootstraps, 1);
}
#[test]
fn interrupted_uninitialized_directory_is_recovery_only() {
    let mut h = Harness::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(h.cache()).unwrap();
    }
    h.rejected(false, HostedError::CacheRecoveryRequired);
    assert_eq!(h.service.bootstraps, 0);
}
#[test]
fn pending_unicode_update_and_separate_draft_survive_offline_restore_and_revocation() {
    let mut h = Harness::new();
    let mut session = h.open(false).unwrap();
    let doc = &mut session.recovery.document;
    let node = doc.snapshot().unwrap().visible[0].id.clone();
    let at = doc.anchor(&node, Utf16Offset(0), Bias::Right).unwrap();
    let plan = doc
        .prepare_text(
            &TextCommand::Insert {
                at,
                text: "Offline 🦀 ".into(),
            },
            &TextOrigin::new("hosted-fixture").unwrap(),
        )
        .unwrap();
    let update = doc.commit_text(plan).unwrap().update.unwrap();
    let id = session
        .storage
        .persist_update(doc, &update, 1, "hosted-fixture")
        .unwrap()
        .update_id;
    session
        .storage
        .persist_draft(&Draft {
            baseline: "original projection".into(),
            source: "uncommitted draft 😀".into(),
        })
        .unwrap();
    let snapshot = doc.snapshot().unwrap();
    drop(session);
    let bytes = h.bytes();
    let restored = h.open(true).unwrap();
    same_projection(restored.recovery.document.snapshot().unwrap(), snapshot);
    assert_eq!(restored.recovery.pending.len(), 1);
    assert_eq!(restored.recovery.pending[0].update_id, id);
    assert_eq!(restored.recovery.pending[0].bytes, update.bytes);
    assert_eq!(
        restored.recovery.draft.unwrap().source,
        "uncommitted draft 😀"
    );
    h.service.failure = Some(ServiceError::Authorization);
    h.rejected(false, HostedError::Authorization);
    assert_eq!(h.bytes(), bytes);
}
#[test]
fn tokens_are_redacted_and_absent_from_all_native_buffers() {
    let mut h = Harness::new();
    drop(h.open(false).unwrap());
    for (_, bytes) in tree(&h.root) {
        assert!(!String::from_utf8_lossy(&bytes).contains(TOKEN));
    }
    assert!(!format!("{:?}", Credential::new(TOKEN.into()).unwrap()).contains(TOKEN));
    for error in [
        HostedError::Authorization,
        HostedError::CredentialUnavailable,
        HostedError::ServiceUnavailable,
    ] {
        assert!(!error.to_string().contains(TOKEN));
    }
}
#[test]
fn arbitrary_web_pages_and_url_ambiguities_are_rejected_without_access() {
    let mut h = Harness::new();
    for url in [
        "http://app.example.test/ac/id",
        "https://other.test/ac/id",
        "https://user@host/ac/id",
        "https://app.example.test/",
        "https://app.example.test/ac/%2e",
        "https://app.example.test:443/ac/id",
    ] {
        assert!(matches!(
            hosted::open(
                url,
                &h.config,
                &h.root,
                false,
                &mut h.credentials,
                &mut h.service
            ),
            Err(HostedError::UnsupportedTarget)
        ));
    }
    for suffix in ["/", "?token=secret", "#fragment"] {
        assert_eq!(
            h.config.resolve_target(&(h.url.clone() + suffix)),
            Err(HostedError::UnsupportedTarget)
        );
    }
    assert_eq!(
        (
            h.credentials.calls,
            h.service.resolves,
            h.service.bootstraps
        ),
        (0, 0, 0)
    );
}
#[test]
fn configuration_requires_explicit_supported_identity_and_safe_endpoint() {
    let h = Harness::new();
    for endpoint in [
        "http://api.example.test",
        "https://user:secret@api.example.test",
        "https://api.example.test?token=x",
        "https://API.example.test",
        "https://api.example.test:443",
        "https://api.example.test/#x",
    ] {
        assert!(
            ServiceConfig::new(
                "app.example.test".into(),
                endpoint.into(),
                h.identity.clone()
            )
            .is_err()
        );
    }
    let mut identity = h.identity.clone();
    identity.schema = "unsupported".into();
    assert!(
        ServiceConfig::new(
            "app.example.test".into(),
            "https://api.example.test".into(),
            identity
        )
        .is_err()
    );
}
#[test]
fn cache_parent_must_be_private_and_is_not_created_implicitly() {
    let mut h = Harness::new();
    assert!(matches!(
        hosted::open(
            &h.url,
            &h.config,
            &h.root.join("missing"),
            false,
            &mut h.credentials,
            &mut h.service
        ),
        Err(HostedError::CacheUnavailable)
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&h.root, fs::Permissions::from_mode(0o755)).unwrap();
        h.rejected(false, HostedError::CacheUnavailable);
    }
    assert_eq!(h.credentials.calls, 0);
}
#[cfg(unix)]
#[test]
fn symlink_cache_is_refused_without_touching_destination() {
    let mut h = Harness::new();
    let destination = h.root.join("unrelated");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("sentinel"), b"preserve").unwrap();
    std::os::unix::fs::symlink(&destination, h.cache()).unwrap();
    h.rejected(true, HostedError::CacheRecoveryRequired);
    assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"preserve");
    assert_eq!(h.service.bootstraps, 0);
}
#[test]
fn credential_values_reject_empty_control_and_oversized_input() {
    for value in [String::new(), "secret\nheader".into(), "x".repeat(8193)] {
        assert!(Credential::new(value).is_err());
    }
}

fn same_projection(
    a: editac_cli::engine::projection::Snapshot,
    b: editac_cli::engine::projection::Snapshot,
) {
    assert_eq!(a.visible, b.visible);
    assert_eq!(a.recovery, b.recovery);
    assert_eq!(a.retained_structure, b.retained_structure);
}

#[test]
fn changed_config_schema_reports_incompatibility_without_replacement() {
    let mut h = Harness::new();
    drop(h.open(false).unwrap());
    let bytes = h.bytes();
    let path = h.cache();
    h.config = configuration(
        DocumentIdentity::v4(h.identity.document_id.clone(), h.identity.epoch.clone()).unwrap(),
    );
    assert_eq!(h.cache(), path);
    h.rejected(true, HostedError::IncompatibleIdentity);
    assert_eq!(h.bytes(), bytes);
}
#[test]
fn corrupt_latest_falls_back_to_previous_without_bootstrap() {
    let mut h = Harness::new();
    drop(h.open(false).unwrap());
    fs::write(h.cache().join("latest.json"), b"corrupt").unwrap();
    let bytes = h.bytes();
    let recovered = h.open(true).unwrap();
    assert_eq!(
        recovered.recovery.point,
        editac_cli::storage::RecoveryPoint::Previous
    );
    assert!(recovered.recovery.rejected_latest.is_some());
    same_projection(
        recovered.recovery.document.snapshot().unwrap(),
        h.server_projection.clone(),
    );
    assert_eq!(h.service.bootstraps, 1);
    assert_eq!(h.bytes(), bytes);
}
