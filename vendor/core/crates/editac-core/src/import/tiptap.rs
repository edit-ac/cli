//! Scoped Tiptap JSON snapshot conversion, not ProseMirror/Yjs schema compatibility.
//!
//! V1 supports doc, paragraph, heading, codeBlock, horizontalRule, blockquote,
//! bulletList/orderedList/listItem, taskList/taskItem, text and hardBreak;
//! bold/italic/strike/underline/code/link marks. Unknown blocks are inert atoms;
//! unknown inline nodes use a literal source fallback. Every unsupported field,
//! attribute or mark is diagnosed. The exact bounded input is retained separately
//! for recovery, including information not represented in the semantic document.
//! Hosts must persist it and the old checkpoint before publishing the new session.
use crate::{
    Utf16Offset,
    anchors::Bias,
    commands::{
        editor::{EditorError, FragmentBlock, FragmentRun},
        format::{Mark, normalize_link_url},
        lists::ContainerStyle,
        text::TextOrigin,
    },
    document::{
        BlockAttributes, CreationPermit, Document, DocumentError, DocumentIdentity, InitialNode,
    },
};
use serde_json::Value;
use yrs::{Any, Map};

pub const CONVERTER_VERSION: u32 = 1;
pub const MAX_INPUT_BYTES: usize = 1_048_576;
pub const MAX_SOURCE_BYTES: usize = 4096;
pub const MAX_DIAGNOSTIC_BYTES: usize = 65_536;
const MAX_NODES: usize = 2048;
const MAX_DEPTH: usize = 16;
const MAX_BLOCKS: usize = 126; // reserve the fragment boundary's two atom guards
const MAX_RUNS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSession {
    pub document_id: String,
    pub epoch: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: String,
    pub reason: String,
    /// UTF-8 bounded JSON preview; exact input remains in PreparedMigration.
    pub source: String,
    pub truncated: bool,
}
#[derive(Debug)]
pub enum MigrationError {
    UnsupportedConverter(u32),
    InvalidJson,
    InvalidContent(String),
    ResourceLimit,
    SameEpoch,
    Admission(DocumentError),
    Semantic(EditorError),
}

/// Immutable conversion result. It cannot target an existing replica.
#[derive(Debug)]
pub struct PreparedMigration {
    original: String,
    source_session: SourceSession,
    diagnostics: Vec<Diagnostic>,
    blocks: Vec<FragmentBlock>,
}
impl PreparedMigration {
    pub fn converter_version(&self) -> u32 {
        CONVERTER_VERSION
    }
    pub fn original_json(&self) -> &str {
        &self.original
    }
    pub fn source_session(&self) -> &SourceSession {
        &self.source_session
    }
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
    pub fn semantic_blocks(&self) -> &[FragmentBlock] {
        &self.blocks
    }

