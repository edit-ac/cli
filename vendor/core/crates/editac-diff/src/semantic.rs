//! Bounded, on-demand semantic review. No live mutation or author inference.
use crate::representation::*;
use editac_core::projection::{ProjectedNode, Snapshot};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use yrs::Any;

pub const MODEL_VERSION: &str = "editac-semantic-diff-v1";
#[derive(Clone, Copy, Debug)]
pub struct SemanticLimits {
    pub input: RepresentationLimits,
    pub max_changes: usize,
    pub max_output_bytes: usize,
}
impl Default for SemanticLimits {
    fn default() -> Self {
        Self {
            input: RepresentationLimits::default(),
            max_changes: 10_001,
            max_output_bytes: 40 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Facet {
    Insert,
    Delete,
    Visibility,
    Placement,
    Kind,
    Text,
    Formatting,
    References,
    Table,
    Attributes,
    UnknownModule,
    Sources,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeChange {
    pub id: String,
    pub facets: Vec<Facet>,
    pub before: Option<SemanticNode>,
    pub after: Option<SemanticNode>,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticDiff {
    pub version: String,
    pub before_hash: String,
    pub after_hash: String,
    pub attribution: String,
    pub changes: Vec<NodeChange>,
    pub retained_structure: Option<(Value, Value)>,
}

// Bound borrowed engine values before conversion; never silently normalize opaque data.
fn any_value(
    value: &Any,
    depth: usize,
    limits: RepresentationLimits,
    budget: &mut usize,
) -> Result<Value, RepresentationError> {
    if depth > limits.max_depth {
        return Err(RepresentationError::ResourceLimit);
    }
    charge(16, budget)?;
    Ok(match value {
        Any::Null => Value::Null,
        Any::Bool(v) => json!(v),
        Any::Number(v) if v.as_f64().is_some_and(f64::is_finite) => json!(v),
        Any::String(v) => {
            charge(v.len(), budget)?;
            json!(v.as_ref())
        }
        Any::Array(values) => Value::Array(
            values
                .iter()
                .map(|v| any_value(v, depth + 1, limits, budget))
                .collect::<Result<_, _>>()?,
        ),
        Any::Map(values) => {
            let mut result = serde_json::Map::new();
            for (key, value) in values.iter() {
                charge(key.len(), budget)?;
                result.insert(key.clone(), any_value(value, depth + 1, limits, budget)?);
            }
            Value::Object(result)
        }
        _ => return Err(RepresentationError::InvalidSemantic),
    })
}
fn charge(bytes: usize, budget: &mut usize) -> Result<(), RepresentationError> {
    *budget = budget
        .checked_sub(bytes)
        .ok_or(RepresentationError::ResourceLimit)?;
    Ok(())
}
fn project_node(
    node: &ProjectedNode,
    limits: RepresentationLimits,
    budget: &mut usize,
) -> Result<SemanticNode, RepresentationError> {
    charge(128, budget)?;
    for s in [&node.id, &node.kind, &node.parent] {
        charge(s.len(), budget)?;
    }
    let mut spans = Vec::new();
    for s in &node.spans {
        charge(
            32usize
                .saturating_add(s.source.len())
                .saturating_add(s.identity.len()),
            budget,
        )?;
        spans.push(SemanticSpan {
            source: s.source.clone(),
            identity: s.identity.clone(),
            start: s.start,
            end: s.end,
        });
    }
    let mut runs = Vec::new();
    for r in &node.runs {
        charge(16usize.saturating_add(r.text.len()), budget)?;
        let mut marks = serde_json::Map::new();
        for (k, v) in &r.attributes {
            charge(k.len(), budget)?;
            marks.insert(k.clone(), any_value(v, 0, limits, budget)?);
        }
        runs.push(SemanticRun {
            text: r.text.clone(),
            marks: Value::Object(marks),
        });
    }
    let attributes = match &node.block_attributes {
        Some(editac_core::document::BlockAttributes::Unknown(value)) => {
            any_value(value, 0, limits, budget)?
        }
        Some(a) => any_value(&a.tuple(), 0, limits, budget)?,
        None => Value::Null,
    };
    let editor = match &node.editor {
        Some(e) => {
            charge(e.version.len() + 64, budget)?;
            let container = e
                .container_attributes
                .as_ref()
                .map(|v| any_value(v, 0, limits, budget))
                .transpose()?;
            json!({"version":e.version,"container_attributes":container,"checked":e.checked,"number":e.number})
        }
        None => Value::Null,
    };
    // The exact opaque block tuple is already retained in attributes, not its fallback.
    Ok(SemanticNode {
        id: node.id.clone(),
        kind: node.kind.clone(),
        parent: node.parent.clone(),
        order: node.order,
        spans,
        runs,
        attributes,
        editor,
        opaque: Value::Null,
    })
}
/// Copy a read-only real engine projection into the immutable history representation.
/// The host supplies the observed revision scope at the same read boundary.
pub fn capture_projection(
    scope: RevisionScope,
    projection: &Snapshot,
    limits: RepresentationLimits,
) -> Result<ImmutableRevision, RepresentationError> {
    if projection
        .visible
        .len()
        .saturating_add(projection.recovery.len())
        > limits.max_nodes
    {
        return Err(RepresentationError::ResourceLimit);
    }
    let mut budget = limits.max_bytes;
    let mut project = |nodes: &[std::sync::Arc<ProjectedNode>]| {
        nodes
            .iter()
            .map(|n| project_node(n, limits, &mut budget))
            .collect::<Result<Vec<_>, _>>()
    };
    let visible = project(&projection.visible)?;
    let recovery = project(&projection.recovery)?;
    let retained_structure = projection
        .retained_structure
        .as_ref()
        .map(|v| any_value(v, 0, limits, &mut budget))
        .transpose()?
        .unwrap_or(Value::Null);
    ImmutableRevision::capture(
        scope,
        RevisionInput::Semantic(&SemanticSnapshot {
            visible,
            recovery,
            retained_structure,
        }),
        limits,
    )
}
fn decode(
    revision: &ImmutableRevision,
    limits: RepresentationLimits,
) -> Result<SemanticSnapshot, RepresentationError> {
    if revision.representation() != &Representation::SemanticV1 {
        return Err(RepresentationError::IncompatibleRepresentation);
    }
    if revision.bytes().len() > limits.max_bytes {
        return Err(RepresentationError::ResourceLimit);
    }
    let prefix = format!("{SEMANTIC_ENCODING}\n");
    let bytes = revision
        .bytes()
        .strip_prefix(prefix.as_bytes())
        .ok_or(RepresentationError::InvalidSemantic)?;
    let snapshot: SemanticSnapshot =
        serde_json::from_slice(bytes).map_err(|_| RepresentationError::InvalidSemantic)?;
    // Revalidate the stricter caller limits, including depth and node counts.
    ImmutableRevision::capture(
        revision.scope().clone(),
        RevisionInput::Semantic(&snapshot),
        limits,
    )?;
    Ok(snapshot)
}
fn text(node: &SemanticNode) -> String {
    node.runs.iter().map(|r| r.text.as_str()).collect()
}
fn mark_ranges(node: &SemanticNode, links: bool) -> Vec<(usize, usize, Value)> {
    let mut result: Vec<(usize, usize, Value)> = Vec::new();
    let mut at = 0;
    for run in &node.runs {
        let end = at + run.text.encode_utf16().count();
        let marks: serde_json::Map<_, _> = run
            .marks
            .as_object()
            .expect("validated marks")
            .iter()
            .filter(|(key, _)| (key.as_str() == "link") == links)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if at != end && !marks.is_empty() {
            let value = Value::Object(marks);
            if let Some(last) = result
                .last_mut()
                .filter(|last| last.1 == at && last.2 == value)
            {
                last.1 = end;
            } else {
                result.push((at, end, value));
            }
        }
        at = end;
    }
    result
}
fn module(node: &SemanticNode) -> Option<&str> {
    node.attributes.as_array()?.first()?.as_str()
}
// Built-in table payloads contain attributed cell runs inside the opaque envelope.
// Keep the full tuple while also exposing cell content and marks independently.
fn table_cells(node: &SemanticNode) -> Vec<SemanticNode> {
    if module(node) != Some("module:editac.table") {
        return Vec::new();
    }
    let mut cells = Vec::new();
    if let Some(rows) = node
        .attributes
        .get(2)
        .and_then(|v| v.get("rows"))
        .and_then(Value::as_array)
    {
        for (row, values) in rows.iter().enumerate() {
            if let Some(values) = values.as_array() {
                for (column, runs) in values.iter().enumerate() {
                    let mut cell = SemanticNode {
                        id: format!("{row}:{column}"),
                        kind: "cell".into(),
                        parent: node.id.clone(),
                        order: 0,
                        spans: Vec::new(),
                        runs: Vec::new(),
                        attributes: Value::Null,
                        editor: Value::Null,
                        opaque: Value::Null,
                    };
                    if let Some(runs) = runs.as_array() {
                        for run in runs {
                            if let Some(content) = run.get(0).and_then(Value::as_str) {
                                let mut marks = serde_json::Map::new();
                                if let Some(items) = run.get(1).and_then(Value::as_array) {
                                    for item in items {
                                        if let Some(key) = item.get(0).and_then(Value::as_str) {
                                            marks.insert(
                                                key.into(),
                                                item.get(1).cloned().unwrap_or(Value::Bool(true)),
                                            );
                                        }
                                    }
                                }
                                cell.runs.push(SemanticRun {
                                    text: content.into(),
                                    marks: Value::Object(marks),
                                });
                            }
                        }
                    }
                    cells.push(cell);
                }
            }
        }
    }
    cells
}
fn table_marks(cells: &[SemanticNode], links: bool) -> Vec<(&str, Vec<(usize, usize, Value)>)> {
    cells
        .iter()
        .filter_map(|c| {
            let ranges = mark_ranges(c, links);
            (!ranges.is_empty()).then_some((c.id.as_str(), ranges))
        })
        .collect()
}
fn unknown(node: &SemanticNode) -> bool {
    module(node).is_some_and(|m| {
        !matches!(
            m,
            "paragraph"
                | "heading"
                | "code"
                | "separator"
                | "module:editac.reference"
                | "module:editac.table"
                | "module:editac.image"
                | "module:editac.callout"
        )
    })
}
/// Stable-ID comparison; every facet is independent, including simultaneous moves/edits.
pub fn compare(
    before: &ImmutableRevision,
    after: &ImmutableRevision,
    limits: SemanticLimits,
) -> Result<SemanticDiff, RepresentationError> {
    before.content_changed(after)?;
    let left = decode(before, limits.input)?;
    let right = decode(after, limits.input)?;
    let index = |s: SemanticSnapshot| -> BTreeMap<String, (bool, SemanticNode)> {
        s.visible
            .into_iter()
            .map(|n| (n.id.clone(), (true, n)))
            .chain(s.recovery.into_iter().map(|n| (n.id.clone(), (false, n))))
            .collect()
    };
    let retained_structure = (left.retained_structure != right.retained_structure).then(|| {
        (
            left.retained_structure.clone(),
            right.retained_structure.clone(),
        )
    });
    let a = index(left);
    let b = index(right);
    let ids: BTreeSet<_> = a.keys().chain(b.keys()).collect();
    let mut changes = Vec::new();
    for id in ids {
        let old = a.get(id);
        let new = b.get(id);
        let mut facets = Vec::new();
        match (old, new) {
            (None, Some(_)) => facets.push(Facet::Insert),
            (Some(_), None) => facets.push(Facet::Delete),
            (Some((av, x)), Some((bv, y))) => {
                if av != bv {
                    facets.push(Facet::Visibility);
                }
                if (x.parent.as_str(), x.order) != (y.parent.as_str(), y.order) {
                    facets.push(Facet::Placement);
                }
                if x.kind != y.kind {
                    facets.push(Facet::Kind);
                }
                let xc = table_cells(x);
                let yc = table_cells(y);
                if text(x) != text(y)
                    || xc.iter().map(|c| (&c.id, text(c))).collect::<Vec<_>>()
                        != yc.iter().map(|c| (&c.id, text(c))).collect::<Vec<_>>()
                {
                    facets.push(Facet::Text);
                }
                if mark_ranges(x, false) != mark_ranges(y, false)
                    || x.editor != y.editor
                    || table_marks(&xc, false) != table_marks(&yc, false)
                {
                    facets.push(Facet::Formatting);
                }
                let attrs = x.attributes != y.attributes;
                if mark_ranges(x, true) != mark_ranges(y, true)
                    || table_marks(&xc, true) != table_marks(&yc, true)
                    || (attrs && [module(x), module(y)].contains(&Some("module:editac.reference")))
                {
                    facets.push(Facet::References);
                }
                if attrs && [module(x), module(y)].contains(&Some("module:editac.table"))
                    || ((x.kind.contains("table") || y.kind.contains("table")) && x != y)
                {
                    facets.push(Facet::Table);
                }
                if attrs {
                    facets.push(Facet::Attributes);
                }
                if x.opaque != y.opaque || (attrs && (unknown(x) || unknown(y))) {
                    facets.push(Facet::UnknownModule);
                }
                if x.spans != y.spans {
                    facets.push(Facet::Sources);
                }
            }
            _ => unreachable!(),
        }
        if !facets.is_empty() {
            if changes.len() >= limits.max_changes {
                return Err(RepresentationError::ResourceLimit);
            }
            facets.sort();
            changes.push(NodeChange {
                id: id.clone(),
                facets,
                before: old.map(|(_, n)| n.clone()),
                after: new.map(|(_, n)| n.clone()),
            });
        }
    }
    if retained_structure.is_some() && changes.len() >= limits.max_changes {
        return Err(RepresentationError::ResourceLimit);
    }
    let result = SemanticDiff {
        version: MODEL_VERSION.into(),
        before_hash: before.hash().hex(),
        after_hash: after.hash().hex(),
        attribution: "unknown".into(),
        changes,
        retained_structure,
    };
    struct Counter {
        bytes: usize,
        max: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(data.len())
                .filter(|n| *n <= self.max)
                .ok_or_else(|| std::io::Error::other("semantic output limit"))?;
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(
        &mut Counter {
            bytes: 0,
            max: limits.max_output_bytes,
        },
        &result,
    )
    .map_err(|_| RepresentationError::ResourceLimit)?;
    Ok(result)
}
