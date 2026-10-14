//! CLI-1.8 canonical hosted acquisition/native draft/two-replica proof.
#[path = "support/hosted.rs"]
mod harness;
use editac_cli::{
    engine::{
        Utf16Offset,
        commands::{editor::EditorCommand, text::TextOrigin},
        document::{Document, ReplayUpdate},
    },
    hosted::{HostedSession, HostedSourceError},
    markdown::reconcile::ConflictKind,
    reconcile::{HostedBaseline, HostedConflict, HostedProjection, ReconcileError},
};
use harness::Harness;

struct Edits {
    host: Harness,
    session: HostedSession,
    peer: Document,
    base: HostedBaseline,
    ids: Vec<String>,
}
impl Edits {
    fn new(source: &str) -> Self {
        let mut host = Harness::with_source(source);
        let session = host.open(false).unwrap();
        let checkpoint = session.recovery.document.checkpoint();
        let mut peer = Document::restore(&checkpoint.identity, &checkpoint, &[]).unwrap();
        peer.enable_history(Default::default()).unwrap();
        let base = session.source_baseline().unwrap();
        let ids = owners(&session.recovery.document);
        assert_ne!(peer.writer_id(), session.recovery.document.writer_id());
        Self {
            host,
            session,
            peer,
            base,
            ids,
        }
    }
    fn commands(&mut self, commands: &[EditorCommand]) -> ReplayUpdate {
        let plan = self
            .peer
            .prepare_editor(commands, &TextOrigin::new("prepared-peer").unwrap())
            .unwrap();
        let update = self.peer.commit_editor(plan).unwrap().update;
        self.session.apply_received(&update).unwrap();
        update
    }
    fn replace(&mut self, index: usize, from: u32, to: u32, text: &str) -> ReplayUpdate {
        self.commands(&[EditorCommand::ReplaceAt {
            owner: self.ids[index].clone(),
            from: Utf16Offset(from),
            to: Utf16Offset(to),
            text: text.into(),
        }])
    }
    fn source(&self) -> String {
        HostedProjection::capture(&self.session.recovery.document)
            .unwrap()
            .source
    }
    fn commit(&mut self, source: &str) -> editac_cli::markdown::reconcile::SourceCommit {
        let plan = self.session.prepare_source(&self.base, source).unwrap();
        plan.commit(&mut self.session.recovery.document).unwrap()
    }
    fn conflict(&mut self, source: &str) -> HostedConflict {
        let before = self.session.recovery.document.checkpoint().bytes;
        let revision = self.session.recovery.document.revision();
        let history = self.session.recovery.document.history_state();
        let conflict = match self.session.prepare_source(&self.base, source) {
            Err(HostedSourceError::Conflict(c)) => c,
            _ => panic!("expected retained conflict"),
        };
        assert_eq!(conflict.base.source, self.base.opening().source);
        assert_eq!(conflict.local, source);
        assert_eq!(conflict.current_checkpoint.bytes, before);
        assert_eq!(conflict.current.as_ref().unwrap().checkpoint.bytes, before);
        assert_eq!(self.session.recovery.document.checkpoint().bytes, before);
        assert_eq!(self.session.recovery.document.revision(), revision);
        assert_eq!(self.session.recovery.document.history_state(), history);
        let draft = self.session.storage.read_draft().unwrap().unwrap();
        assert_eq!(draft.baseline, self.base.opening().source);
        assert_eq!(draft.source, source);
        conflict
    }
}
fn owners(d: &Document) -> Vec<String> {
    d.snapshot()
        .unwrap()
        .visible
        .iter()
        .filter(|n| n.kind == "block")
        .map(|n| n.id.clone())
        .collect()
}
fn kind(c: &HostedConflict) -> ConflictKind {
    match &c.error {
        ReconcileError::Shared(c) => c.kind,
        _ => panic!("expected shared conflict"),
    }
}

