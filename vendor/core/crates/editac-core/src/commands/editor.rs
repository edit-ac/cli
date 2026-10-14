//! Shared v4 primitives. Higher-level list/block/move/history policies have
//! separate acceptance gates; all plans here stage the entire batch first.
use super::text::{TextError, TextOrigin};
use crate::{
    document::{Document, DocumentError, DocumentIdentity, MAX_RESTORE_BYTES, ReplayUpdate},
    editor_mapping as m,
};
use std::collections::VecDeque;
use yrs::{Any, Map, ReadTxn, Transact, XmlTextPrelim};

/// A synchronous UTF-16 cut in the disposable batch state, not a live anchor.
#[derive(Clone, Debug)]
pub struct EditorPosition {
    pub owner: String,
    pub offset: crate::Utf16Offset,
    pub bias: crate::anchors::Bias,
}

#[derive(Clone, Debug)]
pub enum EditorCommand {
    /// Resolve the range after all preceding commands on the disposable replica.
    ReplaceAt {
        owner: String,
        from: crate::Utf16Offset,
        to: crate::Utf16Offset,
        text: String,
    },
    MarkAt {
        owner: String,
        from: crate::Utf16Offset,
        to: crate::Utf16Offset,
        mark: super::format::Mark,
        action: super::format::FormatAction,
        href: Option<String>,
    },
    /// Optional first command: capture the actual transaction-entry bookmark.
    HistorySelectionAt {
        anchor: EditorPosition,
        focus: EditorPosition,
    },
    /// Must be the last command. Both cuts are validated before live mutation.
    SelectionAt {
        anchor: EditorPosition,
        focus: EditorPosition,
    },
    /// Higher-level list policy expands only on the disposable prepared replica.
    List(super::lists::ListCommand),
    BlockCommand(super::blocks::BlockCommand),
    Move(super::r#move::MoveCommand),
    Text(super::text::TextCommand),
    Split {
        owner: String,
        at: crate::anchors::Anchor,
    },
    /// Nonempty heading-end Enter. Destination presentation is staged with the split.
    HeadingEnd {
        owner: String,
        at: crate::anchors::Anchor,
    },
    Join {
        first: String,
        second: String,
    },
    Create {
        id: String,
        kind: String,
        text: String,
        tuple: Any,
    },
    Place {
        target: String,
        parent: String,
        before: Option<String>,
    },
    Block {
        target: String,
        tuple: Any,
    },
    /// Registered inert optional content; retains v1 data in an opaque tuple.
    Content {
        target: String,
        content: crate::modules::content::Content,
    },
    /// Compare the full retained descriptor before editing an opaque table atom.
    TableCell {
        target: String,
        expected: Any,
        row: usize,
        column: usize,
        text: String,
    },
    Container {
        target: String,
        tuple: Any,
    },
    Check {
        target: String,
        checked: bool,
    },
    Enable {
        target: String,
        enabled: bool,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorError {
    UnsupportedProfile,
    UnsupportedTarget,
    /// Current v4 replay would invalidate existing ownership after metadata change.
    PresentationConflict,
    InvalidCommand,
    ResourceLimit,
    Move(super::r#move::MoveRejection),
    Text(TextError),
    Format(super::format::FormatError),
    Admission(DocumentError),
}
pub struct PreparedEditorEdit {
    identity: DocumentIdentity,
    writer: u32,
    revision: u64,
    baseline: Vec<u8>,
    update: Vec<u8>,
    origin: TextOrigin,
    next_operation: u32,
    result_caret: Option<crate::anchors::Anchor>,
    result_selection: Option<crate::anchors::Selection>,
    entry_selection: Option<crate::anchors::Selection>,
}
impl PreparedEditorEdit {
    pub fn result_caret(&self) -> Option<&crate::anchors::Anchor> {
        self.result_caret.as_ref()
    }
    pub fn result_selection(&self) -> Option<&crate::anchors::Selection> {
        self.result_selection.as_ref()
    }
}
pub struct EditorCommit {
    pub result_caret: Option<crate::anchors::Anchor>,
    pub result_selection: Option<crate::anchors::Selection>,
    pub revision: u64,
    pub origin: TextOrigin,
    pub update: ReplayUpdate,
}

/// Portable clipboard data, without live owner IDs or executable content.
#[derive(Clone, Debug)]
pub struct FragmentRun {
    pub text: String,
    pub marks: Vec<(super::format::Mark, Option<String>)>,
}
#[derive(Clone, Debug)]
pub struct FragmentBlock {
    pub runs: Vec<FragmentRun>,
    pub attributes: Option<crate::document::BlockAttributes>,
    pub containers: Vec<(String, super::lists::ContainerStyle)>,
    pub checked: Option<bool>,
}

impl Document {
    /// Stage the complete transfer on an isolated replica. Only the final admitted
    /// update reaches the live document, history, subscribers and persistence.
    pub fn prepare_fragment(
        &self,
        selection: &crate::anchors::Selection,
        blocks: &[FragmentBlock],
        origin: &TextOrigin,
    ) -> Result<PreparedEditorEdit, EditorError> {
        use super::{
            format::{BlockFormatCommand, FormatAction, FormatCommand},
            text::TextCommand,
        };
        use crate::anchors::{Bias, Resolution, Selection};
        use crate::{Utf16Offset, document::BlockAttributes};
        // Edge atoms need editable cuts around them. Otherwise assigning an
        // opaque tuple to the first/last destination could absorb unrelated
        // prefix/suffix text into an atom's fallback. Padding stays staged and
        // preserves both original source identities and one-update history.
        let atom = |block: &FragmentBlock| {
            matches!(
                block.attributes,
                Some(BlockAttributes::Unknown(_) | BlockAttributes::Separator)
            )
        };
        let has_atoms = blocks.iter().any(atom);
        let mut padded;
        let empty = || FragmentBlock {
            runs: vec![FragmentRun {
                text: String::new(),
                marks: vec![],
            }],
            attributes: None,
            containers: vec![],
            checked: None,
        };
        let blocks = if blocks.first().is_some_and(atom) || blocks.last().is_some_and(atom) {
            padded = Vec::with_capacity(blocks.len() + 2);
            if blocks.first().is_some_and(atom) {
                padded.push(empty());
            }
            padded.extend_from_slice(blocks);
            if blocks.last().is_some_and(atom) {
                padded.push(empty());
            }
            padded.as_slice()
        } else {
            blocks
        };
        if blocks.is_empty()
            || blocks.len() > 128
            || blocks.iter().map(|b| b.runs.len()).sum::<usize>() > 1024
            || blocks
                .iter()
                .flat_map(|b| &b.runs)
                .map(|r| r.text.len())
                .sum::<usize>()
                > 1048576
        {
            return Err(EditorError::ResourceLimit);
        }
        if blocks.iter().any(|b| {
            b.containers.len() > 16
                || b.containers
                    .iter()
                    .any(|(key, _)| key.is_empty() || key.len() > 128)
                || b.runs.iter().any(|r| r.marks.len() > 6)
        }) {
            return Err(EditorError::ResourceLimit);
        }
        for block in blocks {
            if let Some(BlockAttributes::Unknown(tuple)) = &block.attributes {
                if let Ok(Some(content)) = crate::modules::content::Content::from_tuple(tuple) {
                    let fallback = match &content {
                        crate::modules::content::Content::Reference { label, .. } => {
                            Some(label.as_str())
                        }
                        _ => content.inert_source(),
                    };
                    if let Some(fallback) = fallback {
                        if block
                            .runs
                            .iter()
                            .map(|r| r.text.as_str())
                            .collect::<String>()
                            != fallback
                            || block.runs.iter().any(|r| !r.marks.is_empty())
                        {
                            return Err(EditorError::InvalidCommand);
                        }
                    }
                }
            }
        }
        // A literal single-block replacement needs no structural cuts or new
        // source owners. Reuse the prepared text path, retaining admitted
        // destination character attributes and presentation.
        if blocks.len() == 1
            && blocks[0].attributes.is_none()
            && blocks[0].containers.is_empty()
            && blocks[0].checked.is_none()
            && blocks[0].runs.iter().all(|run| run.marks.is_empty())
        {
            if let (Ok(Resolution::Position(a)), Ok(Resolution::Position(b))) = (
                self.resolve_anchor(&selection.anchor),
                self.resolve_anchor(&selection.focus),
            ) {
                if a.node == b.node
                    && self
                        .inspect()
                        .map_err(|_| EditorError::InvalidCommand)?
                        .visible
                        .iter()
                        .any(|node| {
                            node.id == a.node
                                && matches!(
                                    node.block_attributes,
                                    Some(
                                        BlockAttributes::Paragraph
                                            | BlockAttributes::Heading(_)
                                            | BlockAttributes::Code(_)
                                    )
                                )
                        })
                {
                    let range = if a.offset.0 <= b.offset.0 {
                        selection.clone()
                    } else {
                        Selection {
                            anchor: selection.focus.clone(),
                            focus: selection.anchor.clone(),
                        }
                    };
                    let mut plan = self.prepare_editor(
                        &[EditorCommand::Text(TextCommand::Replace {
                            selection: range,
                            text: blocks[0].runs.iter().map(|run| run.text.as_str()).collect(),
                        })],
                        origin,
                    )?;
                    // The text fast path still owns the fragment's original
                    // range, including direction and endpoint affinity.
                    plan.entry_selection = Some(selection.clone());
                    let end = if a.offset.0 <= b.offset.0 { &b } else { &a };
                    plan.result_caret = Some(
                        self.anchor(&end.node, end.offset, crate::anchors::Bias::Right)
                            .map_err(|_| EditorError::InvalidCommand)?,
                    );
                    return Ok(plan);
                }
            }
        }
        let baseline = self.checkpoint();
        let mut stage =
            Document::restore(self.identity(), &baseline, &[]).map_err(EditorError::Admission)?;
        let position =
            |doc: &Document, anchor: &crate::anchors::Anchor| match doc.resolve_anchor(anchor) {
                Ok(Resolution::Position(p)) => Ok(p),
                _ => Err(EditorError::UnsupportedTarget),
            };
        let snapshot = stage.inspect().map_err(|_| EditorError::InvalidCommand)?;
        let nodes: Vec<_> = snapshot
            .visible
            .iter()
            .filter(|n| n.kind == "block")
            .collect();
        // Resolve the caller's stable targets against the live immutable
        // baseline, before staging. Recovery-only aliases of an empty source
        // must not replace a visible destination while cloning for preparation.
        let a = position(self, &selection.anchor)?;
        let b = position(self, &selection.focus)?;
        let ai = nodes
            .iter()
            .position(|n| n.id == a.node)
            .ok_or(EditorError::UnsupportedTarget)?;
        let bi = nodes
            .iter()
            .position(|n| n.id == b.node)
            .ok_or(EditorError::UnsupportedTarget)?;
        let (start, end, first, last) = if (ai, a.offset.0) <= (bi, b.offset.0) {
            (a, b, ai, bi)
        } else {
            (b, a, bi, ai)
        };
        // Whole admitted atoms are removable only within this disposable
        // transfer plan. Ordinary text/Join admission remains unchanged. Validate
        // every selected atom before staging any presentation change; a partial
        // label or unknown tuple must never be flattened into editable text.
        let mut selected_atoms = Vec::new();
        for (index, node) in nodes.iter().enumerate().take(last + 1).skip(first) {
            let Some(BlockAttributes::Unknown(tuple)) = &node.block_attributes else {
                continue;
            };
            let length = node
                .runs
                .iter()
                .map(|r| r.text.encode_utf16().count() as u32)
                .sum::<u32>();
            let from = if index == first { start.offset.0 } else { 0 };
            let to = if index == last { end.offset.0 } else { length };
            if from != 0
                || to != length
                || (first == last && length == 0)
                || crate::modules::content::Content::from_tuple(tuple)
                    .map_err(|_| EditorError::UnsupportedTarget)?
                    .is_none()
            {
                return Err(EditorError::UnsupportedTarget);
            }
            selected_atoms.push(node.id.clone());
        }
        for id in selected_atoms {
            m::set(&stage.doc, 4, &id, BlockAttributes::Paragraph.tuple());
        }
        // Ordinary joins require identical code witnesses. A fragment replaces
        // the selected range and supplies its own final presentation, so align
        // only selected tail owners on this disposable replica before joining.
        // Original sources/cuts remain intact; failed staging never reaches live
        // content and undo restores the complete entry presentation.
        let inherited = nodes[first]
            .block_attributes
            .clone()
            .unwrap_or(BlockAttributes::Paragraph);
        // Preserve REG-06 rejection: staging a tail as code must never strip
        // marks from the unselected suffix retained in the result.
        if first != last
            && matches!(inherited, BlockAttributes::Code(_))
            && matches!(
                blocks.last().and_then(|b| b.attributes.as_ref()),
                None | Some(BlockAttributes::Code(_))
            )
        {
            let mut offset = 0;
            for run in &nodes[last].runs {
                offset += run.text.encode_utf16().count() as u32;
                if offset > end.offset.0 && !run.attributes.is_empty() {
                    return Err(EditorError::UnsupportedTarget);
                }
            }
        }
        let mut presentations = Vec::new();
        for node in nodes.iter().take(last + 1).skip(first + 1) {
            let attributes = node.block_attributes.as_ref();
            if (matches!(inherited, BlockAttributes::Code(_))
                || matches!(attributes, Some(BlockAttributes::Code(_))))
                && attributes != Some(&inherited)
            {
                let command = match &inherited {
                    BlockAttributes::Code(language) => super::blocks::BlockCommand::SetCode {
                        owner: node.id.clone(),
                        language: language.clone(),
                    },
                    _ => super::blocks::BlockCommand::SetParagraph {
                        owner: node.id.clone(),
                    },
                };
                presentations.push(EditorCommand::BlockCommand(command));
            }
        }
        if !presentations.is_empty() {
            let plan = stage.prepare_editor(&presentations, origin)?;
            stage.commit_editor(plan)?;
        }
        let insertion = stage
            .anchor(
                &start.node,
                start.offset,
                if nodes[first].runs.iter().all(|run| run.text.is_empty()) {
                    Bias::Right
                } else {
                    Bias::Left
                },
            )
            .map_err(|_| EditorError::InvalidCommand)?;
        let mut deletes = Vec::new();
        for (index, node) in nodes.iter().enumerate().take(last + 1).skip(first) {
            let from = if index == first { start.offset.0 } else { 0 };
            let to = if index == last {
                end.offset.0
            } else {
                node.runs
                    .iter()
                    .map(|r| r.text.encode_utf16().count() as u32)
                    .sum()
            };
            if from != to {
                deletes.push(EditorCommand::Text(TextCommand::Delete {
                    selection: Selection {
                        anchor: stage
                            .anchor(&node.id, Utf16Offset(from), Bias::Left)
                            .map_err(|_| EditorError::InvalidCommand)?,
                        focus: stage
                            .anchor(&node.id, Utf16Offset(to), Bias::Right)
                            .map_err(|_| EditorError::InvalidCommand)?,
                    },
                }));
            }
        }
        for node in nodes.iter().take(last + 1).skip(first + 1) {
            deletes.push(EditorCommand::Join {
                first: start.node.clone(),
                second: node.id.clone(),
            });
        }
        if !deletes.is_empty() {
            let plan = stage.prepare_editor(&deletes, origin)?;
            stage.commit_editor(plan)?;
        }
        let cursor = if nodes[first].runs.iter().all(|run| run.text.is_empty()) {
            // Deleting/joining the selected tail preserves this empty first
            // owner. Its source cut can also occur in a preceding owner.
            start.clone()
        } else {
            position(&stage, &insertion)?
        };
        let mut result = insertion;
        let mut imported = Vec::new();
        // Split only existing text. Incoming payloads are independent immutable
        // original sources; redo cannot retarget frozen cuts into reinserted text.
        let before = stage.inspect().map_err(|_| EditorError::InvalidCommand)?;
        let inherited = nodes[first]
            .block_attributes
            .clone()
            .unwrap_or(BlockAttributes::Paragraph);
        let owner = cursor.node.clone();
        let parent = before
            .visible
            .iter()
            .find(|n| n.id == owner)
            .ok_or(EditorError::UnsupportedTarget)?
            .parent
            .clone();
        let original = before
            .visible
            .iter()
            .find(|n| n.id == owner)
            .ok_or(EditorError::UnsupportedTarget)?;
        let suffix = if original.runs.iter().all(|r| r.text.is_empty()) {
            // An empty multi-source joined owner has no distinct split cut.
            // Create its empty suffix explicitly instead of accepting a dormant
            // split and guessing an owner that was never produced.
            let suffix = stage.next_editor_id(false)?;
            let plan = stage.prepare_editor(
                &[
                    EditorCommand::Create {
                        id: suffix.clone(),
                        kind: "block".into(),
                        text: String::new(),
                        tuple: inherited.tuple(),
                    },
                    EditorCommand::Place {
                        target: suffix.clone(),
                        parent: parent.clone(),
                        // Depend on the destination's structural version. Placing
                        // before its original next sibling could replay before
                        // the join/split which created this empty destination.
                        before: Some(owner.clone()),
                    },
                ],
                origin,
            )?;
            stage.commit_editor(plan)?;
            suffix
        } else {
            let plan = stage.prepare_editor(
                &[EditorCommand::Split {
                    owner: owner.clone(),
                    at: result.clone(),
                }],
                origin,
            )?;
            stage.commit_editor(plan)?;
            let after = stage.inspect().map_err(|_| EditorError::InvalidCommand)?;
            after
                .visible
                .iter()
                .find(|n| n.kind == "block" && !before.visible.iter().any(|old| old.id == n.id))
                .ok_or(EditorError::InvalidCommand)?
                .id
                .clone()
        };
        // Literal insertion inherits admitted destination character attributes.
        let mut offset = 0u32;
        let inherited_marks = nodes[first]
            .runs
            .iter()
            .find_map(|run| {
                let start = offset;
                offset += run.text.encode_utf16().count() as u32;
                (start <= cursor.offset.0 && cursor.offset.0 <= offset)
                    .then_some(run.attributes.clone())
            })
            .unwrap_or_default();
        for block in blocks {
            let text: String = block.runs.iter().map(|r| r.text.as_str()).collect();
            let fresh = stage.next_editor_id(false)?;
            let tuple = if matches!(inherited, BlockAttributes::Code(_)) {
                inherited.tuple()
            } else {
                BlockAttributes::Paragraph.tuple()
            };
            let plan = stage.prepare_editor(
                &[
                    EditorCommand::Create {
                        id: fresh.clone(),
                        kind: "block".into(),
                        text: text.clone(),
                        tuple,
                    },
                    EditorCommand::Place {
                        target: fresh.clone(),
                        parent: parent.clone(),
                        before: Some(suffix.clone()),
                    },
                ],
                origin,
            )?;
            stage.commit_editor(plan)?;
            if block.attributes.is_none() && !text.is_empty() && !inherited_marks.is_empty() {
                use yrs::{Out, Text};
                let mut tx = stage.doc.transact_mut_with(origin.as_str());
                let source = match tx.get_map(m::ROOTS[1]).unwrap().get(&tx, &fresh) {
                    Some(Out::YXmlText(source)) => source,
                    _ => return Err(EditorError::InvalidCommand),
                };
                let attributes = inherited_marks
                    .iter()
                    .map(|(key, value)| (key.as_str().into(), value.clone()))
                    .collect();
                source.format(&mut tx, 0, text.encode_utf16().count() as u32, attributes);
            }
            let mut offset = 0u32;
            let mut marks = Vec::new();
            for run in &block.runs {
                let end = offset + run.text.encode_utf16().count() as u32;
                for (mark, href) in &run.marks {
                    if end > offset {
                        marks.push(FormatCommand {
                            selection: Selection {
                                anchor: stage
                                    .anchor(&fresh, Utf16Offset(offset), Bias::Right)
                                    .map_err(|_| EditorError::InvalidCommand)?,
                                focus: stage
                                    .anchor(&fresh, Utf16Offset(end), Bias::Left)
                                    .map_err(|_| EditorError::InvalidCommand)?,
                            },
                            mark: *mark,
                            action: FormatAction::Apply,
                            href: href.clone(),
                        });
                    }
                }
                offset = end;
            }
            for batch in marks.chunks(128) {
                let plan = stage
                    .prepare_format(batch, origin)
                    .map_err(|_| EditorError::InvalidCommand)?;
                stage
                    .commit_format(plan)
                    .map_err(|_| EditorError::InvalidCommand)?;
            }
            imported.push(fresh);
        }
        let first_import = imported[0].clone();
        if original.runs.iter().all(|r| r.text.is_empty()) {
            // The suffix and payload now depend on the existing destination.
            // Move that destination in front only after all payload placement;
            // the resulting join cannot race its own placement predecessors.
            let plan = stage.prepare_editor(
                &[EditorCommand::Place {
                    target: owner.clone(),
                    parent: parent.clone(),
                    before: Some(first_import.clone()),
                }],
                origin,
            )?;
            stage.commit_editor(plan)?;
        }
        let plan = stage.prepare_editor(
            &[EditorCommand::Join {
                first: owner.clone(),
                second: first_import,
            }],
            origin,
        )?;
        stage.commit_editor(plan)?;
        imported[0] = owner;
        let last = imported.last().unwrap().clone();
        // Capture from the newly registered payload before attaching old suffix.
        let end = stage
            .inspect()
            .map_err(|_| EditorError::InvalidCommand)?
            .visible
            .iter()
            .find(|n| n.id == last)
            .ok_or(EditorError::InvalidCommand)?
            .runs
            .iter()
            .map(|r| r.text.encode_utf16().count() as u32)
            .sum();
        result = stage
            .anchor(&last, Utf16Offset(end), Bias::Right)
            .map_err(|_| EditorError::InvalidCommand)?;
        let plan = stage.prepare_editor(
            &[EditorCommand::Join {
                first: last,
                second: suffix,
            }],
            origin,
        )?;
        stage.commit_editor(plan)?;
        for depth in 0..16 {
            let mut first = 0;
            while first < blocks.len() {
                let Some((key, style)) = blocks[first].containers.get(depth) else {
                    first += 1;
                    continue;
                };
                let mut end = first + 1;
                while end < blocks.len()
                    && blocks[end].containers.get(depth) == Some(&(key.clone(), style.clone()))
                {
                    end += 1;
                }
                let plan = stage.prepare_editor(
                    &[EditorCommand::List(super::lists::ListCommand::Wrap {
                        targets: imported[first..end].to_vec(),
                        style: style.clone(),
                    })],
                    origin,
                )?;
                stage.commit_editor(plan)?;
                first = end;
            }
        }
        // Freeze every destination presentation before installing opaque data.
        // A split child otherwise inherits its parent's later tuple, which could
        // turn a suffix or the next atom into an unsupported opaque edit target.
        if has_atoms {
            let plan = stage
                .prepare_format_batch(
                    &[],
                    &[BlockFormatCommand {
                        targets: imported.clone(),
                        attributes: BlockAttributes::Paragraph,
                        action: FormatAction::Apply,
                    }],
                    origin,
                )
                .map_err(|_| EditorError::InvalidCommand)?;
            stage
                .commit_format(plan)
                .map_err(|_| EditorError::InvalidCommand)?;
        }
        let mut content_commands = Vec::new();
        for (block, owner) in blocks.iter().zip(&imported) {
            if let Some(checked) = block.checked {
                let plan = stage.prepare_editor(
                    &[EditorCommand::List(super::lists::ListCommand::SetChecked {
                        owner: owner.clone(),
                        checked,
                    })],
                    origin,
                )?;
                stage.commit_editor(plan)?;
            }
            if let Some(attributes) = block
                .attributes
                .as_ref()
                .or(has_atoms.then_some(&inherited))
            {
                match attributes {
                    BlockAttributes::Code(language) => {
                        let plan = stage.prepare_editor(
                            &[EditorCommand::BlockCommand(
                                super::blocks::BlockCommand::SetCode {
                                    owner: owner.clone(),
                                    language: language.clone(),
                                },
                            )],
                            origin,
                        )?;
                        stage.commit_editor(plan)?;
                    }
                    BlockAttributes::Paragraph | BlockAttributes::Heading(_) => {
                        let plan = stage
                            .prepare_format_batch(
                                &[],
                                &[BlockFormatCommand {
                                    targets: vec![owner.clone()],
                                    attributes: attributes.clone(),
                                    action: FormatAction::Apply,
                                }],
                                origin,
                            )
                            .map_err(|_| EditorError::InvalidCommand)?;
                        stage
                            .commit_format(plan)
                            .map_err(|_| EditorError::InvalidCommand)?;
                    }
                    BlockAttributes::Separator => {
                        let plan = stage.prepare_editor(
                            &[EditorCommand::BlockCommand(
                                super::blocks::BlockCommand::ConvertSeparator {
                                    owner: owner.clone(),
                                },
                            )],
                            origin,
                        )?;
                        stage.commit_editor(plan)?;
                    }
                    BlockAttributes::Unknown(tuple) => {
                        let content = crate::modules::content::Content::from_tuple(tuple)
                            .map_err(|_| EditorError::InvalidCommand)?
                            .ok_or(EditorError::UnsupportedTarget)?;
                        content_commands.push(EditorCommand::Content {
                            target: owner.clone(),
                            content,
                        });
                    }
                }
            }
        }
        if !content_commands.is_empty() {
            let plan = stage.prepare_editor(&content_commands, origin)?;
            stage.commit_editor(plan)?;
        }
        crate::document::validate_immutable_structure(&self.doc, &stage.doc, true)
            .map_err(EditorError::Admission)?;
        super::lists::validate_editor_hierarchy(
            &stage.inspect().map_err(|_| EditorError::InvalidCommand)?,
            &[],
        )
        .map_err(|_| EditorError::InvalidCommand)?;
        let update = stage
            .update_v1(&self.state_vector_v1())
            .map_err(EditorError::Admission)?;
        if stage.checkpoint().bytes.len() > MAX_RESTORE_BYTES {
            return Err(EditorError::ResourceLimit);
        }
        Ok(PreparedEditorEdit {
            identity: self.identity().clone(),
            writer: self.writer_id(),
            revision: self.revision(),
            baseline: baseline.bytes,
            update,
            origin: origin.clone(),
            next_operation: self.next_structure_counter,
            result_caret: Some(result),
            result_selection: None,
            entry_selection: Some(selection.clone()),
        })
    }
}
fn error(e: &str) -> EditorError {
    match e {
        "resource-limit" => EditorError::ResourceLimit,
        "unsupported-target" => EditorError::UnsupportedTarget,
        _ => EditorError::InvalidCommand,
    }
}
fn tag(a: &Any) -> &str {
    match a {
        Any::Array(v) => match v.first() {
            Some(Any::String(s)) => s,
            _ => "",
        },
        _ => "",
    }
}
fn target(d: &yrs::Doc, stable: bool, id: &str) -> Result<m::Owner, EditorError> {
    crate::editor::view(d, stable)
        .map_err(error)?
        .owners
        .into_iter()
        .find(|o| o.id == id && !o.deleted)
        .ok_or(EditorError::UnsupportedTarget)
}
fn register(
    d: &yrs::Doc,
    writer: u32,
    id: &str,
    kind: &str,
    text: &str,
    tuple: &Any,
    stable: bool,
) -> Result<(), EditorError> {
    let prefix = if kind == "block" {
        "n"
    } else if matches!(kind, "list" | "quote") {
        "c"
    } else {
        return Err(EditorError::UnsupportedTarget);
    };
    let (w, counter) = m::id(id, prefix).map_err(error)?;
    let t = d.transact();
    let kinds = t.get_map(m::ROOTS[0]).unwrap();
    let max = kinds
        .keys(&t)
        .filter_map(|s| m::id(s, prefix).ok())
        .filter(|(w, _)| *w == writer)
        .map(|(_, c)| c)
        .max()
        .unwrap_or(0);
    if w != writer
        || counter <= max
        || kinds.contains_key(&t, id)
        || (prefix == "c" && !text.is_empty())
    {
        return Err(EditorError::InvalidCommand);
    }
    if (kind == "block" && !matches!(tag(tuple), "paragraph" | "heading" | "code" | "separator"))
        || (kind == "quote" && tag(tuple) != "quote")
        || (kind == "list" && !matches!(tag(tuple), "ordered" | "unordered" | "checklist"))
    {
        return Err(EditorError::UnsupportedTarget);
    }
    if kind == "block" && tag(tuple) == "separator" && !text.is_empty() {
        return Err(EditorError::InvalidCommand);
    }
    let order = if stable {
        let max = t
            .get_map(m::ROOTS[2])
            .unwrap()
            .iter(&t)
            .filter_map(|(_, v)| {
                let yrs::Out::Any(Any::Array(row)) = v else {
                    return None;
                };
                if row.first() != Some(&Any::from("@root")) {
                    return None;
                }
                let Some(Any::Number(number)) = row.get(1) else {
                    return None;
                };
                number.as_i64().and_then(|n| u32::try_from(n).ok())
            })
            .max()
            .unwrap_or(0);
        max.checked_add(1)
            .filter(|n| *n <= 2147483647)
            .ok_or(EditorError::ResourceLimit)?
    } else {
        0
    };
    drop(t);
    let mut t = d.transact_mut_with("prepare:editor");
    t.get_map(m::ROOTS[0]).unwrap().insert(&mut t, id, kind);
    t.get_map(m::ROOTS[2]).unwrap().insert(
        &mut t,
        id,
        m::arr(vec![Any::from("@root"), Any::from(order)]),
    );
    if kind == "block" {
        t.get_map(m::ROOTS[1])
            .unwrap()
            .insert(&mut t, id, XmlTextPrelim::new(text));
        t.get_map(m::ROOTS[6]).unwrap().insert(&mut t, id, true);
    }
    t.get_map(m::ROOTS[if kind == "block" { 4 } else { 7 }])
        .unwrap()
        .insert(&mut t, id, tuple.clone());
    Ok(())
}
impl Document {
    /// Suggest the next retained namespace ID; reservation occurs only on commit.
    /// Portable semantic view; unknown marks are not silently removed.
    pub fn fragment_for_node(&self, id: &str) -> Result<FragmentBlock, EditorError> {
        use super::{format::Mark, lists::ContainerStyle};
        let snapshot = self.inspect().map_err(|_| EditorError::InvalidCommand)?;
        let node = snapshot
            .visible
            .iter()
            .find(|n| n.id == id && n.kind == "block")
            .ok_or(EditorError::UnsupportedTarget)?;
        let mut runs = Vec::new();
        for run in &node.runs {
            let mut marks = Vec::new();
            for (key, value) in &run.attributes {
                if matches!(value, Any::Null | Any::Bool(false)) {
                    continue;
                }
                let mark = match key.as_str() {
                    "bold" => Mark::Bold,
                    "italic" => Mark::Italic,
                    "strike" => Mark::Strike,
                    "underline" => Mark::Underline,
                    "code" => Mark::Code,
                    "link" => Mark::Link,
                    _ => return Err(EditorError::UnsupportedTarget),
                };
                let href = if mark == Mark::Link {
                    let Any::String(href) = value else {
                        return Err(EditorError::UnsupportedTarget);
                    };
                    Some(href.to_string())
                } else {
                    if value != &Any::Bool(true) {
                        return Err(EditorError::UnsupportedTarget);
                    }
                    None
                };
                marks.push((mark, href));
            }
            runs.push(FragmentRun {
                text: run.text.clone(),
                marks,
            });
        }
        let mut containers = Vec::new();
        let mut parent = node.parent.as_str();
        while parent != "@root" {
            if containers.len() >= 16 {
                return Err(EditorError::ResourceLimit);
            }
            let container = snapshot
                .visible
                .iter()
                .find(|n| n.id == parent)
                .ok_or(EditorError::UnsupportedTarget)?;
            let Some(Any::Array(tuple)) = container
                .editor
                .as_ref()
                .and_then(|e| e.container_attributes.as_ref())
            else {
                return Err(EditorError::UnsupportedTarget);
            };
            let style = match tuple.first() {
                Some(Any::String(kind)) if kind.as_ref() == "unordered" => {
                    ContainerStyle::Unordered
                }
                Some(Any::String(kind)) if kind.as_ref() == "checklist" => {
                    ContainerStyle::Checklist
                }
                Some(Any::String(kind)) if kind.as_ref() == "quote" => ContainerStyle::Quote,
                Some(Any::String(kind)) if kind.as_ref() == "ordered" => {
                    let Some(Any::Number(start)) = tuple.get(1) else {
                        return Err(EditorError::UnsupportedTarget);
                    };
                    ContainerStyle::Ordered {
                        start: start
                            .as_f64()
                            .filter(|v| v.fract() == 0.0 && (1.0..=2147483647.0).contains(v))
                            .ok_or(EditorError::InvalidCommand)?
                            as u32,
                    }
                }
                _ => return Err(EditorError::UnsupportedTarget),
            };
            containers.push((container.id.clone(), style));
            parent = &container.parent;
        }
        containers.reverse();
        Ok(FragmentBlock {
            runs,
            attributes: node.block_attributes.clone(),
            containers,
            checked: node.editor.as_ref().and_then(|e| e.checked),
        })
    }

    /// Source edits preserve existing owner/source identities and stage one update.
    pub fn prepare_source_edits(
        &self,
        edits: &[(crate::anchors::Selection, Vec<FragmentBlock>)],
        origin: &TextOrigin,
    ) -> Result<PreparedEditorEdit, EditorError> {
        use super::{
            format::{FormatAction, FormatCommand, Mark},
            text::TextCommand,
        };
        use crate::anchors::{Bias, Resolution, Selection};
        if edits.is_empty() || edits.len() > 128 {
            return Err(EditorError::InvalidCommand);
        }
        let mut owners = std::collections::BTreeSet::new();
        for (selection, blocks) in edits {
            let (Ok(Resolution::Position(a)), Ok(Resolution::Position(b))) = (
                self.resolve_anchor(&selection.anchor),
                self.resolve_anchor(&selection.focus),
            ) else {
                return Err(EditorError::UnsupportedTarget);
            };
            if a.node != b.node
                || a.offset.0 > b.offset.0
                || !owners.insert(a.node)
                || blocks.len() != 1
                || !blocks[0].containers.is_empty()
            {
                return Err(EditorError::InvalidCommand);
            }
        }
        let baseline = self.checkpoint();
        let mut stage =
            Document::restore(self.identity(), &baseline, &[]).map_err(EditorError::Admission)?;
        for (selection, blocks) in edits {
            let block = &blocks[0];
            let (Ok(Resolution::Position(a)), Ok(Resolution::Position(b))) = (
                stage.resolve_anchor(&selection.anchor),
                stage.resolve_anchor(&selection.focus),
            ) else {
                return Err(EditorError::UnsupportedTarget);
            };
            let was_code = matches!(
                stage.fragment_for_node(&a.node)?.attributes,
                Some(crate::document::BlockAttributes::Code(_))
            );
            let will_code = block.attributes.as_ref().map_or(was_code, |attributes| {
                matches!(attributes, crate::document::BlockAttributes::Code(_))
            });
            if will_code && block.runs.iter().any(|r| !r.marks.is_empty()) {
                return Err(EditorError::InvalidCommand);
            }
            if was_code && !will_code {
                if let Some(attributes) = &block.attributes {
                    let plan = stage.prepare_editor(
                        &[EditorCommand::Block {
                            target: a.node.clone(),
                            tuple: attributes.tuple(),
                        }],
                        origin,
                    )?;
                    stage.commit_editor(plan)?;
                }
            }
            let text: String = block.runs.iter().map(|r| r.text.as_str()).collect();
            let caret = stage
                .anchor(&a.node, b.offset, Bias::Right)
                .map_err(|_| EditorError::InvalidCommand)?;
            let plan = stage
                .prepare_text(
                    &TextCommand::Replace {
                        selection: selection.clone(),
                        text: text.clone(),
                    },
                    origin,
                )
                .map_err(EditorError::Text)?;
            stage.commit_text(plan).map_err(EditorError::Text)?;
            let Ok(Resolution::Position(end)) = stage.resolve_anchor(&caret) else {
                return Err(EditorError::UnsupportedTarget);
            };
            let length = text.encode_utf16().count() as u32;
            let start = end
                .offset
                .0
                .checked_sub(length)
                .ok_or(EditorError::InvalidCommand)?;
            let mut formats = Vec::new();
            if length > 0 {
                let range = Selection {
                    anchor: stage
                        .anchor(&a.node, crate::Utf16Offset(start), Bias::Right)
                        .map_err(|_| EditorError::InvalidCommand)?,
                    focus: stage
                        .anchor(&a.node, end.offset, Bias::Left)
                        .map_err(|_| EditorError::InvalidCommand)?,
                };
                for mark in [
                    Mark::Bold,
                    Mark::Italic,
                    Mark::Strike,
                    Mark::Underline,
                    Mark::Code,
                    Mark::Link,
                ] {
                    formats.push(FormatCommand {
                        selection: range.clone(),
                        mark,
                        action: FormatAction::Remove,
                        href: None,
                    });
                }
            }
            let mut offset = start;
            for run in &block.runs {
                let end = offset + run.text.encode_utf16().count() as u32;
                if end > offset {
                    for (mark, href) in &run.marks {
                        formats.push(FormatCommand {
                            selection: Selection {
                                anchor: stage
                                    .anchor(&a.node, crate::Utf16Offset(offset), Bias::Right)
                                    .map_err(|_| EditorError::InvalidCommand)?,
                                focus: stage
                                    .anchor(&a.node, crate::Utf16Offset(end), Bias::Left)
                                    .map_err(|_| EditorError::InvalidCommand)?,
                            },
                            mark: *mark,
                            action: FormatAction::Apply,
                            href: href.clone(),
                        });
                    }
                }
                offset = end;
            }
            for batch in formats.chunks(128).filter(|_| !(was_code && will_code)) {
                let plan = stage
                    .prepare_format(batch, origin)
                    .map_err(|_| EditorError::InvalidCommand)?;
                stage
                    .commit_format(plan)
                    .map_err(|_| EditorError::InvalidCommand)?;
            }
            let mut presentation = Vec::new();
            if let Some(attributes) = &block.attributes {
                if !matches!(
                    attributes,
                    crate::document::BlockAttributes::Paragraph
                        | crate::document::BlockAttributes::Heading(_)
                        | crate::document::BlockAttributes::Code(_)
                ) {
                    return Err(EditorError::UnsupportedTarget);
                }
                presentation.push(EditorCommand::Block {
                    target: a.node.clone(),
                    tuple: attributes.tuple(),
                });
            }
            if let Some(checked) = block.checked {
                presentation.push(EditorCommand::Check {
                    target: a.node.clone(),
                    checked,
                });
            }
            if !presentation.is_empty() {
                let plan = stage.prepare_editor(&presentation, origin)?;
                stage.commit_editor(plan)?;
            }
        }
        crate::document::validate_immutable_structure(&self.doc, &stage.doc, true)
            .map_err(EditorError::Admission)?;
        super::lists::validate_editor_hierarchy(
            &stage.inspect().map_err(|_| EditorError::InvalidCommand)?,
            &[],
        )
        .map_err(|_| EditorError::InvalidCommand)?;
        if stage.checkpoint().bytes.len() > MAX_RESTORE_BYTES {
            return Err(EditorError::ResourceLimit);
        }
        Ok(PreparedEditorEdit {
            identity: self.identity().clone(),
            writer: self.writer_id(),
            revision: self.revision(),
            baseline: baseline.bytes,
            update: stage
                .update_v1(&self.state_vector_v1())
                .map_err(EditorError::Admission)?,
            origin: origin.clone(),
            next_operation: self.next_structure_counter,
            result_caret: None,
            result_selection: None,
            entry_selection: None,
        })
    }

    pub fn next_editor_id(&self, container: bool) -> Result<String, EditorError> {
        if !self.identity().has_editor() {
            return Err(EditorError::UnsupportedProfile);
        }
        let prefix = if container { "c" } else { "n" };
        let t = self.read();
        let max = t
            .get_map(m::ROOTS[0])
            .unwrap()
            .keys(&t)
            .filter_map(|s| m::id(s, prefix).ok())
            .filter(|(w, _)| *w == self.writer_id())
            .map(|(_, c)| c)
            .max()
            .unwrap_or(0);
        Ok(format!(
            "{prefix}:{}:{}",
            self.writer_id(),
            max.checked_add(1).ok_or(EditorError::ResourceLimit)?
        ))
    }
    pub fn can_editor(
        &self,
        commands: &[EditorCommand],
        origin: &TextOrigin,
    ) -> Result<(), EditorError> {
        self.prepare_editor(commands, origin).map(|_| ())
    }
    pub fn prepare_editor(
        &self,
        commands: &[EditorCommand],
        origin: &TextOrigin,
    ) -> Result<PreparedEditorEdit, EditorError> {
        if !self.identity().has_editor() {
            return Err(EditorError::UnsupportedProfile);
        }
        if commands.len() > 4096 {
            return Err(EditorError::ResourceLimit);
        }
        let stable = self.identity().has_stable_presentation();
        let baseline = self.checkpoint().bytes;
        let stage = m::replica(self.writer_id() as u64);
        m::apply(&stage, &baseline).map_err(error)?;
        let sv = stage.transact().state_vector();
        let mut counter = self.next_structure_counter;
        let mut last_separator = None;
        let mut result_caret = None;
        let mut result_selection = None;
        let mut entry_selection = None;
        let mut result_owner = None;
        let mut pending: VecDeque<_> = commands.to_vec().into();
        let mut expanded = 0usize;
        while let Some(c) = pending.pop_front() {
            expanded += 1;
            if expanded > 16384 {
                return Err(EditorError::ResourceLimit);
            }
            match &c {
                EditorCommand::ReplaceAt {
                    owner,
                    from,
                    to,
                    text,
                } => {
                    let d = self.virtual_document(&stage)?;
                    let selection = d
                        .selection(
                            (owner, *from, crate::anchors::Bias::Right),
                            (owner, *to, crate::anchors::Bias::Left),
                        )
                        .map_err(|_| EditorError::InvalidCommand)?;
                    let plan = d
                        .prepare_text(
                            &super::text::TextCommand::Replace {
                                selection,
                                text: text.clone(),
                            },
                            origin,
                        )
                        .map_err(EditorError::Text)?;
                    super::text::apply_prepared_to_stage(&stage, plan);
                }
                EditorCommand::MarkAt {
                    owner,
                    from,
                    to,
                    mark,
                    action,
                    href,
                } => {
                    let mut d = self.virtual_document(&stage)?;
                    let selection = d
                        .selection(
                            (owner, *from, crate::anchors::Bias::Right),
                            (owner, *to, crate::anchors::Bias::Left),
                        )
                        .map_err(|_| EditorError::InvalidCommand)?;
                    let plan = d
                        .prepare_format(
                            &[super::format::FormatCommand {
                                selection,
                                mark: *mark,
                                action: *action,
                                href: href.clone(),
                            }],
                            origin,
                        )
                        .map_err(EditorError::Format)?;
                    let commit = d.commit_format(plan).map_err(EditorError::Format)?;
                    if let Some(update) = commit.update {
                        m::apply(&stage, &update.bytes).map_err(error)?;
                    }
                }
                EditorCommand::HistorySelectionAt { anchor, focus } => {
                    if expanded != 1 {
                        return Err(EditorError::InvalidCommand);
                    }
                    entry_selection = Some(
                        self.selection(
                            (&anchor.owner, anchor.offset, anchor.bias),
                            (&focus.owner, focus.offset, focus.bias),
                        )
                        .map_err(|_| EditorError::InvalidCommand)?,
                    );
                }
                EditorCommand::SelectionAt { anchor, focus } => {
                    if !pending.is_empty() {
                        return Err(EditorError::InvalidCommand);
                    }
                    let d = self.virtual_document(&stage)?;
                    result_selection = Some(
                        d.selection(
                            (&anchor.owner, anchor.offset, anchor.bias),
                            (&focus.owner, focus.offset, focus.bias),
                        )
                        .map_err(|_| EditorError::InvalidCommand)?,
                    );
                }
                EditorCommand::Move(command) => {
                    let step = super::r#move::expand(&stage, stable, command)?;
                    pending.push_front(step);
                }
                EditorCommand::List(command) => {
                    let steps =
                        super::lists::expand(&stage, self.identity(), self.writer_id(), command)?;
                    for step in steps.into_iter().rev() {
                        pending.push_front(step);
                    }
                }
                EditorCommand::BlockCommand(command) => {
                    let steps =
                        super::blocks::expand(&stage, self.identity(), self.writer_id(), command)?;
                    for step in steps.into_iter().rev() {
                        pending.push_front(step);
                    }
                }
                EditorCommand::Text(command) => {
                    // Reuse the exact text preparation path on disposable state. Its
                    // plans cannot notify live observers or reserve live identities.
                    let checkpoint = crate::document::InitializedCheckpoint {
                        identity: self.identity().clone(),
                        initialized: true,
                        writer_id: self.writer_id(),
                        bytes: m::bytes(&stage),
                    };
                    let text_stage = Document::restore(self.identity(), &checkpoint, &[])
                        .map_err(EditorError::Admission)?;
                    let plan = text_stage
                        .prepare_text(command, origin)
                        .map_err(EditorError::Text)?;
                    super::text::apply_prepared_to_stage(&stage, plan);
                }
                EditorCommand::Split { owner, at } | EditorCommand::HeadingEnd { owner, at } => {
                    let heading_end = matches!(c, EditorCommand::HeadingEnd { .. });
                    if heading_end && !stable {
                        return Err(EditorError::UnsupportedProfile);
                    }
                    let snapshot =
                        crate::projection::ProjectionIndex::with_identity(&stage, self.identity())
                            .read()
                            .map_err(|_| EditorError::InvalidCommand)?;
                    let node = snapshot
                        .visible
                        .iter()
                        .find(|n| n.id == *owner)
                        .ok_or(EditorError::UnsupportedTarget)?;
                    if node.kind != "block"
                        || !matches!(
                            node.block_attributes,
                            Some(
                                crate::document::BlockAttributes::Paragraph
                                    | crate::document::BlockAttributes::Heading(_)
                                    | crate::document::BlockAttributes::Code(_)
                            )
                        )
                    {
                        return Err(EditorError::UnsupportedTarget);
                    }
                    let selected = crate::projection::Snapshot {
                        visible: vec![node.clone()],
                        ..Default::default()
                    };
                    let p = super::text::position(crate::anchors::resolve(
                        &stage.transact(),
                        self.identity(),
                        &selected,
                        at,
                    ))
                    .map_err(EditorError::Text)?;
                    if heading_end {
                        let length: usize = node
                            .runs
                            .iter()
                            .map(|r| r.text.encode_utf16().count())
                            .sum();
                        if !matches!(
                            node.block_attributes,
                            Some(crate::document::BlockAttributes::Heading(_))
                        ) || length == 0
                            || p.offset.0 as usize != length
                        {
                            return Err(EditorError::UnsupportedTarget);
                        }
                    }
                    // Contextual list/quote Enter expands through Split too; keep
                    // those source-preserving policies unchanged.
                    let terminal_paragraph = node.parent == "@root"
                        && matches!(
                            node.block_attributes,
                            Some(crate::document::BlockAttributes::Paragraph)
                        )
                        && p.offset.0 as usize
                            == node
                                .runs
                                .iter()
                                .map(|r| r.text.encode_utf16().count())
                                .sum::<usize>();
                    if heading_end || terminal_paragraph {
                        // A fresh source prevents collisions with existing source cuts.
                        // Every structural step is confined to the disposable replica.
                        let o = target(&stage, stable, owner)?;
                        let view = crate::editor::view(&stage, stable).map_err(error)?;
                        let at = view
                            .owners
                            .iter()
                            .position(|n| n.id == *owner)
                            .ok_or(EditorError::UnsupportedTarget)?;
                        let before = view
                            .owners
                            .iter()
                            .skip(at + 1)
                            .find(|n| {
                                !n.deleted
                                    && n.parent == o.parent
                                    && (n.kind != "block" || !n.spans.is_empty())
                            })
                            .map(|n| n.id.clone());
                        let txn = stage.transact();
                        let max = txn
                            .get_map(m::ROOTS[0])
                            .unwrap()
                            .keys(&txn)
                            .filter_map(|s| m::id(s, "n").ok())
                            .filter(|(w, _)| *w == self.writer_id())
                            .map(|(_, c)| c)
                            .max()
                            .unwrap_or(0);
                        drop(txn);
                        let fresh = format!(
                            "n:{}:{}",
                            self.writer_id(),
                            max.checked_add(1).ok_or(EditorError::ResourceLimit)?
                        );
                        register(
                            &stage,
                            self.writer_id(),
                            &fresh,
                            "block",
                            "",
                            &m::arr(vec![Any::from("paragraph")]),
                            stable,
                        )?;
                        // Register at the initial root tail before replaying existing
                        // operations. A new order-zero source could otherwise obstruct
                        // an earlier selected-range Join before its Place is replayed.
                        let txn = stage.transact();
                        let max_order = txn
                            .get_map(m::ROOTS[2])
                            .unwrap()
                            .keys(&txn)
                            .filter(|key| *key != fresh.as_str())
                            .filter_map(|key| txn.get_map(m::ROOTS[2]).unwrap().get(&txn, key))
                            .filter_map(|v| match v {
                                yrs::Out::Any(Any::Array(a)) => match a.get(1) {
                                    Some(Any::Number(n)) => n.as_f64().map(|n| n as u32),
                                    _ => None,
                                },
                                _ => None,
                            })
                            .max()
                            .unwrap_or(0);
                        drop(txn);
                        m::set(
                            &stage,
                            2,
                            &fresh,
                            m::arr(vec![
                                Any::from("@root"),
                                Any::from(
                                    max_order.checked_add(1).ok_or(EditorError::ResourceLimit)?,
                                ),
                            ]),
                        );
                        let mut operation = || -> Result<String, EditorError> {
                            let op = format!("o:{}:{counter}", self.writer_id());
                            counter = counter.checked_add(1).ok_or(EditorError::ResourceLimit)?;
                            Ok(op)
                        };
                        // In v5, placing before the old next sibling only witnesses
                        // that sibling. It can replay ahead of the owner's earlier
                        // fragment joins and obstruct their adjacency. Witness the
                        // current owner first, then move it before the fresh source:
                        // both placements now follow its complete operation lineage.
                        if stable {
                            crate::editor::place(
                                &stage,
                                stable,
                                &operation()?,
                                &fresh,
                                &o.parent,
                                Some(owner),
                            )
                            .map_err(error)?;
                            crate::editor::place(
                                &stage,
                                stable,
                                &operation()?,
                                owner,
                                &o.parent,
                                Some(&fresh),
                            )
                            .map_err(error)?;
                        } else if o.parent != "@root" || before.is_some() {
                            crate::editor::place(
                                &stage,
                                stable,
                                &operation()?,
                                &fresh,
                                &o.parent,
                                before.as_deref(),
                            )
                            .map_err(error)?;
                        }
                        let snapshot = crate::projection::ProjectionIndex::with_identity(
                            &stage,
                            self.identity(),
                        )
                        .read()
                        .map_err(|_| EditorError::InvalidCommand)?;
                        result_caret = Some(
                            crate::anchors::capture(
                                &stage.transact(),
                                self.identity(),
                                &snapshot,
                                &fresh,
                                crate::Utf16Offset(0),
                                crate::anchors::Bias::Right,
                            )
                            .map_err(|_| EditorError::InvalidCommand)?,
                        );
                        crate::editor::join(&stage, stable, &operation()?, owner, &fresh)
                            .map_err(error)?;
                        let joined = target(&stage, stable, owner)?;
                        let index = joined
                            .spans
                            .iter()
                            .position(|s| s.source == fresh)
                            .ok_or(EditorError::InvalidCommand)?;
                        let destination = operation()?;
                        crate::editor::split(&stage, stable, &destination, owner, index, 0)
                            .map_err(error)?;
                        m::set(
                            &stage,
                            4,
                            &destination,
                            m::arr(vec![Any::from("paragraph")]),
                        );
                        result_owner = Some(destination);
                        continue;
                    }
                    let o = target(&stage, stable, owner)?;
                    let mut base = 0;
                    let mut candidates = vec![];
                    for (i, span) in o.spans.iter().enumerate() {
                        if span.source == p.source
                            && span.start <= p.source_offset.0
                            && p.source_offset.0 <= span.end
                            && base + p.source_offset.0 - span.start == p.offset.0
                        {
                            candidates.push(i);
                        }
                        base += span.end - span.start;
                    }
                    let index = if p.bias == crate::anchors::Bias::Left {
                        candidates.first()
                    } else {
                        candidates.last()
                    }
                    .ok_or(EditorError::InvalidCommand)?;
                    let op = format!("o:{}:{counter}", self.writer_id());
                    counter = counter.checked_add(1).ok_or(EditorError::ResourceLimit)?;
                    crate::editor::split(&stage, stable, &op, owner, *index, p.source_offset.0)
                        .map_err(error)?;
                }
                EditorCommand::Join { first, second } => {
                    let op = format!("o:{}:{counter}", self.writer_id());
                    counter = counter.checked_add(1).ok_or(EditorError::ResourceLimit)?;
                    for id in [first, second] {
                        let o = target(&stage, stable, id)?;
                        if o.kind != "block"
                            || !matches!(tag(&o.attrs), "paragraph" | "heading" | "code")
                        {
                            return Err(EditorError::UnsupportedTarget);
                        }
                    }
                    crate::editor::join(&stage, stable, &op, first, second).map_err(error)?;
                }
                EditorCommand::Create {
                    id,
                    kind,
                    text,
                    tuple,
                } => {
                    if text.len() > 1048576 {
                        return Err(EditorError::ResourceLimit);
                    }
                    register(&stage, self.writer_id(), id, kind, text, tuple, stable)?;
                    if tag(tuple) == "separator" {
                        last_separator = Some(id.clone());
                    }
                }
                EditorCommand::Place {
                    target,
                    parent,
                    before,
                } => {
                    let op = format!("o:{}:{counter}", self.writer_id());
                    counter = counter.checked_add(1).ok_or(EditorError::ResourceLimit)?;
                    crate::editor::place(&stage, stable, &op, target, parent, before.as_deref())
                        .map_err(error)?;
                }
                EditorCommand::TableCell {
                    target: id,
                    expected,
                    row,
                    column,
                    text,
                } => {
                    use crate::anchors::{Bias, Selection};
                    use crate::modules::content::Content;
                    if text.len() > 1_048_576 {
                        return Err(EditorError::ResourceLimit);
                    }
                    if text.contains(['\n', '\r']) {
                        return Err(EditorError::InvalidCommand);
                    }
                    let o = target(&stage, stable, id)?;
                    if o.kind != "block" {
                        return Err(EditorError::UnsupportedTarget);
                    }
                    if &o.attrs != expected {
                        return Err(EditorError::PresentationConflict);
                    }
                    let Some(Content::Table {
                        alignments,
                        mut rows,
                        source,
                    }) = Content::from_tuple(&o.attrs).map_err(|_| EditorError::InvalidCommand)?
                    else {
                        return Err(EditorError::UnsupportedTarget);
                    };
                    let cell = rows
                        .get_mut(*row)
                        .and_then(|r| r.get_mut(*column))
                        .ok_or(EditorError::InvalidCommand)?;
                    crate::modules::content::edit_cell(cell, text);
                    let content = Content::Table {
                        alignments,
                        rows,
                        source,
                    };
                    let tuple = content.tuple().map_err(|e| {
                        if e == crate::modules::content::ContentError::ResourceLimit {
                            EditorError::ResourceLimit
                        } else {
                            EditorError::InvalidCommand
                        }
                    })?;
                    let Content::Table { rows, .. } = &content else {
                        unreachable!()
                    };
                    let fallback = rows
                        .iter()
                        .map(|row| {
                            row.iter()
                                .map(|cell| {
                                    cell.iter().map(|run| run.text.as_str()).collect::<String>()
                                })
                                .collect::<Vec<_>>()
                                .join("\t")
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    // Temporarily expose the existing source to the ordinary text planner
                    // on disposable state, then install the validated descriptor. One live
                    // commit preserves owner identity and groups metadata/text history.
                    m::set(
                        &stage,
                        4,
                        id,
                        crate::document::BlockAttributes::Paragraph.tuple(),
                    );
                    let checkpoint = crate::document::InitializedCheckpoint {
                        identity: self.identity().clone(),
                        initialized: true,
                        writer_id: self.writer_id(),
                        bytes: m::bytes(&stage),
                    };
                    let d = Document::restore(self.identity(), &checkpoint, &[])
                        .map_err(EditorError::Admission)?;
                    let node = d
                        .inspect()
                        .map_err(|_| EditorError::InvalidCommand)?
                        .visible
                        .into_iter()
                        .find(|n| n.id == *id)
                        .ok_or(EditorError::UnsupportedTarget)?;
                    let length = node
                        .runs
                        .iter()
                        .map(|r| r.text.encode_utf16().count())
                        .sum::<usize>();
                    let selection = Selection {
                        anchor: d
                            .anchor(id, crate::Utf16Offset(0), Bias::Right)
                            .map_err(|_| EditorError::InvalidCommand)?,
                        focus: d
                            .anchor(id, crate::Utf16Offset(length as u32), Bias::Left)
                            .map_err(|_| EditorError::InvalidCommand)?,
                    };
                    let plan = d
                        .prepare_text(
                            &super::text::TextCommand::Replace {
                                selection,
                                text: fallback,
                            },
                            origin,
                        )
                        .map_err(EditorError::Text)?;
                    super::text::apply_prepared_to_stage(&stage, plan);
                    m::set(&stage, 4, id, tuple);
                }
                EditorCommand::Content {
                    target: id,
                    content,
                } => {
                    let o = target(&stage, stable, id)?;
                    if o.kind != "block"
                        || !matches!(tag(&o.attrs), "paragraph" | "heading" | "code")
                    {
                        return Err(EditorError::UnsupportedTarget);
                    }
                    let tuple = content.tuple().map_err(|e| {
                        if e == crate::modules::content::ContentError::ResourceLimit {
                            EditorError::ResourceLimit
                        } else {
                            EditorError::InvalidCommand
                        }
                    })?;
                    m::set(&stage, 4, id, tuple);
                    let after = target(&stage, stable, id)?;
                    if after.spans != o.spans
                        || after.version != o.version
                        || after.parent != o.parent
                    {
                        return Err(EditorError::PresentationConflict);
                    }
                }
                EditorCommand::Block { target: id, tuple } => {
                    let o = target(&stage, stable, id)?;
                    if o.kind != "block"
                        || !matches!(
                            tag(&o.attrs),
                            "paragraph" | "heading" | "code" | "separator"
                        )
                        || !matches!(tag(tuple), "paragraph" | "heading" | "code" | "separator")
                    {
                        return Err(EditorError::UnsupportedTarget);
                    }
                    if tag(tuple) == "separator" && o.spans.iter().any(|s| s.end != s.start) {
                        return Err(EditorError::InvalidCommand);
                    }
                    m::set(&stage, 4, id, tuple.clone());
                    let after = target(&stage, stable, id)?;
                    // Presentation is metadata-only. Until the v4 contract can
                    // retain a join across compatibility changes, reject rather
                    // than silently exposing absorbed owners or changing spans.
                    if !stable
                        && (after.spans != o.spans
                            || after.version != o.version
                            || after.parent != o.parent)
                    {
                        return Err(EditorError::PresentationConflict);
                    }
                    if tag(tuple) == "separator" {
                        last_separator = Some(id.clone());
                    }
                }
                EditorCommand::Container { target: id, tuple } => {
                    let o = target(&stage, stable, id)?;
                    if !((o.kind == "quote" && tag(tuple) == "quote" && tag(&o.attrs) == "quote")
                        || (o.kind == "list"
                            && matches!(tag(&o.attrs), "unordered" | "ordered" | "checklist")
                            && matches!(tag(tuple), "unordered" | "ordered" | "checklist")))
                    {
                        return Err(EditorError::UnsupportedTarget);
                    }
                    m::set(&stage, 7, id, tuple.clone());
                }
                EditorCommand::Check {
                    target: id,
                    checked,
                } => {
                    let o = target(&stage, stable, id)?;
                    let p = target(&stage, stable, &o.parent)?;
                    if o.kind != "block"
                        || !matches!(tag(&o.attrs), "paragraph" | "heading" | "code")
                        || tag(&p.attrs) != "checklist"
                    {
                        return Err(EditorError::UnsupportedTarget);
                    }
                    m::set(
                        &stage,
                        8,
                        id,
                        m::arr(vec![Any::from("check"), Any::Bool(*checked)]),
                    );
                }
                EditorCommand::Enable {
                    target: id,
                    enabled,
                } => {
                    let t = stage.transact();
                    if !t.get_map(m::ROOTS[0]).unwrap().contains_key(&t, id)
                        && !t.get_map(m::ROOTS[5]).unwrap().contains_key(&t, id)
                    {
                        return Err(EditorError::UnsupportedTarget);
                    }
                    drop(t);
                    m::set(
                        &stage,
                        9,
                        id,
                        m::arr(vec![Any::from("enabled"), Any::Bool(*enabled)]),
                    );
                }
            }
            crate::editor::view(&stage, stable).map_err(error)?;
        }
        let view = crate::editor::view(&stage, stable).map_err(error)?;
        if let Some(last) = last_separator {
            if !view.owners.iter().any(|o| {
                !o.deleted
                    && o.kind == "block"
                    && matches!(tag(&o.attrs), "paragraph" | "heading" | "code")
            }) {
                let at = view
                    .owners
                    .iter()
                    .position(|o| o.id == last)
                    .ok_or(EditorError::UnsupportedTarget)?;
                let parent = &view.owners[at].parent;
                let before = view
                    .owners
                    .iter()
                    .skip(at + 1)
                    .find(|o| !o.deleted && o.parent == *parent)
                    .map(|o| o.id.as_str());
                let t = stage.transact();
                let max = t
                    .get_map(m::ROOTS[0])
                    .unwrap()
                    .keys(&t)
                    .filter_map(|s| m::id(s, "n").ok())
                    .filter(|(w, _)| *w == self.writer_id())
                    .map(|(_, c)| c)
                    .max()
                    .unwrap_or(0);
                drop(t);
                let id = format!(
                    "n:{}:{}",
                    self.writer_id(),
                    max.checked_add(1).ok_or(EditorError::ResourceLimit)?
                );
                register(
                    &stage,
                    self.writer_id(),
                    &id,
                    "block",
                    "",
                    &m::arr(vec![Any::from("paragraph")]),
                    stable,
                )?;
                let op = format!("o:{}:{counter}", self.writer_id());
                counter = counter.checked_add(1).ok_or(EditorError::ResourceLimit)?;
                crate::editor::place(&stage, stable, &op, &id, parent, before).map_err(error)?;
            }
        }
        crate::editor::view(&stage, stable).map_err(error)?;
        let hierarchy = crate::projection::ProjectionIndex::with_identity(&stage, self.identity())
            .read()
            .map_err(|_| EditorError::InvalidCommand)?;
        if let Some(caret) = &result_caret {
            match crate::anchors::resolve(&stage.transact(), self.identity(), &hierarchy, caret) {
                crate::anchors::Resolution::Position(p)
                    if Some(&p.node) == result_owner.as_ref() => {}
                _ => return Err(EditorError::InvalidCommand),
            }
        }
        if let Some(selection) = &result_selection {
            for endpoint in [&selection.anchor, &selection.focus] {
                if !matches!(
                    crate::anchors::resolve(
                        &stage.transact(),
                        self.identity(),
                        &hierarchy,
                        endpoint
                    ),
                    crate::anchors::Resolution::Position(_)
                ) {
                    return Err(EditorError::InvalidCommand);
                }
            }
        }
        super::lists::validate_editor_hierarchy(&hierarchy, &[]).map_err(|e| {
            if matches!(
                e,
                super::lists::HierarchyError::ResourceLimit
                    | super::lists::HierarchyError::DepthLimit(_)
            ) {
                EditorError::ResourceLimit
            } else {
                EditorError::InvalidCommand
            }
        })?;
        crate::document::validate_immutable_structure(&self.doc, &stage, stable)
            .map_err(EditorError::Admission)?;
        if m::bytes(&stage).len() > MAX_RESTORE_BYTES {
            return Err(EditorError::ResourceLimit);
        }
        let update = stage.transact().encode_state_as_update_v1(&sv);
        Ok(PreparedEditorEdit {
            identity: self.identity().clone(),
            writer: self.writer_id(),
            revision: self.revision(),
            baseline,
            update,
            origin: origin.clone(),
            next_operation: counter,
            result_caret,
            result_selection,
            entry_selection,
        })
    }
    fn virtual_document(&self, stage: &yrs::Doc) -> Result<Document, EditorError> {
        Document::restore(
            self.identity(),
            &crate::document::InitializedCheckpoint {
                identity: self.identity().clone(),
                initialized: true,
                writer_id: self.writer_id(),
                bytes: m::bytes(stage),
            },
            &[],
        )
        .map_err(EditorError::Admission)
    }
    pub fn commit_editor(&mut self, plan: PreparedEditorEdit) -> Result<EditorCommit, EditorError> {
        if self.identity() != &plan.identity || self.writer_id() != plan.writer {
            return Err(EditorError::Text(TextError::WrongReplica));
        }
        if self.revision() != plan.revision || self.checkpoint().bytes != plan.baseline {
            return Err(EditorError::Text(TextError::StalePlan));
        }
        let revision = self
            .revision()
            .checked_add(1)
            .ok_or(EditorError::ResourceLimit)?;
        let update = crate::ownership::decode(&plan.update).map_err(error)?;
        if update.is_empty() {
            return Ok(EditorCommit {
                result_caret: plan.result_caret,
                result_selection: plan.result_selection,
                revision: self.revision(),
                origin: plan.origin,
                update: ReplayUpdate {
                    identity: self.identity().clone(),
                    bytes: plan.update,
                },
            });
        }
        let patch_before = self
            .patch_before()
            .map_err(|e| EditorError::Text(TextError::from(e)))?;
        self.history_begin(&plan.origin, false);
        self.doc
            .transact_mut_with(plan.origin.as_str())
            .apply_update(update)
            .expect("validated editor update");
        self.history_bookmarks(
            plan.entry_selection,
            plan.result_selection.clone().or_else(|| {
                plan.result_caret
                    .clone()
                    .map(|a| crate::anchors::Selection {
                        anchor: a.clone(),
                        focus: a,
                    })
            }),
        );
        self.history_finish(plan.update.len());
        self.next_structure_counter = plan.next_operation;
        self.revision = revision;
        let commit = EditorCommit {
            result_caret: plan.result_caret,
            result_selection: plan.result_selection,
            revision,
            origin: plan.origin,
            update: ReplayUpdate {
                identity: self.identity().clone(),
                bytes: plan.update,
            },
        };
        self.patch_finish(
            patch_before,
            crate::patches::PatchOperation::Editor,
            commit.origin.as_str(),
            Some(commit.update.clone()),
        );
        Ok(commit)
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use crate::document::{CreationPermit, InitialNode};
    use std::sync::{Arc, Mutex};
    use yrs::Origin;
    #[test]
    fn v4_whole_batch_preparation_is_silent_commit_is_one_origin_and_only_local_writer() {
        let identity = DocumentIdentity::v4(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaa26",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbb26",
        )
        .unwrap();
        let mut d = Document::create(
            CreationPermit::authorize_new(identity).unwrap(),
            &[InitialNode {
                kind: "block".into(),
                text: "".into(),
            }],
        )
        .unwrap();
        let id = d.snapshot().unwrap().visible[0].id.clone();
        let observed = Arc::new(Mutex::new(vec![]));
        let seen = observed.clone();
        d.doc
            .observe_update_v1("v4-origin", move |t, e| {
                seen.lock()
                    .unwrap()
                    .push((t.origin().cloned(), e.update.clone()))
            })
            .unwrap();
        let count = Arc::new(Mutex::new(0));
        let counted = count.clone();
        d.doc
            .observe_after_transaction("v4-count", move |_| *counted.lock().unwrap() += 1)
            .unwrap();
        *count.lock().unwrap() = 0;
        let origin = TextOrigin::new("v4:whole-batch").unwrap();
        let before = d.checkpoint().bytes;
        let invalid = [
            EditorCommand::Block {
                target: id.clone(),
                tuple: m::arr(vec![Any::from("separator")]),
            },
            EditorCommand::Block {
                target: "missing".into(),
                tuple: m::arr(vec![Any::from("paragraph")]),
            },
        ];
        assert!(d.prepare_editor(&invalid, &origin).is_err());
        assert_eq!(d.checkpoint().bytes, before);
        assert_eq!(*count.lock().unwrap(), 0);
        assert!(observed.lock().unwrap().is_empty());
        let p = d
            .prepare_editor(
                &[EditorCommand::Block {
                    target: id,
                    tuple: m::arr(vec![Any::from("separator")]),
                }],
                &origin,
            )
            .unwrap();
        assert_eq!(*count.lock().unwrap(), 0);
        assert!(observed.lock().unwrap().is_empty());
        d.commit_editor(p).unwrap();
        assert_eq!(*count.lock().unwrap(), 1);
        let updates = observed.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, Some(Origin::from(origin.as_str())));
        assert_eq!(
            d.read()
                .state_vector()
                .iter()
                .map(|(id, _)| id.get() as u32)
                .collect::<Vec<_>>(),
            vec![d.writer_id()]
        );
    }
    #[test]
    fn v4_block_text_batch_is_silent_and_commits_once_with_only_local_writer() {
        let identity = DocumentIdentity::v4(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaa27",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbb27",
        )
        .unwrap();
        let mut d = Document::create(
            CreationPermit::authorize_new(identity).unwrap(),
            &[InitialNode {
                kind: "block".into(),
                text: "".into(),
            }],
        )
        .unwrap();
        let id = d.snapshot().unwrap().visible[0].id.clone();
        let observed = Arc::new(Mutex::new(vec![]));
        let seen = observed.clone();
        d.doc
            .observe_update_v1("v4-origin", move |t, e| {
                seen.lock()
                    .unwrap()
                    .push((t.origin().cloned(), e.update.clone()))
            })
            .unwrap();
        let count = Arc::new(Mutex::new(0));
        let counted = count.clone();
        d.doc
            .observe_after_transaction("v4-count", move |_| *counted.lock().unwrap() += 1)
            .unwrap();
        *count.lock().unwrap() = 0;
        let origin = TextOrigin::new("v4:whole-batch").unwrap();
        let before = d.checkpoint().bytes;
        let invalid = [
            EditorCommand::Block {
                target: id.clone(),
                tuple: m::arr(vec![Any::from("separator")]),
            },
            EditorCommand::Block {
                target: "missing".into(),
                tuple: m::arr(vec![Any::from("paragraph")]),
            },
        ];
        assert!(d.prepare_editor(&invalid, &origin).is_err());
        assert_eq!(d.checkpoint().bytes, before);
        assert_eq!(*count.lock().unwrap(), 0);
        assert!(observed.lock().unwrap().is_empty());
        let at = d
            .anchor(&id, crate::Utf16Offset(0), crate::anchors::Bias::Right)
            .unwrap();
        let p = d
            .prepare_blocks(
                &[
                    super::super::blocks::BlockCommand::SetCode {
                        owner: id.clone(),
                        language: "rust".into(),
                    },
                    super::super::blocks::BlockCommand::Text(
                        super::super::text::TextCommand::Insert {
                            at: at.clone(),
                            text: "literal".into(),
                        },
                    ),
                    super::super::blocks::BlockCommand::HardBreak { at },
                    super::super::blocks::BlockCommand::InsertSeparator {
                        beside: id,
                        side: super::super::blocks::Side::After,
                    },
                ],
                &origin,
            )
            .unwrap();
        assert_eq!(*count.lock().unwrap(), 0);
        assert!(observed.lock().unwrap().is_empty());
        d.commit_editor(p).unwrap();
        assert_eq!(*count.lock().unwrap(), 1);
        let updates = observed.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, Some(Origin::from(origin.as_str())));
        assert_eq!(
            d.read()
                .state_vector()
                .iter()
                .map(|(id, _)| id.get() as u32)
                .collect::<Vec<_>>(),
            vec![d.writer_id()]
        );
    }
    #[test]
    fn v5_witnessed_cross_source_literal_conversion_is_silent_then_commits_once() {
        use super::super::blocks::BlockCommand;
        use super::super::text::TextCommand;
        use crate::Utf16Offset;
        use crate::anchors::{Bias, Selection};
        let identity = DocumentIdentity::v5(
            "cccccccc-cccc-4ccc-8ccc-cccccccccc55",
            "dddddddd-dddd-4ddd-8ddd-dddddddddd55",
        )
        .unwrap();
        let mut d = Document::create(
            CreationPermit::authorize_new(identity).unwrap(),
            &[
                InitialNode {
                    kind: "block".into(),
                    text: "ab".into(),
                },
                InitialNode {
                    kind: "block".into(),
                    text: "cd".into(),
                },
            ],
        )
        .unwrap();
        let snapshot = d.snapshot().unwrap();
        let a = snapshot.visible[0].id.clone();
        let b = snapshot.visible[1].id.clone();
        let selection = Selection {
            anchor: d.anchor(&a, Utf16Offset(1), Bias::Right).unwrap(),
            focus: d.anchor(&b, Utf16Offset(1), Bias::Left).unwrap(),
        };
        let origin = TextOrigin::new("v5:whole-witnessed-batch").unwrap();
        let observed = Arc::new(Mutex::new(vec![]));
        let seen = observed.clone();
        d.doc
            .observe_update_v1("v5-origin", move |t, e| {
                seen.lock()
                    .unwrap()
                    .push((t.origin().cloned(), e.update.clone()))
            })
            .unwrap();
        let count = Arc::new(Mutex::new(0));
        let counted = count.clone();
        d.doc
            .observe_after_transaction("v5-count", move |_| *counted.lock().unwrap() += 1)
            .unwrap();
        *count.lock().unwrap() = 0;
        let commands = [
            BlockCommand::SetCode {
                owner: a.clone(),
                language: "rs".into(),
            },
            BlockCommand::SetCode {
                owner: b.clone(),
                language: "rs".into(),
            },
            BlockCommand::Join {
                first: a.clone(),
                second: b,
            },
            BlockCommand::Text(TextCommand::Replace {
                selection,
                text: "\n\n".into(),
            }),
            BlockCommand::SetParagraph { owner: a.clone() },
        ];
        let before = d.checkpoint().bytes;
        let mut bad = commands.to_vec();
        bad.push(BlockCommand::SetCode {
            owner: a.clone(),
            language: "invalid space".into(),
        });
        assert!(d.prepare_blocks(&bad, &origin).is_err());
        assert_eq!(d.checkpoint().bytes, before);
        assert_eq!(*count.lock().unwrap(), 0);
        assert!(observed.lock().unwrap().is_empty());
        let plan = d.prepare_blocks(&commands, &origin).unwrap();
        assert_eq!(d.checkpoint().bytes, before);
        assert_eq!(*count.lock().unwrap(), 0);
        assert!(observed.lock().unwrap().is_empty());
        d.commit_blocks(plan).unwrap();
        assert_eq!(*count.lock().unwrap(), 1);
        let updates = observed.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, Some(Origin::from(origin.as_str())));
        let n = d
            .snapshot()
            .unwrap()
            .visible
            .into_iter()
            .find(|n| n.id == a)
            .unwrap();
        assert_eq!(n.spans.len(), 2);
        assert_eq!(
            n.runs.iter().map(|r| r.text.as_str()).collect::<String>(),
            "a\n\nd"
        );
        assert!(
            n.runs
                .iter()
                .filter(|r| r.text.contains('\n'))
                .all(|r| r.attributes.is_empty())
        );
        assert_eq!(
            d.read()
                .state_vector()
                .iter()
                .map(|(id, _)| id.get() as u32)
                .collect::<Vec<_>>(),
            vec![d.writer_id()]
        );
    }
}
