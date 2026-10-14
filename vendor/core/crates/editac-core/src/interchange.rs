//! Bounded semantic fragment JSON interchange. No parser or writable document.
use crate::{
    commands::{
        editor::{FragmentBlock, FragmentRun},
        format::Mark,
        lists::ContainerStyle,
    },
    document::BlockAttributes,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use yrs::Any;
const MAX_REQUEST: usize = 2 * 1024 * 1024;
#[derive(Debug, Serialize)]
pub struct Error {
    pub code: &'static str,
    pub operation: &'static str,
    pub message: String,
}
pub type Result<T> = std::result::Result<T, Error>;
#[inline(never)]
pub fn err(code: &'static str, operation: &'static str, message: impl Into<String>) -> Error {
    Error {
        code,
        operation,
        message: message.into(),
    }
}
pub fn kernel(operation: &'static str, e: impl std::fmt::Debug) -> Error {
    err("EngineRejected", operation, format!("{e:?}"))
}
pub fn parse<T: DeserializeOwned>(s: &str, op: &'static str) -> Result<T> {
    if s.len() > MAX_REQUEST {
        return Err(err("ResourceLimit", op, "request exceeds byte limit"));
    }
    serde_json::from_str(s).map_err(|e| err("InvalidInput", op, e.to_string()))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferBlock {
    runs: Vec<TransferRun>,
    attributes: Option<TransferAttributes>,
    #[serde(default)]
    containers: Vec<TransferContainer>,
    checked: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferContainer {
    key: String,
    style: Style,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRun {
    text: String,
    marks: Vec<TransferMark>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferMark {
    mark: MarkName,
    href: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum TransferAttributes {
    Paragraph {},
    Heading { level: u8 },
    Code { language: String },
    Separator {},
    Content { tuple: Value },
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Style {
    Unordered {},
    Ordered { start: u32 },
    Checklist {},
    Quote {},
}
impl Style {
    pub fn native(self) -> ContainerStyle {
        match self {
            Self::Unordered {} => ContainerStyle::Unordered,
            Self::Ordered { start } => ContainerStyle::Ordered { start },
            Self::Checklist {} => ContainerStyle::Checklist,
            Self::Quote {} => ContainerStyle::Quote,
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkName {
    Bold,
    Italic,
    Strike,
    Underline,
    Code,
    Link,
}
impl MarkName {
    pub fn native(self) -> Mark {
        match self {
            Self::Bold => Mark::Bold,
            Self::Italic => Mark::Italic,
            Self::Strike => Mark::Strike,
            Self::Underline => Mark::Underline,
            Self::Code => Mark::Code,
            Self::Link => Mark::Link,
        }
    }
}
pub fn transfer_blocks(blocks: Vec<TransferBlock>) -> Result<Vec<FragmentBlock>> {
    if blocks.len() > 128
        || blocks.iter().map(|b| b.runs.len()).sum::<usize>() > 1024
        || blocks
            .iter()
            .flat_map(|b| &b.runs)
            .map(|r| r.text.len())
            .sum::<usize>()
            > 1024 * 1024
        || blocks
            .iter()
            .any(|b| b.containers.len() > 16 || b.runs.iter().any(|r| r.marks.len() > 6))
    {
        return Err(err("ResourceLimit", "markdown", "fragment exceeds limits"));
    }
    let converted = blocks
        .into_iter()
        .map(|b| {
            Ok(FragmentBlock {
                containers: b
                    .containers
                    .into_iter()
                    .map(|c| (c.key, c.style.native()))
                    .collect(),
                checked: b.checked,
                runs: b
                    .runs
                    .into_iter()
                    .map(|r| FragmentRun {
                        text: r.text,
                        marks: r
                            .marks
                            .into_iter()
                            .map(|m| (m.mark.native(), m.href))
                            .collect(),
                    })
                    .collect(),
                attributes: b
                    .attributes
                    .map(|a| match a {
                        TransferAttributes::Paragraph {} => Ok(BlockAttributes::Paragraph),
                        TransferAttributes::Heading { level } => {
                            Ok(BlockAttributes::Heading(level))
                        }
                        TransferAttributes::Code { language } => {
                            Ok(BlockAttributes::Code(language))
                        }
                        TransferAttributes::Separator {} => Ok(BlockAttributes::Separator),
                        TransferAttributes::Content { tuple } => {
                            let a = Any::from_json(&tuple.to_string())
                                .map_err(|e| kernel("markdown", e))?;
                            let content = crate::modules::content::Content::from_tuple(&a)
                                .map_err(|e| kernel("markdown", e))?;
                            if content.is_none() {
                                return Err(err(
                                    "InvalidInput",
                                    "markdown",
                                    "unregistered content",
                                ));
                            }
                            Ok(BlockAttributes::Unknown(a))
                        }
                    })
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    crate::commands::fragment::validate(&converted).map_err(|e| kernel("markdown", e))?;
    Ok(converted)
}
pub fn transfer_value(b: &FragmentBlock) -> Value {
    let attributes = match &b.attributes {
        None => Value::Null,
        Some(BlockAttributes::Paragraph) => json!({"type":"paragraph"}),
        Some(BlockAttributes::Heading(level)) => json!({"type":"heading","level":level}),
        Some(BlockAttributes::Code(language)) => json!({"type":"code","language":language}),
        Some(BlockAttributes::Separator) => json!({"type":"separator"}),
        Some(BlockAttributes::Unknown(tuple)) => json!({"type":"content","tuple":value(tuple)}),
    };
    json!({"attributes":attributes,"checked":b.checked,
        "containers":b.containers.iter().map(|(key,style)| {
            let style=match style {ContainerStyle::Unordered=>json!({"type":"unordered"}),ContainerStyle::Checklist=>json!({"type":"checklist"}),ContainerStyle::Quote=>json!({"type":"quote"}),ContainerStyle::Ordered{start}=>json!({"type":"ordered","start":start})};
            json!({"key":key,"style":style})
        }).collect::<Vec<_>>(),
        "runs":b.runs.iter().map(|r|json!({"text":r.text,"marks":r.marks.iter().map(|(m,href)|{
            let name=match m {Mark::Bold=>"bold",Mark::Italic=>"italic",Mark::Strike=>"strike",Mark::Underline=>"underline",Mark::Code=>"code",Mark::Link=>"link"};json!({"mark":name,"href":href})
        }).collect::<Vec<_>>()})).collect::<Vec<_>>()})
}
pub fn value(a: &Any) -> Value {
    match a {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(b) => json!(b),
        Any::Number(n) => {
            if let Some(i) = n.as_i64() {
                json!(i)
            } else {
                json!(n)
            }
        }
        Any::String(s) => json!(s.as_ref()),
        Any::Array(a) => Value::Array(a.iter().map(value).collect()),
        Any::Map(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), value(v))).collect()),
        Any::Buffer(b) => json!(b.as_ref()),
    }
}