#[test]
fn opening_is_deterministic_bound_projection_not_source_reimport() {
    let e = Edits::new("# café 😀\n\nKeep **identities**.\n");
    assert_eq!(e.base.opening().source, e.source());
    assert_eq!(e.base.opening().checkpoint.identity, e.host.identity);
    assert!(e.base.opening().diagnostics.is_empty());
    assert_eq!(e.host.service.bootstraps, 1);
    assert_eq!(e.base.opening().snapshot.visible.len(), e.ids.len());
}
#[test]
fn disjoint_blocks_merge_and_keep_original_semantic_identities() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    e.replace(1, 0, 4, "remote");
    let result = e.commit("local\n\nbeta\n");
    assert!(result.commit.is_some());
    assert_eq!(e.source(), "local\n\nremote\n");
    assert_eq!(owners(&e.session.recovery.document), e.ids);
    assert_eq!(e.session.recovery.document.identity(), &e.host.identity);
    assert_eq!(e.session.recovery.document.history_state().undo_groups, 1);
    assert_eq!(e.host.service.bootstraps, 1);
}
#[test]
fn disjoint_ranges_in_same_block_use_stable_unicode_anchors() {
    let mut e = Edits::new("café 😀 and fin\n");
    e.replace(0, 12, 15, "remote");
    e.commit("local 😀 and fin\n");
    assert_eq!(e.source(), "local 😀 and remote\n");
    assert_eq!(owners(&e.session.recovery.document), e.ids);
}
#[test]
fn remote_prefix_shifts_local_utf16_target_without_replacing_remote_text() {
    let mut e = Edits::new("alpha omega\n");
    e.replace(0, 0, 0, "😀 ");
    e.commit("alpha LOCAL\n");
    assert_eq!(e.source(), "😀 alpha LOCAL\n");
}
#[test]
fn overlapping_remote_text_retains_exact_base_local_current_and_native_draft() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    e.replace(0, 0, 5, "remote");
    let c = e.conflict("local\n\nbeta\n");
    assert_eq!(kind(&c), ConflictKind::Overlap);
    assert_eq!(c.current.unwrap().source, "remote\n\nbeta\n");
}
#[test]
fn remotely_deleted_text_does_not_resurrect_from_stale_markdown() {
    let mut e = Edits::new("alpha omega\n");
    e.replace(0, 6, 11, "");
    let c = e.conflict("alpha LOCAL\n");
    assert!(matches!(
        kind(&c),
        ConflictKind::Overlap | ConflictKind::DeletedTarget
    ));
    assert_eq!(e.source(), "alpha \n");
}
#[test]
fn remotely_joined_owner_is_not_recreated_by_stale_source() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    e.commands(&[EditorCommand::Join {
        first: e.ids[0].clone(),
        second: e.ids[1].clone(),
    }]);
    let c = e.conflict("alpha\n\nLOCAL\n");
    assert!(matches!(
        kind(&c),
        ConflictKind::DeletedTarget | ConflictKind::MovedTarget
    ));
    assert!(owners(&e.session.recovery.document).len() < e.ids.len());
}
#[test]
fn ambiguous_added_block_is_retained_without_partial_first_block_mutation() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    let c = e.conflict("LOCAL\n\nbeta\n\nextra\n");
    assert_eq!(kind(&c), ConflictKind::AmbiguousMapping);
}
#[test]
fn unsupported_candidate_rejects_whole_batch() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    let c = e.conflict("LOCAL\n\n![image](https://example.test/image)\n");
    assert!(matches!(
        kind(&c),
        ConflictKind::UnsupportedContent | ConflictKind::AmbiguousMapping
    ));
    assert_eq!(e.source(), "alpha\n\nbeta\n");
}
#[test]
fn remote_mark_overlap_is_not_silently_erased() {
    use editac_cli::engine::commands::format::{FormatAction, Mark};
    let mut e = Edits::new("alpha\n");
    e.commands(&[EditorCommand::MarkAt {
        owner: e.ids[0].clone(),
        from: Utf16Offset(0),
        to: Utf16Offset(5),
        mark: Mark::Bold,
        action: FormatAction::Apply,
        href: None,
    }]);
    assert_eq!(kind(&e.conflict("local\n")), ConflictKind::Overlap);
    assert_eq!(e.source(), "**alpha**\n");
}
#[test]
fn stale_plan_after_receive_retains_all_three_representations() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    let plan = e
        .session
        .prepare_source(&e.base, "LOCAL\n\nbeta\n")
        .unwrap();
    e.replace(1, 0, 4, "remote");
    let before = e.session.recovery.document.checkpoint().bytes;
    let c = plan.commit(&mut e.session.recovery.document).err().unwrap();
    assert_eq!(kind(&c), ConflictKind::StalePlan);
    assert_eq!(c.local, "LOCAL\n\nbeta\n");
    assert_eq!(c.base.source, "alpha\n\nbeta\n");
    assert_eq!(c.current.unwrap().source, "alpha\n\nremote\n");
    assert_eq!(e.session.recovery.document.checkpoint().bytes, before);
    // An explicit fresh preparation against current anchors now proves safety.
    e.commit("LOCAL\n\nbeta\n");
    assert_eq!(e.source(), "LOCAL\n\nremote\n");
}
#[test]
fn no_op_after_remote_changes_preserves_bytes_revision_and_history() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    e.replace(1, 0, 4, "remote");
    let before = e.session.recovery.document.checkpoint().bytes;
    let revision = e.session.recovery.document.revision();
    assert!(e.commit("alpha\n\nbeta\n").commit.is_none());
    assert_eq!(e.session.recovery.document.checkpoint().bytes, before);
    assert_eq!(e.session.recovery.document.revision(), revision);
    assert_eq!(e.session.recovery.document.history_state().undo_groups, 0);
}
#[test]
fn local_undo_redo_preserves_received_remote_edit_and_epoch() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    e.replace(1, 0, 4, "remote");
    e.commit("LOCAL\n\nbeta\n");
    e.session.recovery.document.undo().unwrap().unwrap();
    assert_eq!(e.source(), "alpha\n\nremote\n");
    e.session.recovery.document.redo().unwrap().unwrap();
    assert_eq!(e.source(), "LOCAL\n\nremote\n");
    assert_eq!(e.session.recovery.document.identity(), &e.host.identity);
}
#[test]
fn subsequent_capture_and_receive_do_not_reset_prior_local_history() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    e.commit("LOCAL\n\nbeta\n");
    e.base = e.session.source_baseline().unwrap();
    e.replace(1, 0, 4, "remote");
    assert_eq!(e.session.recovery.document.history_state().undo_groups, 1);
    e.commit("LOCAL2\n\nbeta\n");
    assert_eq!(e.session.recovery.document.history_state().undo_groups, 2);
    assert_eq!(e.source(), "LOCAL2\n\nremote\n");
}
#[test]
fn wrong_document_receive_fails_without_mutation_or_bootstrap() {
    let mut e = Edits::new("alpha\n");
    let before = e.session.recovery.document.checkpoint().bytes;
    let mut other = Edits::new("other\n");
    let update = other.replace(0, 0, 5, "wrong");
    assert!(e.session.apply_received(&update).is_err());
    assert_eq!(e.session.recovery.document.checkpoint().bytes, before);
    assert_eq!(e.host.service.bootstraps, 1);
}
#[test]
fn corrupt_receive_is_atomic_and_does_not_discard_native_draft() {
    let mut e = Edits::new("alpha\n");
    e.session.prepare_source(&e.base, "local\n").unwrap();
    let before = e.session.recovery.document.checkpoint().bytes;
    let update = ReplayUpdate {
        identity: e.host.identity.clone(),
        bytes: vec![255],
    };
    assert!(e.session.apply_received(&update).is_err());
    assert_eq!(e.session.recovery.document.checkpoint().bytes, before);
    assert_eq!(
        e.session.storage.read_draft().unwrap().unwrap().source,
        "local\n"
    );
}
#[test]
fn duplicate_receive_does_not_reset_history_or_create_an_edit_group() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    let update = e.replace(1, 0, 4, "remote");
    e.commit("LOCAL\n\nbeta\n");
    let before = e.session.recovery.document.checkpoint().bytes;
    e.session.apply_received(&update).unwrap();
    assert_eq!(e.session.recovery.document.checkpoint().bytes, before);
    assert_eq!(e.session.recovery.document.history_state().undo_groups, 1);
}

