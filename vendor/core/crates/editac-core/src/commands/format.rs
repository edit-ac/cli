//! Prepared character and owner formatting over explicit v1/v2 profiles.
//!
//! Toggle removes a mark only when every selected character has the requested
//! value; otherwise it applies that value to the entire selection. Empty ranges
//! are no-ops (stored typing marks belong to a future input contract). Inline code
//! excludes the five other known marks: applying code clears them; applying rich
//! marks over code rejects the whole batch. Unknown attributes are retained.
//!
//! Link policy: absolute HTTP(S) and mailto only, no credentials, whitespace,
//! controls or backslashes, including percent-encoded versions. URL parsing
//! normalizes scheme/host and escapes. Relative links require a future explicit
//! base policy; HTML entity spellings are rejected rather than guessed/decoded.
//! V2 block actions operate on logical owner IDs, independent of text length.
//! Remove resets to paragraph. Toggle resets all targets to paragraph when all
//! have the requested presentation, otherwise applies it to all. Block actions
//! run in order after mark actions in the same validated, explicit-origin commit.
//! Formatting a split owner writes an explicit override, even when inherited.
use super::text::{MAX_TEXT_BYTES, TextCommit, TextError, TextOrigin, check_depth, position};
use crate::Utf16Offset;
use crate::anchors::{self, Selection, Unresolvable};
use crate::document::{
    BLOCK_ATTRIBUTES, BlockAttributes, Document, DocumentIdentity, MAX_RESTORE_BYTES, ROOTS,
    ReplayUpdate, parse_block_tuple,
};
use crate::projection::TouchedRange;
use std::collections::{BTreeMap, BTreeSet};
use yrs::types::Attrs;
use yrs::{Any, GetString, Map, Out, ReadTxn, StateVector, Text, XmlTextRef};