    /// The host explicitly assigns a new epoch for the same logical document.
    /// Returns a single new replica only after the complete staged commit succeeds.
    /// Creation authority is one-shot; restore the returned checkpoint thereafter.
    /// No source-session update bytes are consumed by this operation.
    pub fn start_session(&self, new_epoch: &str) -> Result<Document, MigrationError> {
        if new_epoch == self.source_session.epoch {
            return Err(MigrationError::SameEpoch);
        }
        let identity = DocumentIdentity::v5(&self.source_session.document_id, new_epoch)
            .map_err(MigrationError::Admission)?;
        let permit = CreationPermit::authorize_new(identity).map_err(MigrationError::Admission)?;
        let mut document = Document::create(
            permit,
            &[InitialNode {
                kind: "block".into(),
                text: String::new(),
            }],
        )
        .map_err(MigrationError::Admission)?;
        let snapshot = document
            .snapshot()
            .map_err(|_| MigrationError::InvalidContent("projection".into()))?;
        let owner = snapshot
            .visible
            .iter()
            .find(|n| n.kind == "block")
            .ok_or_else(|| MigrationError::InvalidContent("initial owner".into()))?;
        let selection = document
            .selection(
                (&owner.id, Utf16Offset(0), Bias::Left),
                (&owner.id, Utf16Offset(0), Bias::Right),
            )
            .map_err(|_| MigrationError::InvalidContent("initial selection".into()))?;
        let origin = TextOrigin::new("import:tiptap:v1").expect("constant valid origin");
        // Clipboard's opaque insertion intentionally accepts registered content
        // descriptors only. Migration first stages safe paragraphs through that
        // canonical harness, then admits generic unknown presentation tuples on
        // another disposable replica using the unchanged schema/replay validator.
        let mut blocks = self.blocks.clone();
        let atom = |b: &FragmentBlock| {
            matches!(
                b.attributes,
                Some(BlockAttributes::Unknown(_) | BlockAttributes::Separator)
            )
        };
        if blocks.first().is_some_and(atom) {
            blocks.insert(0, empty_block());
        }
        if blocks.last().is_some_and(atom) {
            blocks.push(empty_block());
        }
        let opaque: Vec<_> = blocks
            .iter()
            .enumerate()
            .filter_map(|(i, block)| {
                if let Some(BlockAttributes::Unknown(tuple)) = &block.attributes {
                    Some((i, tuple.clone()))
                } else {
                    None
                }
            })
            .collect();
        for block in &mut blocks {
            if matches!(block.attributes, Some(BlockAttributes::Unknown(_))) {
                block.attributes = Some(BlockAttributes::Paragraph);
            }
        }
        let plan = document
            .prepare_fragment(&selection, &blocks, &origin)
            .map_err(MigrationError::Semantic)?;
        document
            .commit_editor(plan)
            .map_err(MigrationError::Semantic)?;
        if !opaque.is_empty() {
            let snapshot = document
                .snapshot()
                .map_err(|_| MigrationError::InvalidContent("projection".into()))?;
            let owners: Vec<_> = snapshot
                .visible
                .iter()
                .filter(|n| n.kind == "block")
                .collect();
            if owners.len() != blocks.len() {
                return invalid("$", "staged owner count mismatch");
            }
            let mut stage = Document::restore(document.identity(), &document.checkpoint(), &[])
                .map_err(MigrationError::Admission)?;
            {
                let metadata = stage
                    .doc
                    .get_or_insert_map(crate::document::BLOCK_ATTRIBUTES);
                let mut txn = stage.write("import:tiptap:v1:opaque");
                for (index, tuple) in opaque {
                    metadata.insert(&mut txn, owners[index].id.as_str(), tuple);
                }
            }
            // Exact identity, JSON shape/bounds, immutable originals and operation
            // graph are admitted before any staged data reaches the result replica.
            let update = crate::document::ReplayUpdate {
                identity: document.identity().clone(),
                bytes: stage
                    .update_v1(&document.state_vector_v1())
                    .map_err(MigrationError::Admission)?,
            };
            document
                .apply_replay(&update)
                .map_err(MigrationError::Admission)?;
        }
        Ok(document)
    }
}