// Prepared inert unknown tuple on the second replica, carried as structured
// update bytes. No source import, engine-root mutation or server occurs in CLI.
fn opaque_update(e: &Edits, index: usize) -> ReplayUpdate {
    use yrs::{Any, Doc, Map, ReadTxn, StateVector, Transact, updates::decoder::Decode};
    let checkpoint = e.peer.checkpoint();
    let raw = Doc::new();
    raw.transact_mut()
        .apply_update(yrs::Update::decode_v1(&checkpoint.bytes).unwrap())
        .unwrap();
    raw.get_or_insert_map("registry-block-attributes").insert(
        &mut raw.transact_mut(),
        e.ids[index].as_str(),
        Any::Array(
            vec![
                Any::from("vendor:unknown"),
                Any::from(1u32),
                Any::from("exact opaque café 😀 payload"),
            ]
            .into(),
        ),
    );
    ReplayUpdate {
        identity: checkpoint.identity,
        bytes: raw
            .transact()
            .encode_state_as_update_v1(&StateVector::default()),
    }
}
#[test]
fn unknown_remote_semantics_conflict_with_retained_current_checkpoint() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    e.session.apply_received(&opaque_update(&e, 1)).unwrap();
    let c = e.conflict("LOCAL\n\nchanged opaque\n");
    assert!(matches!(
        kind(&c),
        ConflictKind::UnsupportedContent | ConflictKind::Overlap
    ));
    assert!(!c.current.as_ref().unwrap().diagnostics.is_empty());
    let restored = Document::restore(&e.host.identity, &c.current_checkpoint, &[]).unwrap();
    assert_eq!(
        restored.snapshot().unwrap().visible[1].block_attributes,
        e.session.recovery.document.snapshot().unwrap().visible[1].block_attributes
    );
}
#[test]
fn unchanged_unknown_data_survives_safe_disjoint_source_edit() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    e.session.apply_received(&opaque_update(&e, 1)).unwrap();
    let before = e.session.recovery.document.snapshot().unwrap().visible[1].clone();
    e.commit("LOCAL\n\nbeta\n");
    assert_eq!(
        e.session.recovery.document.snapshot().unwrap().visible[1],
        before
    );
    assert_eq!(owners(&e.session.recovery.document), e.ids);
}
#[test]
fn conflict_draft_and_current_state_recover_offline_without_service_calls() {
    let mut e = Edits::new("alpha\n\nbeta\n");
    let update = e.replace(0, 0, 5, "remote");
    // The existing native journal is conservative pending storage, not a
    // transport receipt or remote durability acknowledgment. CLI-1.9 owns sync.
    e.session
        .storage
        .persist_update(&e.session.recovery.document, &update, 1, "prepared-receive")
        .unwrap();
    e.conflict("local\n\nbeta\n");
    e.session
        .storage
        .publish_checkpoint(&e.session.recovery.document, &[])
        .unwrap();
    let calls = (
        e.host.credentials.calls,
        e.host.service.resolves,
        e.host.service.bootstraps,
    );
    let offline = e.host.open(true).unwrap();
    assert_eq!(
        HostedProjection::capture(&offline.recovery.document)
            .unwrap()
            .source,
        "remote\n\nbeta\n"
    );
    let draft = offline.recovery.draft.unwrap();
    assert_eq!(draft.baseline, "alpha\n\nbeta\n");
    assert_eq!(draft.source, "local\n\nbeta\n");
    assert_eq!(
        (
            e.host.credentials.calls,
            e.host.service.resolves,
            e.host.service.bootstraps
        ),
        calls
    );
    assert_ne!(
        offline.recovery.document.writer_id(),
        e.session.recovery.document.writer_id()
    );
}
#[test]
fn native_draft_failure_prevents_preparation_and_semantic_mutation() {
    let e = Edits::new("alpha\n");
    let before = e.session.recovery.document.checkpoint().bytes;
    // Occupy the draft publication path with a directory deterministically.
    std::fs::create_dir(e.session.storage.root().join("draft.json")).unwrap();
    assert!(matches!(
        e.session.prepare_source(&e.base, "local\n"),
        Err(HostedSourceError::Draft(_))
    ));
    assert_eq!(e.session.recovery.document.checkpoint().bytes, before);
}
