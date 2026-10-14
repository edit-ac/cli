//! Canonical CLI-1.4: real local resolver/editor -> shared parser/prepared engine.
use editac_cli::{
    editor::{self, ChangedBuffer, EditorCommand, EditorOutcome},
    engine::document::{CreationPermit, Document, DocumentIdentity},
    markdown::reconcile::ConflictKind,
    reconcile::{LocalBaseline, ReconcileError, RejectedEdit},
    target::{LocalTarget, ResolveOptions, resolve},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

fn identity() -> DocumentIdentity {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    DocumentIdentity::v5(
        &format!("77777777-7777-4777-8777-{id:012x}"),
        "88888888-8888-4888-8888-888888888888",
    )
    .unwrap()
}

struct Fixture {
    root: PathBuf,
    target: LocalTarget,
}
impl Fixture {
    fn new(source: &str, extension: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "editac-reconcile-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let path = root.join(format!("source.{extension}"));
        fs::write(&path, source).unwrap();
        Self {
            root,
            target: resolve(path, ResolveOptions::default()).unwrap(),
        }
    }
    fn open(&self) -> (Document, LocalBaseline) {
        let identity = identity();
        LocalBaseline::import(
            self.target.clone(),
            CreationPermit::authorize_new(identity).unwrap(),
        )
        .unwrap()
    }
    fn edit(&self, source: &str) -> ChangedBuffer {
        fs::write(self.root.join("draft"), source).unwrap();
        let executable =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/editor/fake editor.sh");
        let command = EditorCommand::parse(&format!(
            "'{}' changed '{}'",
            executable.display(),
            self.root.join("draft").display()
        ))
        .unwrap();
        match editor::run(&self.target, &command, &self.root).unwrap() {
            EditorOutcome::Changed(buffer) => buffer,
            _ => panic!("fixture must actually change the buffer"),
        }
    }
    fn preserved(&self, buffer: &ChangedBuffer, draft: &str) {
        assert_eq!(
            fs::read_to_string(self.target.resolved_path()).unwrap(),
            self.target.source()
        );
        assert_eq!(
            fs::read_to_string(buffer.recovery().baseline()).unwrap(),
            self.target.source()
        );
        assert_eq!(
            fs::read_to_string(buffer.recovery().buffer()).unwrap(),
            draft
        );
        assert_eq!(buffer.source(), draft);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
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
fn texts(d: &Document) -> Vec<String> {
    d.snapshot()
        .unwrap()
        .visible
        .iter()
        .filter(|n| n.kind == "block")
        .map(|n| n.runs.iter().map(|r| r.text.as_str()).collect())
        .collect()
}
fn unchanged(d: &Document, bytes: &[u8], vector: &[u8], groups: usize) {
    assert_eq!(d.checkpoint().bytes, bytes);
    assert_eq!(d.state_vector_v1(), vector);
    assert_eq!(d.history_state().undo_groups, groups);
}
fn kind(error: &RejectedEdit) -> ConflictKind {
    match &error.error {
        ReconcileError::Shared(c) => c.kind,
        e => panic!("{e:?}"),
    }
}

#[test]
fn changed_markdown_uses_one_history_operation_and_preserves_untouched_owners() {
    let f = Fixture::new("before **bold** after\n\nuntouched 😀\n\nend\n", "md");
    let (mut d, base) = f.open();
    let ids = owners(&d);
    let before = d.snapshot().unwrap();
    let bytes = d.checkpoint().bytes;
    let vector = d.state_vector_v1();
    let draft = "before **brave** after\n\nuntouched 😀\n\nlast é😀\n";
    let plan = base.prepare(&d, f.edit(draft)).unwrap();
    assert_eq!(plan.changed_blocks(), 2);
    unchanged(&d, &bytes, &vector, 0);
    let result = plan.commit(&mut d).unwrap();
    assert!(!result.commit.unwrap().update.bytes.is_empty());
    assert_eq!(d.history_state().undo_groups, 1);
    assert_eq!(owners(&d), ids);
    assert_eq!(
        before.visible.iter().find(|n| n.id == ids[1]),
        d.snapshot()
            .unwrap()
            .visible
            .iter()
            .find(|n| n.id == ids[1])
    );
    f.preserved(&result.buffer, draft);
    assert_eq!(base.source(), f.target.source());
    d.undo().unwrap().unwrap();
    assert_eq!(texts(&d), ["before bold after", "untouched 😀", "end"]);
    d.redo().unwrap().unwrap();
    assert_eq!(
        texts(&d),
        ["before brave after", "untouched 😀", "last é😀"]
    );
}

#[test]
fn spelling_only_preserves_checkpoint_and_exact_crlf_source() {
    let f = Fixture::new("*é😀*\r\n\r\nsecond\r\n", "md");
    let (mut d, base) = f.open();
    let bytes = d.checkpoint().bytes;
    let vector = d.state_vector_v1();
    let draft = "_é😀_\r\n\r\nsecond\r\n";
    let plan = base.prepare(&d, f.edit(draft)).unwrap();
    assert_eq!(plan.changed_blocks(), 0);
    let result = plan.commit(&mut d).unwrap();
    assert!(result.commit.is_none());
    unchanged(&d, &bytes, &vector, 0);
    f.preserved(&result.buffer, draft);
}

#[test]
fn mark_only_edit_retains_text_owners_and_is_one_undoable_operation() {
    let f = Fixture::new("plain é😀\n\nuntouched\n", "md");
    let (mut d, base) = f.open();
    let ids = owners(&d);
    let before = d.snapshot().unwrap();
    let draft = "**plain é😀**\n\nuntouched\n";
    let result = base
        .prepare(&d, f.edit(draft))
        .unwrap()
        .commit(&mut d)
        .unwrap();
    assert!(result.commit.is_some());
    assert_eq!(owners(&d), ids);
    assert_eq!(texts(&d), ["plain é😀", "untouched"]);
    assert_eq!(d.history_state().undo_groups, 1);
    let after = d.snapshot().unwrap();
    let first = after.visible.iter().find(|n| n.id == ids[0]).unwrap();
    assert!(
        first
            .runs
            .iter()
            .all(|r| r.attributes.get("bold") == Some(&yrs::Any::Bool(true)))
    );
    assert_eq!(
        before.visible.iter().find(|n| n.id == ids[1]),
        after.visible.iter().find(|n| n.id == ids[1])
    );
    f.preserved(&result.buffer, draft);
    d.undo().unwrap().unwrap();
    assert_eq!(d.snapshot().unwrap().visible, before.visible);
    d.redo().unwrap().unwrap();
    assert_eq!(d.snapshot().unwrap().visible, after.visible);
}

#[test]
fn edited_lossy_source_rejects_all_changes_and_retains_exact_draft() {
    let f = Fixture::new("known\n\nsecond\n", "md");
    let (d, base) = f.open();
    let bytes = d.checkpoint().bytes;
    let vector = d.state_vector_v1();
    let draft = "changed\n\n![alt](https://example.com/image.png)\n";
    let error = base.prepare(&d, f.edit(draft)).err().unwrap();
    assert_eq!(kind(&error), ConflictKind::UnsupportedContent);
    unchanged(&d, &bytes, &vector, 0);
    f.preserved(&error.buffer, draft);
}

#[test]
fn literal_text_does_not_parse_markdown_and_retains_unicode_and_line_endings() {
    let f = Fixture::new("# literal\r\n*é😀*\r\n", "txt");
    let (mut d, base) = f.open();
    let ids = owners(&d);
    let draft = "# literal\r\n*é🦀 e\u{301} אב*\r\n";
    let result = base
        .prepare(&d, f.edit(draft))
        .unwrap()
        .commit(&mut d)
        .unwrap();
    assert_eq!(texts(&d), [draft]);
    assert_eq!(owners(&d), ids);
    assert!(
        d.snapshot()
            .unwrap()
            .visible
            .iter()
            .all(|n| n.runs.iter().all(|r| r.attributes.is_empty()))
    );
    assert_eq!(d.history_state().undo_groups, 1);
    f.preserved(&result.buffer, draft);
}

#[test]
fn heading_and_task_check_state_use_prepared_semantic_commands() {
    let f = Fixture::new("# title\n\n- [ ] todo\n", "md");
    let (mut d, base) = f.open();
    let ids = owners(&d);
    let result = base
        .prepare(&d, f.edit("## title\n\n- [x] done\n"))
        .unwrap()
        .commit(&mut d)
        .unwrap();
    assert!(result.commit.is_some());
    assert_eq!(owners(&d), ids);
    assert_eq!(d.history_state().undo_groups, 1);
    let snap = d.snapshot().unwrap();
    assert_eq!(
        snap.visible
            .iter()
            .find(|n| n.id == ids[0])
            .unwrap()
            .block_attributes,
        Some(editac_cli::engine::document::BlockAttributes::Heading(2))
    );
    assert_eq!(
        snap.visible
            .iter()
            .find(|n| n.id == ids[1])
            .unwrap()
            .editor
            .as_ref()
            .unwrap()
            .checked,
        Some(true)
    );
}

#[test]
fn whole_document_list_fence_and_reference_context_survives() {
    let f = Fixture::new(
        "- first\n- second\n\n```rust\nold\n```\n\n[link][r]\n\n[r]: https://example.com\n",
        "md",
    );
    let (mut d, base) = f.open();
    let ids = owners(&d);
    let draft = f
        .target
        .source()
        .replace("old", "new")
        .replace("first", "changed");
    let result = base
        .prepare(&d, f.edit(&draft))
        .unwrap()
        .commit(&mut d)
        .unwrap();
    assert!(result.commit.is_some());
    assert_eq!(owners(&d), ids);
    assert_eq!(d.history_state().undo_groups, 1);
    f.preserved(&result.buffer, &draft);
}

#[test]
fn structural_cardinality_changes_reject_without_mutation() {
    let f = Fixture::new("one\n", "md");
    let (d, base) = f.open();
    let bytes = d.checkpoint().bytes;
    let vector = d.state_vector_v1();
    let draft = "one\n\ntwo\n";
    let error = base.prepare(&d, f.edit(draft)).err().unwrap();
    assert_eq!(kind(&error), ConflictKind::AmbiguousMapping);
    unchanged(&d, &bytes, &vector, 0);
    f.preserved(&error.buffer, draft);
}

#[test]
fn lossy_opening_source_is_rejected_before_returning_a_replica() {
    let f = Fixture::new("known\n\n![alt](https://example.com/image.png)\n", "md");
    // Opening lossy source must not silently become a fresh editable replacement.
    let identity = identity();
    assert!(matches!(
        LocalBaseline::import(
            f.target.clone(),
            CreationPermit::authorize_new(identity).unwrap()
        ),
        Err(ReconcileError::UnsupportedOpening)
    ));
}

// Inject a prepared inert unknown attribute into a checkpoint, not production
// schema machinery. Uses the same pinned Yrs as the engine; never a service.
fn opaque(d: Document, id: &str) -> Document {
    use yrs::{Any, Doc, Map, ReadTxn, StateVector, Transact, updates::decoder::Decode};
    let mut checkpoint = d.checkpoint();
    let raw = Doc::new();
    raw.transact_mut()
        .apply_update(yrs::Update::decode_v1(&checkpoint.bytes).unwrap())
        .unwrap();
    raw.get_or_insert_map("registry-block-attributes").insert(
        &mut raw.transact_mut(),
        id,
        Any::Array(
            vec![
                Any::from("vendor:unknown"),
                Any::from(1u32),
                Any::from("exact opaque payload"),
            ]
            .into(),
        ),
    );
    checkpoint.bytes = raw
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let mut restored = Document::restore(&checkpoint.identity, &checkpoint, &[]).unwrap();
    restored.enable_history(Default::default()).unwrap();
    restored
}

#[test]
fn opaque_data_survives_unrelated_edit_exactly() {
    let f = Fixture::new("known\n\nopaque\n", "md");
    let (d, _) = f.open();
    let ids = owners(&d);
    let mut d = opaque(d, &ids[1]);
    let base = LocalBaseline::capture(&d, f.target.clone(), &ids).unwrap();
    let before = d.snapshot().unwrap();
    let payload = opaque_payload(&d, &ids[1]);
    let result = base
        .prepare(&d, f.edit("changed\n\nopaque\n"))
        .unwrap()
        .commit(&mut d)
        .unwrap();
    assert!(result.commit.is_some());
    assert_eq!(
        before.visible.iter().find(|n| n.id == ids[1]),
        d.snapshot()
            .unwrap()
            .visible
            .iter()
            .find(|n| n.id == ids[1])
    );
    assert_eq!(opaque_payload(&d, &ids[1]), payload);
    assert_eq!(owners(&d), ids);
    assert_eq!(d.history_state().undo_groups, 1);
}

#[test]
fn opaque_second_delta_rejects_whole_batch_and_retains_draft() {
    let f = Fixture::new("known\n\nopaque\n", "md");
    let (d, _) = f.open();
    let ids = owners(&d);
    let d = opaque(d, &ids[1]);
    let base = LocalBaseline::capture(&d, f.target.clone(), &ids).unwrap();
    let bytes = d.checkpoint().bytes;
    let vector = d.state_vector_v1();
    let draft = "changed\n\nchanged opaque\n";
    let error = base.prepare(&d, f.edit(draft)).err().unwrap();
    assert_eq!(kind(&error), ConflictKind::UnsupportedContent);
    unchanged(&d, &bytes, &vector, 0);
    f.preserved(&error.buffer, draft);
}

#[test]
fn duplicate_or_reordered_owner_bindings_are_rejected() {
    let f = Fixture::new("one\n\ntwo\n", "md");
    let (d, _) = f.open();
    let ids = owners(&d);
    for bad in [
        vec![ids[0].clone(), ids[0].clone()],
        vec![ids[1].clone(), ids[0].clone()],
        vec![],
    ] {
        assert!(
            matches!(LocalBaseline::capture(&d, f.target.clone(), &bad), Err(ReconcileError::Shared(c)) if c.kind == ConflictKind::AmbiguousMapping)
        );
    }
}

#[test]
fn different_opening_target_cannot_supply_a_draft() {
    let f = Fixture::new("one\n", "md");
    let other = Fixture::new("one\n", "md");
    let (d, base) = f.open();
    let bytes = d.checkpoint().bytes;
    let vector = d.state_vector_v1();
    let error = base.prepare(&d, other.edit("changed\n")).err().unwrap();
    assert!(matches!(error.error, ReconcileError::TargetMismatch));
    unchanged(&d, &bytes, &vector, 0);
    other.preserved(&error.buffer, "changed\n");
}

#[test]
fn stale_prepared_commit_keeps_current_replica_and_recoverable_source() {
    let f = Fixture::new("one\n", "md");
    let (mut d, base) = f.open();
    let stale = base.prepare(&d, f.edit("stale\n")).unwrap();
    base.prepare(&d, f.edit("current\n"))
        .unwrap()
        .commit(&mut d)
        .unwrap();
    let bytes = d.checkpoint().bytes;
    let vector = d.state_vector_v1();
    let error = stale.commit(&mut d).err().unwrap();
    assert_eq!(kind(&error), ConflictKind::StalePlan);
    unchanged(&d, &bytes, &vector, 1);
    f.preserved(&error.buffer, "stale\n");
}

#[test]
fn wrong_document_identity_rejects_without_mutation() {
    let f = Fixture::new("one\n", "md");
    let (d, base) = f.open();
    let identity = identity();
    let (mut other, _) = LocalBaseline::import(
        f.target.clone(),
        CreationPermit::authorize_new(identity).unwrap(),
    )
    .unwrap();
    let plan = base.prepare(&d, f.edit("changed\n")).unwrap();
    let bytes = other.checkpoint().bytes;
    let vector = other.state_vector_v1();
    let error = plan.commit(&mut other).err().unwrap();
    assert_eq!(kind(&error), ConflictKind::IdentityMismatch);
    unchanged(&other, &bytes, &vector, 0);
    f.preserved(&error.buffer, "changed\n");
}

#[test]
fn empty_markdown_and_literal_open_without_syntax_or_identity_invention() {
    for ext in ["md", "txt"] {
        let f = Fixture::new("", ext);
        let (mut d, base) = f.open();
        let ids = owners(&d);
        assert_eq!(ids.len(), 1);
        assert_eq!(base.source(), "");
        base.prepare(&d, f.edit("é😀"))
            .unwrap()
            .commit(&mut d)
            .unwrap();
        assert_eq!(texts(&d), ["é😀"]);
        assert_eq!(owners(&d), ids);
    }
}

#[test]
fn editor_exact_noop_never_advances_to_preparation() {
    let f = Fixture::new("*same*\n", "md");
    let (d, base) = f.open();
    let bytes = d.checkpoint().bytes;
    let vector = d.state_vector_v1();
    let executable =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/editor/fake editor.sh");
    let command =
        EditorCommand::parse(&format!("'{}' noop ignored", executable.display())).unwrap();
    assert!(matches!(
        editor::run(&f.target, &command, &f.root).unwrap(),
        EditorOutcome::Unchanged(_)
    ));
    unchanged(&d, &bytes, &vector, 0);
    assert_eq!(base.source(), "*same*\n");
}

#[test]
fn same_count_container_change_rejects_without_partial_mutation() {
    let f = Fixture::new("first\n\nsecond\n", "md");
    let (d, base) = f.open();
    let bytes = d.checkpoint().bytes;
    let vector = d.state_vector_v1();
    let draft = "changed\n\n- second\n";
    let error = base.prepare(&d, f.edit(draft)).err().unwrap();
    assert_eq!(kind(&error), ConflictKind::UnsupportedContent);
    unchanged(&d, &bytes, &vector, 0);
    f.preserved(&error.buffer, draft);
}

#[test]
fn opening_resource_limit_does_not_return_a_partial_session_or_touch_source() {
    let source = (0..129)
        .map(|i| format!("paragraph {i}\n\n"))
        .collect::<String>();
    let f = Fixture::new(&source, "md");
    let identity = identity();
    assert!(
        matches!(LocalBaseline::import(f.target.clone(), CreationPermit::authorize_new(identity).unwrap()), Err(ReconcileError::Shared(c)) if c.kind == ConflictKind::ResourceLimit)
    );
    assert_eq!(
        fs::read_to_string(f.target.resolved_path()).unwrap(),
        source
    );
}

fn opaque_payload(d: &Document, id: &str) -> yrs::Any {
    use yrs::{Doc, Map, ReadTxn, Transact, updates::decoder::Decode};
    let raw = Doc::new();
    raw.transact_mut()
        .apply_update(yrs::Update::decode_v1(&d.checkpoint().bytes).unwrap())
        .unwrap();
    let tx = raw.transact();
    match tx
        .get_map("registry-block-attributes")
        .unwrap()
        .get(&tx, id)
        .unwrap()
    {
        yrs::Out::Any(value) => value,
        _ => panic!("fixture attribute must be inert data"),
    }
}