/// Parse and validate bounded snapshot content without touching any live document.
pub fn prepare_json(
    input: &str,
    version: u32,
    source: SourceSession,
) -> Result<PreparedMigration, MigrationError> {
    if version != CONVERTER_VERSION {
        return Err(MigrationError::UnsupportedConverter(version));
    }
    if input.len() > MAX_INPUT_BYTES {
        return Err(MigrationError::ResourceLimit);
    }
    DocumentIdentity::v5(&source.document_id, &source.epoch).map_err(MigrationError::Admission)?;
    let value: Value = serde_json::from_str(input).map_err(|_| MigrationError::InvalidJson)?;
    let mut converter = Converter {
        diagnostics: vec![],
        diagnostic_bytes: 0,
        next_container: 0,
        blocks: vec![],
    };
    bound_tree(&value, 0, &mut 0)?;
    if node_type(&value, "$")? != "doc" {
        return invalid("$", "expected doc");
    }
    converter.audit(&value, "$", &["type", "content", "attrs"], &[])?;
    for (i, node) in children(&value, "$")?.iter().enumerate() {
        converter.block(node, &format!("$.content[{i}]"), &[], None)?;
    }
    if converter.blocks.is_empty() {
        converter.blocks.push(empty_block());
    }
    if converter.blocks.len() > MAX_BLOCKS
        || converter.blocks.iter().map(|b| b.runs.len()).sum::<usize>() > MAX_RUNS
        || converter
            .blocks
            .iter()
            .flat_map(|b| &b.runs)
            .map(|r| r.text.len())
            .sum::<usize>()
            > MAX_INPUT_BYTES
    {
        return Err(MigrationError::ResourceLimit);
    }
    Ok(PreparedMigration {
        original: input.into(),
        source_session: source,
        diagnostics: converter.diagnostics,
        blocks: converter.blocks,
    })
}
fn invalid<T>(path: &str, reason: &str) -> Result<T, MigrationError> {
    Err(MigrationError::InvalidContent(format!("{path}: {reason}")))
}
fn bound_tree(value: &Value, depth: usize, count: &mut usize) -> Result<(), MigrationError> {
    *count += 1;
    // JSON object/array wrappers count too, providing a stricter bounded walk.
    if depth > 64 || *count > MAX_NODES * 16 {
        return Err(MigrationError::ResourceLimit);
    }
    match value {
        Value::Array(items) => {
            for item in items {
                bound_tree(item, depth + 1, count)?;
            }
        }
        Value::Object(fields) => {
            for item in fields.values() {
                bound_tree(item, depth + 1, count)?;
            }
        }
        _ => (),
    }
    Ok(())
}
fn node_type<'a>(value: &'a Value, path: &str) -> Result<&'a str, MigrationError> {
    value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| MigrationError::InvalidContent(format!("{path}: missing node type")))
}
fn children<'a>(value: &'a Value, path: &str) -> Result<&'a [Value], MigrationError> {
    match value.get("content") {
        None => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        _ => invalid(path, "content must be an array"),
    }
}
fn empty_block() -> FragmentBlock {
    FragmentBlock {
        runs: vec![],
        attributes: Some(BlockAttributes::Paragraph),
        containers: vec![],
        checked: None,
    }
}
struct Converter {
    diagnostics: Vec<Diagnostic>,
    diagnostic_bytes: usize,
    next_container: u32,
    blocks: Vec<FragmentBlock>,
}
impl Converter {
    fn diagnostic(
        &mut self,
        path: &str,
        reason: &str,
        value: &Value,
    ) -> Result<String, MigrationError> {
        let mut source = value.to_string();
        let truncated = source.len() > MAX_SOURCE_BYTES;
        if truncated {
            let mut end = MAX_SOURCE_BYTES;
            while !source.is_char_boundary(end) {
                end -= 1;
            }
            source.truncate(end);
        }
        self.diagnostic_bytes += source.len() + path.len() + reason.len();
        if self.diagnostic_bytes > MAX_DIAGNOSTIC_BYTES || self.diagnostics.len() >= 256 {
            return Err(MigrationError::ResourceLimit);
        }
        self.diagnostics.push(Diagnostic {
            path: path.into(),
            reason: reason.into(),
            source: source.clone(),
            truncated,
        });
        Ok(source)
    }
    fn audit(
        &mut self,
        value: &Value,
        path: &str,
        fields: &[&str],
        attrs: &[&str],
    ) -> Result<(), MigrationError> {
        let object = value
            .as_object()
            .ok_or_else(|| MigrationError::InvalidContent(format!("{path}: object required")))?;
        for (key, entry) in object {
            if !fields.contains(&key.as_str()) {
                self.diagnostic(&format!("{path}.{key}"), "unsupported field", entry)?;
            }
        }
        if let Some(attributes) = object.get("attrs").filter(|v| !v.is_null()) {
            let attributes = attributes.as_object().ok_or_else(|| {
                MigrationError::InvalidContent(format!("{path}.attrs: object required"))
            })?;
            for (key, entry) in attributes {
                if !attrs.contains(&key.as_str()) {
                    self.diagnostic(
                        &format!("{path}.attrs.{key}"),
                        "unsupported attribute",
                        entry,
                    )?;
                }
            }
        }
        Ok(())
    }
    fn block(
        &mut self,
        node: &Value,
        path: &str,
        containers: &[(String, ContainerStyle)],
        checked: Option<bool>,
    ) -> Result<(), MigrationError> {
        if containers.len() > MAX_DEPTH || self.blocks.len() >= MAX_BLOCKS {
            return Err(MigrationError::ResourceLimit);
        }
        let kind = node_type(node, path)?;
        match kind {
            "blockquote" | "bulletList" | "orderedList" | "taskList" => {
                if containers.len() == MAX_DEPTH {
                    return Err(MigrationError::ResourceLimit);
                }
                self.audit(
                    node,
                    path,
                    &["type", "content", "attrs"],
                    if kind == "orderedList" {
                        &["start"]
                    } else {
                        &[]
                    },
                )?;
                let style = match kind {
                    "blockquote" => ContainerStyle::Quote,
                    "bulletList" => ContainerStyle::Unordered,
                    "taskList" => ContainerStyle::Checklist,
                    _ => {
                        let start = match node.pointer("/attrs/start") {
                            None | Some(Value::Null) => 1,
                            Some(value) => value
                                .as_u64()
                                .filter(|n| (1..=2_147_483_647).contains(n))
                                .ok_or_else(|| {
                                    MigrationError::InvalidContent(format!(
                                        "{path}: invalid list start"
                                    ))
                                })? as u32,
                        };
                        ContainerStyle::Ordered { start }
                    }
                };
                let content = children(node, path)?;
                if content.is_empty() {
                    return self.unknown_block(node, path, containers, checked, "empty container");
                }
                if kind != "blockquote" {
                    for item in content {
                        let direct = children(item, path)?
                            .iter()
                            .filter(|block| {
                                !matches!(
                                    block.get("type").and_then(Value::as_str),
                                    Some("bulletList" | "orderedList" | "taskList" | "blockquote")
                                )
                            })
                            .count();
                        if direct != 1 {
                            // An opaque child does not participate in shared list
                            // numbering. Preserve the whole unsupported list so
                            // later items never acquire guessed ordinal semantics.
                            return self.unknown_block(
                                node,
                                path,
                                containers,
                                checked,
                                "unsupported multi-block list item",
                            );
                        }
                    }
                }
                let mut nested = containers.to_vec();
                self.next_container += 1;
                nested.push((format!("tiptap:container:{}", self.next_container), style));
                for (i, item) in content.iter().enumerate() {
                    let child_path = format!("{path}.content[{i}]");
                    if kind == "blockquote" {
                        self.block(item, &child_path, &nested, None)?;
                        continue;
                    }
                    let expected = if kind == "taskList" {
                        "taskItem"
                    } else {
                        "listItem"
                    };
                    if node_type(item, &child_path)? != expected {
                        return invalid(&child_path, "wrong list item type");
                    }
                    self.audit(
                        item,
                        &child_path,
                        &["type", "content", "attrs"],
                        if expected == "taskItem" {
                            &["checked"]
                        } else {
                            &[]
                        },
                    )?;
                    let item_checked = if expected == "taskItem" {
                        Some(match item.pointer("/attrs/checked") {
                            None => false,
                            Some(value) => value.as_bool().ok_or_else(|| {
                                MigrationError::InvalidContent(format!(
                                    "{child_path}: invalid checked"
                                ))
                            })?,
                        })
                    } else {
                        None
                    };
                    let blocks = children(item, &child_path)?;
                    if blocks.is_empty() {
                        return invalid(&child_path, "empty list item");
                    }
                    for (j, block) in blocks.iter().enumerate() {
                        self.block(
                            block,
                            &format!("{child_path}.content[{j}]"),
                            &nested,
                            item_checked,
                        )?;
                    }
                }
            }
            "paragraph" | "heading" | "codeBlock" | "horizontalRule" => {
                self.audit(
                    node,
                    path,
                    &["type", "content", "attrs"],
                    match kind {
                        "heading" => &["level"],
                        "codeBlock" => &["language"],
                        _ => &[],
                    },
                )?;
                let attributes = match kind {
                    "heading" => BlockAttributes::Heading(
                        node.pointer("/attrs/level")
                            .and_then(Value::as_u64)
                            .filter(|n| (1..=6).contains(n))
                            .ok_or_else(|| {
                                MigrationError::InvalidContent(format!(
                                    "{path}: invalid heading level"
                                ))
                            })? as u8,
                    ),
                    "codeBlock" => {
                        let language = match node.pointer("/attrs/language") {
                            None | Some(Value::Null) => "",
                            Some(value) => value.as_str().ok_or_else(|| {
                                MigrationError::InvalidContent(format!("{path}: invalid language"))
                            })?,
                        };
                        if language.len() > 64
                            || !language
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"+._-".contains(&b))
                        {
                            return invalid(path, "invalid language");
                        }
                        BlockAttributes::Code(language.into())
                    }
                    "horizontalRule" => BlockAttributes::Separator,
                    _ => BlockAttributes::Paragraph,
                };
                let content = children(node, path)?;
                if kind == "horizontalRule" && !content.is_empty() {
                    return invalid(path, "separator content");
                }
                let mut runs = vec![];
                for (i, inline) in content.iter().enumerate() {
                    runs.push(self.inline(
                        inline,
                        &format!("{path}.content[{i}]"),
                        kind == "codeBlock",
                    )?);
                }
                self.blocks.push(FragmentBlock {
                    runs,
                    attributes: Some(attributes),
                    containers: containers.to_vec(),
                    checked,
                });
            }
            _ => self.unknown_block(node, path, containers, checked, "unsupported block")?,
        }
        Ok(())
    }
    fn unknown_block(
        &mut self,
        node: &Value,
        path: &str,
        containers: &[(String, ContainerStyle)],
        checked: Option<bool>,
        reason: &str,
    ) -> Result<(), MigrationError> {
        let source = self.diagnostic(path, reason, node)?;
        let truncated = self.diagnostics.last().unwrap().truncated;
        self.blocks.push(FragmentBlock {
            runs: vec![FragmentRun {
                text: source.clone(),
                marks: vec![],
            }],
            attributes: Some(BlockAttributes::Unknown(Any::Array(
                vec![
                    Any::from("tiptap.v1"),
                    Any::from(source),
                    Any::Bool(truncated),
                ]
                .into(),
            ))),
            containers: containers.to_vec(),
            checked,
        });
        Ok(())
    }
    fn inline(
        &mut self,
        node: &Value,
        path: &str,
        code_block: bool,
    ) -> Result<FragmentRun, MigrationError> {
        let kind = node_type(node, path)?;
        if !matches!(kind, "text" | "hardBreak") {
            let source = self.diagnostic(path, "unsupported inline node", node)?;
            return Ok(FragmentRun {
                text: source,
                marks: vec![],
            });
        }
        self.audit(
            node,
            path,
            if kind == "text" {
                &["type", "text", "marks", "attrs"]
            } else {
                &["type", "marks", "attrs"]
            },
            &[],
        )?;
        let text = if kind == "hardBreak" {
            "\n"
        } else {
            node.get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| MigrationError::InvalidContent(format!("{path}: missing text")))?
        };
        let mut marks = vec![];
        if let Some(items) = node.get("marks") {
            let items = items.as_array().ok_or_else(|| {
                MigrationError::InvalidContent(format!("{path}: marks must be array"))
            })?;
            for (i, mark) in items.iter().enumerate() {
                let mark_path = format!("{path}.marks[{i}]");
                let kind = node_type(mark, &mark_path)?;
                let mapped = match kind {
                    "bold" => Mark::Bold,
                    "italic" => Mark::Italic,
                    "strike" => Mark::Strike,
                    "underline" => Mark::Underline,
                    "code" => Mark::Code,
                    "link" => Mark::Link,
                    _ => {
                        self.diagnostic(&mark_path, "unsupported mark", mark)?;
                        continue;
                    }
                };
                self.audit(
                    mark,
                    &mark_path,
                    &["type", "attrs"],
                    if mapped == Mark::Link { &["href"] } else { &[] },
                )?;
                let value = if mapped == Mark::Link {
                    let raw = mark
                        .pointer("/attrs/href")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            MigrationError::InvalidContent(format!("{mark_path}: missing href"))
                        })?;
                    match normalize_link_url(raw) {
                        Ok(url) => Some(url),
                        Err(_) => {
                            self.diagnostic(&mark_path, "unsafe link omitted", mark)?;
                            continue;
                        }
                    }
                } else {
                    None
                };
                if code_block {
                    self.diagnostic(&mark_path, "code block mark omitted", mark)?;
                    continue;
                }
                if marks.iter().any(|(m, _)| *m == mapped) {
                    return invalid(&mark_path, "duplicate mark");
                }
                marks.push((mapped, value));
            }
        }
        if marks.iter().any(|(m, _)| *m == Mark::Code) && marks.len() > 1 {
            return invalid(path, "code excludes rich marks");
        }
        Ok(FragmentRun {
            text: text.into(),
            marks,
        })
    }
}
