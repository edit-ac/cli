//! TUI-1.1 entry evidence only: this does not qualify a terminal presenter.
#[path = "support/hosted.rs"]
mod harness;

use editac_cli::{
    engine::{
        commands::text::TextOrigin,
        document::{CreationPermit, DocumentIdentity},
    },
    markdown::{
        optional::Codecs,
        reconcile::{SourceBaseline, SourceMode},
    },
    reconcile::LocalBaseline,
    status,
    target::{ResolveOptions, resolve},
};

const LOCAL: &str = include_str!("fixtures/tui/local.md");
const HOSTED: &str = include_str!("fixtures/tui/hosted.md");

#[test]
fn local_exact_source_has_inert_preparation_and_one_history_group() {
    let h = harness::Harness::with_source(LOCAL);
    let path = h.root.join("server.md");
    let target = resolve(&path, ResolveOptions::default()).unwrap();
    let (mut document, local) = LocalBaseline::import(
        target,
        CreationPermit::authorize_new(
            DocumentIdentity::v5(
                h.identity.document_id.replace("ac000000", "bc000000"),
                "ac000000-0000-4000-8000-000000000000",
            )
            .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(local.source(), LOCAL);
    let owners: Vec<_> = document
        .snapshot()
        .unwrap()
        .visible
        .iter()
        .filter(|node| node.kind == "block")
        .map(|node| node.id.clone())
        .collect();
    let baseline = SourceBaseline::capture(
        &document,
        LOCAL,
        SourceMode::Markdown(Codecs::default()),
        &owners,
    )
    .unwrap();
    let opening = document.checkpoint().bytes;
    let revision = document.revision();
    let origin = TextOrigin::new("tui-entry:prepared").unwrap();
    let unchanged = baseline.prepare(&document, LOCAL, &origin).unwrap();
    assert!(unchanged.commit(&mut document).unwrap().commit.is_none());
    assert_eq!(document.revision(), revision);
    let edited = LOCAL.replace("local", "edited");
    let plan = baseline.prepare(&document, &edited, &origin).unwrap();
    assert_eq!(document.checkpoint().bytes, opening);
    assert!(plan.commit(&mut document).unwrap().commit.is_some());
    assert_eq!(document.history_state().undo_groups, 1);
    document.undo().unwrap().unwrap();
    assert_eq!(
        document
            .snapshot()
            .unwrap()
            .visible
            .iter()
            .filter(|node| node.kind == "block")
            .map(|node| node.id.clone())
            .collect::<Vec<_>>(),
        owners
    );
    // A semantic commit/history operation is not a file save.
    assert_eq!(std::fs::read_to_string(path).unwrap(), LOCAL);
}

#[test]
fn cached_hosted_fixture_restores_without_auth_and_separates_draft_from_save() {
    let mut h = harness::Harness::with_source(HOSTED);
    drop(h.open(false).unwrap());
    let calls = (
        h.credentials.calls,
        h.service.resolves,
        h.service.bootstraps,
    );
    let mut session = h.open(true).unwrap();
    assert_eq!(
        (
            h.credentials.calls,
            h.service.resolves,
            h.service.bootstraps
        ),
        calls
    );
    assert!(!session.authorized_online);
    let before = status::inspect(&session).unwrap();
    assert!(before.locally_saved);
    assert_eq!(before.remote_durability, "unknown");
    let baseline = session.source_baseline().unwrap();
    assert_eq!(baseline.opening().checkpoint.identity, h.identity);
    let current = baseline.opening().source.clone();
    let candidate = current.replace("hosted", "draft");
    let revision = session.recovery.document.revision();
    let plan = session.prepare_source(&baseline, &candidate).unwrap();
    assert_eq!(session.recovery.document.revision(), revision);
    let draft = status::inspect(&session).unwrap();
    assert!(draft.unresolved_draft);
    assert_eq!(draft.pending_upload, 0);
    let committed = plan.commit(&mut session.recovery.document).unwrap();
    assert!(committed.commit.is_some());
    assert!(!status::inspect(&session).unwrap().locally_saved);
    // Deliberately stop before native commit acknowledgment: restart retains draft,
    // not an invented save/receipt for the unpersisted semantic edit.
    drop(session);
    let restored = h.open(true).unwrap();
    assert_eq!(
        restored.source_baseline().unwrap().opening().source,
        current
    );
    assert_eq!(restored.recovery.draft.as_ref().unwrap().source, candidate);
    assert_eq!(
        status::inspect(&restored).unwrap().remote_durability,
        "unknown"
    );
}
