//! Inert v1 descriptors inside the existing opaque block tuple envelope.
use crate::commands::{
    editor::FragmentRun,
    format::{Mark, normalize_link_url},
};
use std::{collections::HashMap, sync::Arc};
use yrs::Any;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    None,
    Left,
    Center,
    Right,
}
#[derive(Clone, Debug)]
pub enum Content {
    /// Validated inert transfer envelope, independent of optional codecs/presenters.
    InertExtension {
        data: Any,
    },
    Reference {
        target_id: String,
        label: String,
    },
    Image {
        url: String,
        alt: String,
        title: String,
        source: String,
    },
    Callout {
        kind: String,
        body: String,
        source: String,
    },
    /// Bounded v2 cell blocks and structural metadata; renderers remain optional.
    RichTable {
        data: Any,
    },
    Table {
        alignments: Vec<Alignment>,
        rows: Vec<Vec<Vec<FragmentRun>>>,
        source: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentError {
    InvalidShape,
    ResourceLimit,
    UnsafeUrl,
    UnsupportedVersion,
}
/// Apply a native simple-cell text edit while retaining marks on unchanged runs.
/// Compare scalar boundaries, never slice inside a UTF-8/UTF-16 character.
pub(crate) fn edit_cell(runs: &mut Vec<FragmentRun>, text: &str) {
    let old: String = runs.iter().map(|r| r.text.as_str()).collect();
    let prefix = old
        .chars()
        .zip(text.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    let suffix = old[prefix..]
        .chars()
        .rev()
        .zip(text[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    let end = old.len() - suffix;
    let inserted = &text[prefix..text.len() - suffix];
    let marks = runs
        .iter()
        .scan(0usize, |at, r| {
            let start = *at;
            *at += r.text.len();
            Some((start, *at, r))
        })
        .find(|(_, stop, _)| *stop >= prefix)
        .map(|(_, _, r)| r.marks.clone())
        .unwrap_or_default();
    let mut result = Vec::new();
    let mut at = 0;
    for r in runs.iter() {
        let stop = at + r.text.len();
        if at < prefix {
            let n = (prefix - at).min(r.text.len());
            if n > 0 {
                result.push(FragmentRun {
                    text: r.text[..n].into(),
                    marks: r.marks.clone(),
                });
            }
        }
        at = stop;
    }
    if !inserted.is_empty() {
        result.push(FragmentRun {
            text: inserted.into(),
            marks: marks.clone(),
        });
    }
    at = 0;
    for r in runs.iter() {
        let stop = at + r.text.len();
        if stop > end {
            let start = end.saturating_sub(at);
            result.push(FragmentRun {
                text: r.text[start..].into(),
                marks: r.marks.clone(),
            });
        }
        at = stop;
    }
    if result.is_empty() {
        result.push(FragmentRun {
            text: String::new(),
            marks,
        });
    }
    let mut compact: Vec<FragmentRun> = Vec::new();
    for r in result {
        if let Some(last) = compact.last_mut().filter(|last| last.marks == r.marks) {
            last.text.push_str(&r.text);
        } else {
            compact.push(r);
        }
    }
    *runs = compact;
}
fn array(v: Vec<Any>) -> Any {
    Any::Array(v.into())
}
fn object(v: Vec<(&str, Any)>) -> Any {
    Any::Map(Arc::new(
        v.into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect::<HashMap<_, _>>(),
    ))
}
fn string(v: &Any) -> Result<String, ContentError> {
    if let Any::String(s) = v {
        Ok(s.to_string())
    } else {
        Err(ContentError::InvalidShape)
    }
}
fn mark_name(m: Mark) -> &'static str {
    match m {
        Mark::Bold => "bold",
        Mark::Italic => "italic",
        Mark::Strike => "strike",
        Mark::Underline => "underline",
        Mark::Code => "code",
        Mark::Link => "link",
    }
}
fn alignment_name(a: Alignment) -> &'static str {
    match a {
        Alignment::None => "none",
        Alignment::Left => "left",
        Alignment::Center => "center",
        Alignment::Right => "right",
    }
}
impl Content {
    pub fn inert_source(&self) -> Option<&str> {
        if let Self::InertExtension {
            data: Any::Map(data),
        } = self
        {
            if let Some(Any::String(source)) = data.get("source") {
                return Some(source);
            }
        }
        None
    }
    /// Exact inert v2 source, exposed without making consumers parse engine values.
    pub fn rich_table_source(&self) -> Option<&str> {
        if let Self::RichTable {
            data: Any::Map(data),
        } = self
        {
            if let Some(Any::String(source)) = data.get("source") {
                return Some(source.as_ref());
            }
        }
        None
    }
    pub fn validate(&self) -> Result<(), ContentError> {
        match self {
            Self::InertExtension { data } => super::inert_content::validate(data)?,
            Self::RichTable { data } => super::rich_table::validate(data)?,
            Self::Reference { target_id, label } => {
                if target_id.is_empty()
                    || label.is_empty()
                    || target_id.encode_utf16().count() > 512
                    || label.encode_utf16().count() > 512
                {
                    return Err(ContentError::ResourceLimit);
                }
                if target_id.chars().any(char::is_control) || label.chars().any(char::is_control) {
                    return Err(ContentError::InvalidShape);
                }
            }
            Self::Image {
                url,
                alt,
                title,
                source,
            } => {
                if alt.len() > 8192 || title.len() > 2048 || source.len() > 1_048_576 {
                    return Err(ContentError::ResourceLimit);
                }
                let url = normalize_link_url(url).map_err(|_| ContentError::UnsafeUrl)?;
                if !url.starts_with("https://") && !url.starts_with("http://") {
                    return Err(ContentError::UnsafeUrl);
                }
            }
            Self::Callout { kind, body, source } => {
                if !["NOTE", "TIP", "IMPORTANT", "WARNING", "CAUTION"].contains(&kind.as_str()) {
                    return Err(ContentError::InvalidShape);
                }
                if body.len() > 1_048_576 || source.len() > 1_048_576 {
                    return Err(ContentError::ResourceLimit);
                }
            }
            Self::Table {
                alignments,
                rows,
                source,
            } => {
                if alignments.is_empty()
                    || alignments.len() > 32
                    || rows.is_empty()
                    || rows.len() > 128
                    || source.len() > 1_048_576
                {
                    return Err(ContentError::ResourceLimit);
                }
                if rows.iter().any(|r| r.len() != alignments.len()) {
                    return Err(ContentError::InvalidShape);
                }
                let runs: Vec<_> = rows.iter().flatten().flatten().collect();
                if runs.len() > 1024 || runs.iter().map(|r| r.text.len()).sum::<usize>() > 1_048_576
                {
                    return Err(ContentError::ResourceLimit);
                }
                for run in runs {
                    if run.marks.len() > 6 || run.text.contains(['\n', '\r']) {
                        return Err(ContentError::InvalidShape);
                    }
                    for (mark, href) in &run.marks {
                        if *mark == Mark::Link {
                            normalize_link_url(href.as_deref().ok_or(ContentError::UnsafeUrl)?)
                                .map_err(|_| ContentError::UnsafeUrl)?;
                        } else if href.is_some() {
                            return Err(ContentError::InvalidShape);
                        }
                    }
                }
            }
        }
        Ok(())
    }
    pub fn tuple(&self) -> Result<Any, ContentError> {
        self.validate()?;
        if let Self::RichTable { data } = self {
            return Ok(array(vec![
                Any::from("module:editac.table"),
                Any::from(2u32),
                data.clone(),
            ]));
        }
        let (tag, data) = match self {
            Self::InertExtension { data } => ("module:editac.inert-extension", data.clone()),
            Self::RichTable { .. } => unreachable!(),
            Self::Reference { target_id, label } => (
                "module:editac.reference",
                object(vec![
                    ("targetId", Any::from(target_id.clone())),
                    ("label", Any::from(label.clone())),
                ]),
            ),
            Self::Image {
                url,
                alt,
                title,
                source,
            } => (
                "module:editac.image",
                object(vec![
                    ("url", Any::from(url.clone())),
                    ("alt", Any::from(alt.clone())),
                    ("title", Any::from(title.clone())),
                    ("source", Any::from(source.clone())),
                ]),
            ),
            Self::Callout { kind, body, source } => (
                "module:editac.callout",
                object(vec![
                    ("kind", Any::from(kind.clone())),
                    ("body", Any::from(body.clone())),
                    ("source", Any::from(source.clone())),
                ]),
            ),
            Self::Table {
                alignments,
                rows,
                source,
            } => (
                "module:editac.table",
                object(vec![
                    (
                        "alignments",
                        array(
                            alignments
                                .iter()
                                .map(|a| Any::from(alignment_name(*a)))
                                .collect(),
                        ),
                    ),
                    (
                        "rows",
                        array(
                            rows.iter()
                                .map(|row| {
                                    array(
                                        row.iter()
                                            .map(|cell| {
                                                array(
                                                    cell.iter()
                                                        .map(|run| {
                                                            array(vec![
                                                                Any::from(run.text.clone()),
                                                                array(
                                                                    run.marks
                                                                        .iter()
                                                                        .map(|(mark, href)| {
                                                                            let mut v =
                                                                                vec![Any::from(
                                                                                    mark_name(
                                                                                        *mark,
                                                                                    ),
                                                                                )];
                                                                            if let Some(href) = href
                                                                            {
                                                                                v.push(Any::from(
                                                                                    href.clone(),
                                                                                ));
                                                                            }
                                                                            array(v)
                                                                        })
                                                                        .collect(),
                                                                ),
                                                            ])
                                                        })
                                                        .collect(),
                                                )
                                            })
                                            .collect(),
                                    )
                                })
                                .collect(),
                        ),
                    ),
                    ("source", Any::from(source.clone())),
                ]),
            ),
        };
        Ok(array(vec![Any::from(tag), Any::from(1u32), data]))
    }
    pub fn from_tuple(value: &Any) -> Result<Option<Self>, ContentError> {
        let Any::Array(v) = value else {
            return Ok(None);
        };
        let Some(Any::String(tag)) = v.first() else {
            return Ok(None);
        };
        if !matches!(
            tag.as_ref(),
            "module:editac.image"
                | "module:editac.table"
                | "module:editac.callout"
                | "module:editac.reference"
                | "module:editac.inert-extension"
        ) {
            return Ok(None);
        }
        if v.len() != 3 {
            return Err(ContentError::InvalidShape);
        }
        if tag.as_ref() == "module:editac.table" && v[1] == Any::from(2u32) {
            let content = Self::RichTable { data: v[2].clone() };
            content.validate()?;
            return Ok(Some(content));
        }
        if v[1] != Any::from(1u32) {
            return Err(ContentError::UnsupportedVersion);
        }
        let Any::Map(data) = &v[2] else {
            return Err(ContentError::InvalidShape);
        };
        let get = |key: &str| data.get(key).ok_or(ContentError::InvalidShape);
        let content = if tag.as_ref() == "module:editac.inert-extension" {
            Self::InertExtension { data: v[2].clone() }
        } else if tag.as_ref() == "module:editac.reference" {
            if data.len() != 2 {
                return Err(ContentError::InvalidShape);
            }
            Self::Reference {
                target_id: string(get("targetId")?)?,
                label: string(get("label")?)?,
            }
        } else if tag.as_ref() == "module:editac.image" {
            if data.len() != 4 {
                return Err(ContentError::InvalidShape);
            }
            Self::Image {
                url: string(get("url")?)?,
                alt: string(get("alt")?)?,
                title: string(get("title")?)?,
                source: string(get("source")?)?,
            }
        } else if tag.as_ref() == "module:editac.callout" {
            if data.len() != 3 {
                return Err(ContentError::InvalidShape);
            }
            Self::Callout {
                kind: string(get("kind")?)?,
                body: string(get("body")?)?,
                source: string(get("source")?)?,
            }
        } else {
            if data.len() != 3 {
                return Err(ContentError::InvalidShape);
            }
            let Any::Array(alignments) = get("alignments")? else {
                return Err(ContentError::InvalidShape);
            };
            let alignments = alignments
                .iter()
                .map(|a| match string(a)?.as_str() {
                    "none" => Ok(Alignment::None),
                    "left" => Ok(Alignment::Left),
                    "center" => Ok(Alignment::Center),
                    "right" => Ok(Alignment::Right),
                    _ => Err(ContentError::InvalidShape),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let Any::Array(raw_rows) = get("rows")? else {
                return Err(ContentError::InvalidShape);
            };
            if alignments.len() > 32 || raw_rows.len() > 128 {
                return Err(ContentError::ResourceLimit);
            }
            let mut rows = Vec::new();
            let mut count = 0;
            for raw_row in raw_rows.iter() {
                let Any::Array(raw_cells) = raw_row else {
                    return Err(ContentError::InvalidShape);
                };
                if raw_cells.len() != alignments.len() {
                    return Err(ContentError::InvalidShape);
                }
                let mut row = Vec::new();
                for raw_cell in raw_cells.iter() {
                    let Any::Array(raw_runs) = raw_cell else {
                        return Err(ContentError::InvalidShape);
                    };
                    count += raw_runs.len();
                    if count > 1024 {
                        return Err(ContentError::ResourceLimit);
                    }
                    let mut cell = Vec::new();
                    for raw_run in raw_runs.iter() {
                        let Any::Array(run) = raw_run else {
                            return Err(ContentError::InvalidShape);
                        };
                        if run.len() != 2 {
                            return Err(ContentError::InvalidShape);
                        }
                        let text = string(&run[0])?;
                        let Any::Array(raw_marks) = &run[1] else {
                            return Err(ContentError::InvalidShape);
                        };
                        if raw_marks.len() > 6 {
                            return Err(ContentError::ResourceLimit);
                        }
                        let mut marks = Vec::new();
                        for raw_mark in raw_marks.iter() {
                            let Any::Array(mark) = raw_mark else {
                                return Err(ContentError::InvalidShape);
                            };
                            let name = mark
                                .first()
                                .ok_or(ContentError::InvalidShape)
                                .and_then(string)?;
                            let kind = match name.as_str() {
                                "bold" => Mark::Bold,
                                "italic" => Mark::Italic,
                                "strike" => Mark::Strike,
                                "underline" => Mark::Underline,
                                "code" => Mark::Code,
                                "link" => Mark::Link,
                                _ => return Err(ContentError::InvalidShape),
                            };
                            let href = if kind == Mark::Link && mark.len() == 2 {
                                Some(string(&mark[1])?)
                            } else if kind != Mark::Link && mark.len() == 1 {
                                None
                            } else {
                                return Err(ContentError::InvalidShape);
                            };
                            marks.push((kind, href));
                        }
                        cell.push(FragmentRun { text, marks });
                    }
                    row.push(cell);
                }
                rows.push(row);
            }
            Self::Table {
                alignments,
                rows,
                source: string(get("source")?)?,
            }
        };
        content.validate()?;
        Ok(Some(content))
    }
}
