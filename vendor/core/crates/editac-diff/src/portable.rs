//! Versioned companion presentation data; never an edit or attribution capability.
use crate::{
    semantic::{MODEL_VERSION, SemanticDiff},
    text::{PatchLimits, TextPatch},
};
use serde::{Deserialize, Serialize};

pub const VERSION: &str = "editac-change-model-v1";
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "representation", deny_unknown_fields)]
pub enum ChangeInput {
    #[serde(rename = "exact-source")]
    ExactSource { wire: String },
    #[serde(rename = "projected-markdown")]
    ProjectedMarkdown {
        wire: String,
        serializer: String,
        dialect: String,
    },
    #[serde(rename = "semantic")]
    Semantic { model: SemanticDiff },
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeModel {
    pub version: String,
    pub input: ChangeInput,
}
#[derive(Debug, PartialEq, Eq)]
pub enum ExportError {
    Limit,
    Invalid,
}
impl ChangeModel {
    pub fn new(input: ChangeInput) -> Result<Self, ExportError> {
        let model = Self {
            version: VERSION.into(),
            input,
        };
        model.validate()?;
        Ok(model)
    }
    fn validate(&self) -> Result<(), ExportError> {
        if self.version != VERSION {
            return Err(ExportError::Invalid);
        }
        match &self.input {
            ChangeInput::ExactSource { wire } | ChangeInput::ProjectedMarkdown { wire, .. } => {
                if let ChangeInput::ProjectedMarkdown {
                    serializer,
                    dialect,
                    ..
                } = &self.input
                {
                    if serializer.is_empty() || dialect.is_empty() {
                        return Err(ExportError::Invalid);
                    }
                }
                TextPatch::decode(wire.as_bytes(), PatchLimits::default())
                    .map_err(|_| ExportError::Invalid)?;
            }
            ChangeInput::Semantic { model } => {
                let hash = |s: &str| {
                    s.len() == 64
                        && s.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                };
                if model.version != MODEL_VERSION
                    || model.attribution != "unknown"
                    || !hash(&model.before_hash)
                    || !hash(&model.after_hash)
                {
                    return Err(ExportError::Invalid);
                }
                let mut ids = std::collections::BTreeSet::new();
                for c in &model.changes {
                    if c.id.is_empty()
                        || !ids.insert(&c.id)
                        || c.facets.is_empty()
                        || c.facets.windows(2).any(|w| w[0] >= w[1])
                        || (c.before.is_none() && c.after.is_none())
                        || c.before.iter().chain(c.after.iter()).any(|n| n.id != c.id)
                    {
                        return Err(ExportError::Invalid);
                    }
                }
            }
        }
        Ok(())
    }
    pub fn encode(&self, max_bytes: usize) -> Result<Vec<u8>, ExportError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| ExportError::Invalid)?;
        if max_bytes == 0 || max_bytes > MAX_BYTES || bytes.len() > max_bytes {
            return Err(ExportError::Limit);
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Self, ExportError> {
        if max_bytes == 0 || max_bytes > MAX_BYTES || bytes.len() > max_bytes {
            return Err(ExportError::Limit);
        }
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| ExportError::Invalid)?;
        // SemanticDiff is also an internal comparison type. At this portable
        // boundary, never silently discard unsupported envelope/attribution data.
        if value["input"]["representation"] == "semantic" {
            let model = &value["input"]["model"];
            let keys_match = |value: &serde_json::Value, expected: &[&str]| {
                value.as_object().is_some_and(|object| {
                    object.len() == expected.len()
                        && expected.iter().all(|key| object.contains_key(*key))
                })
            };
            if !keys_match(
                model,
                &[
                    "version",
                    "before_hash",
                    "after_hash",
                    "attribution",
                    "changes",
                    "retained_structure",
                ],
            ) || !model["changes"].as_array().is_some_and(|changes| {
                changes
                    .iter()
                    .all(|change| keys_match(change, &["id", "facets", "before", "after"]))
            }) {
                return Err(ExportError::Invalid);
            }
        }
        let model: Self = serde_json::from_value(value).map_err(|_| ExportError::Invalid)?;
        model.validate()?;
        Ok(model)
    }
}