pub const MAX_FORMAT_COMMANDS: usize = 128;
pub const MAX_FORMAT_RUNS: usize = 8192;
pub const MAX_LINK_BYTES: usize = 2048;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Bold,
    Italic,
    Strike,
    Underline,
    Code,
    Link,
}
impl Mark {
    fn key(self) -> &'static str {
        match self {
            Self::Bold => "bold",
            Self::Italic => "italic",
            Self::Strike => "strike",
            Self::Underline => "underline",
            Self::Code => "code",
            Self::Link => "link",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatAction {
    Apply,
    Remove,
    Toggle,
}
#[derive(Clone, Debug)]
pub struct FormatCommand {
    pub selection: Selection,
    pub mark: Mark,
    pub action: FormatAction,
    /// Required for link apply/toggle; forbidden for all other operations.
    pub href: Option<String>,
}
/// A logical-owner selection includes empty blocks; duplicate IDs count once.
#[derive(Clone, Debug)]
pub struct BlockFormatCommand {
    pub targets: Vec<String>,
    pub attributes: BlockAttributes,
    pub action: FormatAction,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatError {
    Text(TextError),
    InvalidAttributes,
    InvalidUrl,
    CodeConflict,
    ResourceLimit,
    UnsupportedBlockProfile,
    UnsupportedBlockTarget,
    UnavailableBlockTarget,
}
impl From<TextError> for FormatError {
    fn from(e: TextError) -> Self {
        Self::Text(e)
    }
}
#[derive(Clone, Debug)]
struct Run {
    start: u32,
    end: u32,
    attrs: BTreeMap<String, Any>,
}
#[derive(Debug)]
struct Source {
    text: XmlTextRef,
    runs: Vec<Run>,
}
#[derive(Debug)]
struct Edit {
    text: XmlTextRef,
    source: String,
    start: u32,
    end: u32,
    attrs: Attrs,
}
/// Single-use plan. No mutable transaction is opened during preparation.
#[derive(Debug)]
pub struct PreparedFormatBatch {
    identity: DocumentIdentity,
    writer: u32,
    revision: u64,
    state: Vec<u8>,
    origin: TextOrigin,
    edits: Vec<Edit>,
    blocks: BTreeMap<String, Any>,
}
/// Shared inert-import URL admission. This does not fetch or mutate a document.
pub fn normalize_link_url(raw: &str) -> Result<String, FormatError> {
    if raw.len() > MAX_LINK_BYTES {
        return Err(FormatError::ResourceLimit);
    }
    if raw.is_empty()
        || raw
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(FormatError::InvalidUrl);
    }
    // Validate percent encodings before parsing; never let a browser normalize
    // hidden whitespace, encoded executable scheme delimiters or backslashes.
    let mut decoded = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3).ok_or(FormatError::InvalidUrl)?;
            decoded.push(u8::from_str_radix(hex, 16).map_err(|_| FormatError::InvalidUrl)?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = std::str::from_utf8(&decoded).map_err(|_| FormatError::InvalidUrl)?;
    if decoded
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        || decoded.contains("&#")
        || decoded.contains("&colon;")
        || decoded.contains("&Tab;")
        || decoded.contains("&NewLine;")
    {
        return Err(FormatError::InvalidUrl);
    }
    let parsed = url::Url::parse(raw).map_err(|_| FormatError::InvalidUrl)?;
    let inspected = url::Url::parse(decoded).map_err(|_| FormatError::InvalidUrl)?;
    if parsed.scheme() != inspected.scheme() {
        return Err(FormatError::InvalidUrl);
    }
    match parsed.scheme() {
        "http" | "https"
            if parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none() => {}
        "mailto" if !parsed.path().is_empty() && parsed.path().contains('@') => {}
        _ => return Err(FormatError::InvalidUrl),
    }
    let normalized = parsed.to_string();
    if normalized.len() > MAX_LINK_BYTES {
        return Err(FormatError::ResourceLimit);
    }
    Ok(normalized)
}

fn url(raw: &str) -> Result<String, FormatError> {
    normalize_link_url(raw)
}
fn value(command: &FormatCommand) -> Result<Option<Any>, FormatError> {
    match (command.mark, command.action, command.href.as_deref()) {
        (Mark::Link, FormatAction::Remove, None) => Ok(None),
        (Mark::Link, FormatAction::Apply | FormatAction::Toggle, Some(href)) => {
            Ok(Some(Any::from(url(href)?)))
        }
        (Mark::Link, _, _) => Err(FormatError::InvalidAttributes),
        (_, _, Some(_)) => Err(FormatError::InvalidAttributes),
        (_, FormatAction::Remove, None) => Ok(None),
        _ => Ok(Some(Any::Bool(true))),
    }
}
impl Document {
    pub fn can_format(
        &self,
        commands: &[FormatCommand],
        origin: &TextOrigin,
    ) -> Result<(), FormatError> {
        self.prepare_format(commands, origin).map(|_| ())
    }
    pub fn prepare_format(
        &self,
        commands: &[FormatCommand],
        origin: &TextOrigin,
    ) -> Result<PreparedFormatBatch, FormatError> {
        self.prepare_format_batch(commands, &[], origin)
    }
    pub fn can_format_batch(
        &self,
        marks: &[FormatCommand],
        blocks: &[BlockFormatCommand],
        origin: &TextOrigin,
    ) -> Result<(), FormatError> {
        self.prepare_format_batch(marks, blocks, origin).map(|_| ())
    }
    /// Validate both axes before opening a live mutable transaction.
    pub fn prepare_format_batch(
        &self,
        commands: &[FormatCommand],
        blocks: &[BlockFormatCommand],
        origin: &TextOrigin,
    ) -> Result<PreparedFormatBatch, FormatError> {
        if commands.len().saturating_add(blocks.len()) > MAX_FORMAT_COMMANDS {
            return Err(FormatError::ResourceLimit);
        }
        let values = commands.iter().map(value).collect::<Result<Vec<_>, _>>()?;
        let snapshot = self.inspect().map_err(TextError::from)?;
        let txn = self.read();
        let state = txn.encode_state_as_update_v1(&StateVector::default());
        if state.len() > MAX_RESTORE_BYTES {
            return Err(FormatError::ResourceLimit);
        }
        let texts = txn
            .get_map(ROOTS[1])
            .ok_or(TextError::Unresolvable(Unresolvable::MissingSource))?;
        let mut sources: BTreeMap<String, Source> = BTreeMap::new();
        let mut edits = Vec::new();
        let mut budget = state.len();
        for (command, value) in commands.iter().zip(values) {
            let a = position(anchors::resolve(
                &txn,
                self.identity(),
                &snapshot,
                &command.selection.anchor,
            ))?;
            let b = position(anchors::resolve(
                &txn,
                self.identity(),
                &snapshot,
                &command.selection.focus,
            ))?;
            let ai = snapshot
                .visible
                .iter()
                .position(|n| n.id == a.node)
                .ok_or(TextError::Unresolvable(Unresolvable::MissingOwner))?;
            let bi = snapshot
                .visible
                .iter()
                .position(|n| n.id == b.node)
                .ok_or(TextError::Unresolvable(Unresolvable::MissingOwner))?;
            let (start, end, first, last) = if (ai, a.offset) <= (bi, b.offset) {
                (a, b, ai, bi)
            } else {
                (b, a, bi, ai)
            };
            let mut ranges = Vec::new();
            for (i, node) in snapshot
                .visible
                .iter()
                .enumerate()
                .take(last + 1)
                .skip(first)
            {
                if self.identity().has_editor()
                    && matches!(node.kind.as_str(), "list" | "quote")
                    && node.spans.is_empty()
                {
                    continue;
                }
                if !self.identity().has_editor() {
                    check_depth(&snapshot, &node.id)?;
                }
                if !matches!(node.kind.as_str(), "list" | "item")
                    && !(node.kind == "block" && self.identity().has_blocks())
                {
                    return Err(TextError::UnsupportedKind.into());
                }
                if self.identity().has_editor()
                    && !matches!(
                        node.block_attributes,
                        Some(BlockAttributes::Paragraph | BlockAttributes::Heading(_))
                    )
                {
                    return Err(FormatError::UnsupportedBlockTarget);
                }
                let content: String = node.runs.iter().map(|r| r.text.as_str()).collect();
                if content.len() > MAX_TEXT_BYTES {
                    return Err(FormatError::ResourceLimit);
                }
                let lo = if i == first { start.offset.0 } else { 0 };
                let hi = if i == last {
                    end.offset.0
                } else {
                    content.encode_utf16().count() as u32
                };
                anchors::validate_range(&content, Utf16Offset(lo)..Utf16Offset(hi))
                    .map_err(TextError::from)?;
                let mut base = 0;
                for span in &node.spans {
                    let len = span.end - span.start;
                    let l = lo.max(base);
                    let h = hi.min(base + len);
                    if l < h {
                        let s = span.start + l - base;
                        let e = span.start + h - base;
                        if !sources.contains_key(&span.source) {
                            let Some(Out::YXmlText(text)) = texts.get(&txn, &span.source) else {
                                return Err(
                                    TextError::Unresolvable(Unresolvable::MissingSource).into()
                                );
                            };
                            if text.get_string(&txn).len() > MAX_TEXT_BYTES {
                                return Err(FormatError::ResourceLimit);
                            }
                            let mut at = 0;
                            let runs = text
                                .diff(&txn, |_| ())
                                .into_iter()
                                .map(|r| {
                                    let Out::Any(Any::String(string)) = r.insert else {
                                        return Err(FormatError::InvalidAttributes);
                                    };
                                    let len = string.encode_utf16().count() as u32;
                                    let run = Run {
                                        start: at,
                                        end: at + len,
                                        attrs: r
                                            .attributes
                                            .map(|a| {
                                                a.into_iter()
                                                    .map(|(k, v)| (k.to_string(), v))
                                                    .collect()
                                            })
                                            .unwrap_or_default(),
                                    };
                                    at += len;
                                    Ok(run)
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            sources.insert(span.source.clone(), Source { text, runs });
                        }
                        ranges.push((span.source.clone(), s, e));
                    }
                    base += len;
                }
            }
            let key = command.mark.key();
            let all = !ranges.is_empty()
                && ranges.iter().all(|(id, s, e)| {
                    sources[id]
                        .runs
                        .iter()
                        .filter(|r| r.start < *e && r.end > *s)
                        .all(|r| r.attrs.get(key) == value.as_ref())
                });
            let apply = command.action == FormatAction::Apply
                || (command.action == FormatAction::Toggle && !all);
            for (id, s, e) in ranges {
                let source = sources.get_mut(&id).unwrap();
                let mut next = Vec::new();
                for r in &source.runs {
                    if r.start >= e || r.end <= s {
                        next.push(r.clone());
                        continue;
                    }
                    let lo = r.start.max(s);
                    let hi = r.end.min(e);
                    if r.start < lo {
                        next.push(Run {
                            end: lo,
                            ..r.clone()
                        });
                    }
                    let mut attrs = r.attrs.clone();
                    if apply {
                        if command.mark != Mark::Code && attrs.get("code") == Some(&Any::Bool(true))
                        {
                            return Err(FormatError::CodeConflict);
                        }
                        if command.mark == Mark::Code {
                            for k in ["bold", "italic", "strike", "underline", "link"] {
                                attrs.remove(k);
                            }
                        }
                        attrs.insert(key.into(), value.clone().unwrap());
                    } else {
                        attrs.remove(key);
                    }
                    if attrs != r.attrs {
                        let mut delta = Attrs::new();
                        for k in r.attrs.keys().chain(attrs.keys()) {
                            if r.attrs.get(k) != attrs.get(k) {
                                delta.insert(
                                    k.as_str().into(),
                                    attrs.get(k).cloned().unwrap_or(Any::Null),
                                );
                            }
                        }
                        budget = budget
                            .checked_add(
                                128 + delta
                                    .values()
                                    .map(|v| match v {
                                        Any::String(s) => s.len(),
                                        _ => 8,
                                    })
                                    .sum::<usize>(),
                            )
                            .ok_or(FormatError::ResourceLimit)?;
                        if budget > MAX_RESTORE_BYTES || edits.len() >= MAX_FORMAT_RUNS {
                            return Err(FormatError::ResourceLimit);
                        }
                        edits.push(Edit {
                            text: source.text.clone(),
                            source: id.clone(),
                            start: lo,
                            end: hi,
                            attrs: delta,
                        });
                    }
                    next.push(Run {
                        start: lo,
                        end: hi,
                        attrs,
                    });
                    if hi < r.end {
                        next.push(Run {
                            start: hi,
                            ..r.clone()
                        });
                    }
                }
                source.runs = next;
            }
            if sources.values().map(|s| s.runs.len()).sum::<usize>() > MAX_FORMAT_RUNS {
                return Err(FormatError::ResourceLimit);
            }
        }
        let general = if self.identity().has_structure() {
            Some(
                self.general_view()
                    .map_err(|_| FormatError::UnsupportedBlockTarget)?,
            )
        } else {
            None
        };
        let mut block_edits = BTreeMap::new();
        if !blocks.is_empty() {
            if !self.identity().has_blocks() {
                return Err(FormatError::UnsupportedBlockProfile);
            }
            let metadata = txn
                .get_map(BLOCK_ATTRIBUTES)
                .ok_or(FormatError::UnsupportedBlockProfile)?;
            let mut target_count = 0usize;
            for command in blocks {
                if !matches!(
                    command.attributes,
                    BlockAttributes::Paragraph | BlockAttributes::Heading(1..=6)
                ) {
                    return Err(FormatError::InvalidAttributes);
                }
                target_count = target_count
                    .checked_add(command.targets.len())
                    .ok_or(FormatError::ResourceLimit)?;
                if target_count > MAX_FORMAT_RUNS {
                    return Err(FormatError::ResourceLimit);
                }
                let targets: BTreeSet<_> = command.targets.iter().collect();
                let mut all = !targets.is_empty();
                for id in &targets {
                    budget = budget
                        .checked_add(id.len() + 128)
                        .ok_or(FormatError::ResourceLimit)?;
                    if budget > MAX_RESTORE_BYTES {
                        return Err(FormatError::ResourceLimit);
                    }
                    let node = snapshot
                        .visible
                        .iter()
                        .find(|n| &n.id == *id)
                        .ok_or(FormatError::UnavailableBlockTarget)?;
                    if !self.identity().has_editor() {
                        check_depth(&snapshot, id)?;
                    }
                    if node.kind != "block"
                        || matches!(
                            node.block_attributes,
                            Some(BlockAttributes::Unknown(_)) | None
                        )
                    {
                        return Err(FormatError::UnsupportedBlockTarget);
                    }
                    let read = |key: &str| -> Result<Option<BlockAttributes>, FormatError> {
                        let value = block_edits.get(key).cloned().or_else(|| {
                            match metadata.get(&txn, key) {
                                Some(Out::Any(value)) => Some(value),
                                _ => None,
                            }
                        });
                        value
                            .as_ref()
                            .map(parse_block_tuple)
                            .transpose()
                            .map_err(|_| FormatError::InvalidAttributes)
                    };
                    let explicit = read(id)?;
                    let current = if self.identity().has_structure() {
                        let mut key = id.to_string();
                        let mut effective = explicit;
                        for _ in 0..=128 {
                            if effective.is_some() {
                                break;
                            }
                            let Some(parent) = general.as_ref().unwrap().inherited.get(&key) else {
                                break;
                            };
                            key = parent.clone();
                            effective = read(&key)?;
                        }
                        effective
                    } else if explicit.is_none() && id.as_str() == "block-split" {
                        read("block-1")?
                    } else {
                        explicit
                    }
                    .unwrap_or(BlockAttributes::Paragraph);
                    all &= current == command.attributes;
                }
                let desired = if command.action == FormatAction::Remove
                    || (command.action == FormatAction::Toggle && all)
                {
                    BlockAttributes::Paragraph
                } else {
                    command.attributes.clone()
                }
                .tuple();
                for id in targets {
                    let current =
                        block_edits
                            .get(id)
                            .cloned()
                            .or_else(|| match metadata.get(&txn, id) {
                                Some(Out::Any(value)) => Some(value),
                                _ => None,
                            });
                    // Do not materialize absent ordinary paragraph defaults. A
                    // derived owner's explicit override intentionally ends inheritance.
                    if current.as_ref() != Some(&desired)
                        && !(current.is_none()
                            && id != "block-split"
                            && !id.starts_with("o:")
                            && desired == BlockAttributes::Paragraph.tuple())
                    {
                        block_edits.insert(id.clone(), desired.clone());
                    }
                }
            }
        }
        if self.identity().has_structure() {
            self.validate_general_mutation(|t| {
                for edit in &edits {
                    let Some(Out::YXmlText(text)) = t
                        .get_map(crate::document::ROOTS[1])
                        .unwrap()
                        .get(t, &edit.source)
                    else {
                        unreachable!()
                    };
                    text.format(t, edit.start, edit.end - edit.start, edit.attrs.clone());
                }
                for (id, tuple) in &block_edits {
                    t.get_map(BLOCK_ATTRIBUTES)
                        .unwrap()
                        .insert(t, id.clone(), tuple.clone());
                }
            })
            .map_err(|_| FormatError::ResourceLimit)?;
        }
        Ok(PreparedFormatBatch {
            identity: self.identity().clone(),
            writer: self.writer_id(),
            revision: self.revision(),
            state,
            origin: origin.clone(),
            edits,
            blocks: block_edits,
        })
    }
    pub fn commit_format(&mut self, plan: PreparedFormatBatch) -> Result<TextCommit, FormatError> {
        if self.identity() != &plan.identity || self.writer_id() != plan.writer {
            return Err(TextError::WrongReplica.into());
        }
        if self.revision() != plan.revision || self.checkpoint().bytes != plan.state {
            return Err(TextError::StalePlan.into());
        }
        if plan.edits.is_empty() && plan.blocks.is_empty() {
            return Ok(TextCommit {
                revision: self.revision(),
                origin: plan.origin,
                update: None,
                touched_ranges: vec![],
            });
        }
        let revision = self
            .revision()
            .checked_add(1)
            .ok_or(FormatError::ResourceLimit)?;
        let mut touched_ranges = Vec::with_capacity(plan.edits.len());
        let update;
        let patch_before = self.patch_before().map_err(TextError::from)?;
        self.history_begin(&plan.origin, false);
        {
            let mut txn = self.write(plan.origin.as_str());
            for edit in plan.edits {
                edit.text
                    .format(&mut txn, edit.start, edit.end - edit.start, edit.attrs);
                touched_ranges.push(TouchedRange {
                    source: edit.source,
                    start: edit.start,
                    new_start: edit.start,
                    old_end: edit.end,
                    new_end: edit.end,
                });
            }
            if !plan.blocks.is_empty() {
                let metadata = txn
                    .get_map(BLOCK_ATTRIBUTES)
                    .expect("validated block profile");
                for (id, tuple) in plan.blocks {
                    metadata.insert(&mut txn, id, tuple);
                }
            }
            update = txn.encode_update_v1();
        }
        self.history_finish(update.len());
        self.revision = revision;
        let commit = TextCommit {
            revision,
            origin: plan.origin,
            update: Some(ReplayUpdate {
                identity: self.identity().clone(),
                bytes: update,
            }),
            touched_ranges,
        };
        self.patch_finish(
            patch_before,
            crate::patches::PatchOperation::Format,
            commit.origin.as_str(),
            commit.update.clone(),
        );
        Ok(commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::Bias;
    use crate::document::{CreationPermit, InitialNode};
    use std::sync::{Arc, Mutex};
    use yrs::Origin;
    #[test]
    fn complete_batch_has_one_origin_transaction_and_no_preparation_observers() {
        let identity = DocumentIdentity::v1(
            "aaaaaaaa-aaaa-4aaa-8aaa-000000006001",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        )
        .unwrap();
        let mut d = Document::create(
            CreationPermit::authorize_new(identity).unwrap(),
            &[
                InitialNode {
                    kind: "item".into(),
                    text: "abcd".into(),
                },
                InitialNode {
                    kind: "item".into(),
                    text: "efgh".into(),
                },
            ],
        )
        .unwrap();
        let nodes = d.snapshot().unwrap().visible;
        let handle = d.write("fixture:handle").doc().clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        handle
            .observe_update_v1("format-proof", move |txn, event| {
                observed
                    .lock()
                    .unwrap()
                    .push((txn.origin().cloned(), event.update.clone()));
            })
            .unwrap();
        let count = Arc::new(Mutex::new(0));
        let counted = count.clone();
        handle
            .observe_after_transaction("format-count", move |_| {
                *counted.lock().unwrap() += 1;
            })
            .unwrap();
        *count.lock().unwrap() = 0;
        let selection = d
            .selection(
                (&nodes[0].id, Utf16Offset(1), Bias::Right),
                (&nodes[1].id, Utf16Offset(3), Bias::Left),
            )
            .unwrap();
        let commands = [
            FormatCommand {
                selection: selection.clone(),
                mark: Mark::Bold,
                action: FormatAction::Apply,
                href: None,
            },
            FormatCommand {
                selection,
                mark: Mark::Italic,
                action: FormatAction::Apply,
                href: None,
            },
        ];
        let origin = TextOrigin::new("format:batch").unwrap();
        d.can_format(&commands, &origin).unwrap();
        let p = d.prepare_format(&commands, &origin).unwrap();
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(*count.lock().unwrap(), 0);
        let mut invalid = commands.to_vec();
        invalid[1].href = Some("javascript:alert(1)".into());
        assert!(d.can_format(&invalid, &origin).is_err());
        assert_eq!(*count.lock().unwrap(), 0);
        let result = d.commit_format(p).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(*count.lock().unwrap(), 1);
        assert_eq!(seen[0].0, Some(Origin::from(origin.as_str())));
        assert_eq!(seen[0].1, result.update.unwrap().bytes);
        assert_eq!(result.revision, 1);
        handle.unobserve_update_v1("format-proof").unwrap();
        handle.unobserve_after_transaction("format-count").unwrap();
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    use crate::anchors::Bias;
    use crate::document::{CreationPermit, InitialNode};
    use yrs::updates::decoder::Decode;
    use yrs::{Transact, Update};
    #[test]
    fn deletion_only_remote_update_invalidates_exact_state_even_without_revision_change() {
        let identity = DocumentIdentity::v1(
            "aaaaaaaa-aaaa-4aaa-8aaa-000000006002",
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        )
        .unwrap();
        let mut d = Document::create(
            CreationPermit::authorize_new(identity).unwrap(),
            &[InitialNode {
                kind: "item".into(),
                text: "abcd".into(),
            }],
        )
        .unwrap();
        let id = d.snapshot().unwrap().visible[0].id.clone();
        let c = FormatCommand {
            selection: d
                .selection(
                    (&id, Utf16Offset(0), Bias::Right),
                    (&id, Utf16Offset(4), Bias::Left),
                )
                .unwrap(),
            mark: Mark::Bold,
            action: FormatAction::Apply,
            href: None,
        };
        let p = d
            .prepare_format(&[c], &TextOrigin::new("format").unwrap())
            .unwrap();
        let peer = crate::new_document();
        for root in ROOTS {
            peer.get_or_insert_map(root);
        }
        peer.transact_mut()
            .apply_update(Update::decode_v1(&d.checkpoint().bytes).unwrap())
            .unwrap();
        let t = peer.transact();
        let Some(Out::YXmlText(x)) = t.get_map(ROOTS[1]).unwrap().get(&t, &id) else {
            panic!("text")
        };
        drop(t);
        let vector = d.read().state_vector();
        let revision = d.revision();
        x.remove_range(&mut peer.transact_mut(), 1, 1);
        let update = peer.transact().encode_state_as_update_v1(&vector);
        d.write("fixture:remote")
            .apply_update(Update::decode_v1(&update).unwrap())
            .unwrap();
        assert_eq!(d.read().state_vector(), vector);
        assert_eq!(d.revision(), revision);
        let before = d.checkpoint().bytes;
        assert_eq!(
            d.commit_format(p).unwrap_err(),
            FormatError::Text(TextError::StalePlan)
        );
        assert_eq!(d.checkpoint().bytes, before);
    }
}

#[cfg(test)]
mod block_transaction_tests {
    use super::*;
    use crate::anchors::Bias;
    use crate::document::{CreationPermit, InitialNode};
    use std::sync::{Arc, Mutex};
    use yrs::Origin;

    #[test]
    fn mixed_mark_and_empty_block_batch_has_one_origin_transaction_and_read_only_rejection() {
        let identity = DocumentIdentity::v2(
            "aaaaaaaa-aaaa-4aaa-8aaa-000000006003",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        )
        .unwrap();
        let mut d = Document::create(
            CreationPermit::authorize_new(identity).unwrap(),
            &[
                InitialNode {
                    kind: "block".into(),
                    text: "abcd".into(),
                },
                InitialNode {
                    kind: "block".into(),
                    text: "".into(),
                },
            ],
        )
        .unwrap();
        let nodes = d.snapshot().unwrap().visible;
        let handle = d.write("fixture:handle").doc().clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        handle
            .observe_update_v1("mixed-format-proof", move |txn, event| {
                observed
                    .lock()
                    .unwrap()
                    .push((txn.origin().cloned(), event.update.clone()));
            })
            .unwrap();
        let count = Arc::new(Mutex::new(0));
        let counted = count.clone();
        handle
            .observe_after_transaction("mixed-format-count", move |_| {
                *counted.lock().unwrap() += 1;
            })
            .unwrap();
        let marks = [FormatCommand {
            selection: d
                .selection(
                    (&nodes[0].id, Utf16Offset(1), Bias::Right),
                    (&nodes[0].id, Utf16Offset(3), Bias::Left),
                )
                .unwrap(),
            mark: Mark::Bold,
            action: FormatAction::Apply,
            href: None,
        }];
        let blocks = [BlockFormatCommand {
            targets: nodes.iter().map(|n| n.id.clone()).collect(),
            attributes: BlockAttributes::Heading(6),
            action: FormatAction::Apply,
        }];
        let origin = TextOrigin::new("format:mixed").unwrap();
        *count.lock().unwrap() = 0;
        let before = d.checkpoint().bytes;
        d.can_format_batch(&marks, &blocks, &origin).unwrap();
        let plan = d.prepare_format_batch(&marks, &blocks, &origin).unwrap();
        let mut invalid = blocks.clone();
        invalid[0].attributes = BlockAttributes::Heading(7);
        assert_eq!(
            d.prepare_format_batch(&marks, &invalid, &origin)
                .unwrap_err(),
            FormatError::InvalidAttributes
        );
        assert_eq!(d.checkpoint().bytes, before);
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(*count.lock().unwrap(), 0);
        let commit = d.commit_format(plan).unwrap();
        assert_eq!(commit.revision, 1);
        assert_eq!(*count.lock().unwrap(), 1);
        {
            let events = seen.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0, Some(Origin::from(origin.as_str())));
            assert_eq!(events[0].1, commit.update.unwrap().bytes);
        }
        assert!(
            d.snapshot()
                .unwrap()
                .visible
                .iter()
                .all(|n| n.block_attributes == Some(BlockAttributes::Heading(6)))
        );
        handle.unobserve_update_v1("mixed-format-proof").unwrap();
        handle
            .unobserve_after_transaction("mixed-format-count")
            .unwrap();
    }
}
