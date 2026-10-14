//! Explicitly registered image/table codecs. Disabled imports remain recoverable.
use crate::{
    FragmentRun,
    import::{Diagnostic, ImportError, PreparedContent, import},
    profile,
};
use editac_core::{
    commands::format::{Mark, normalize_link_url},
    document::BlockAttributes,
    modules::content::{Alignment, Content, ContentError},
};
use pulldown_cmark::{Event, Tag, TagEnd};

#[derive(Clone, Copy, Debug, Default)]
pub struct Codecs {
    pub callouts: bool,
    pub images: bool,
    pub tables: bool,
}
fn text(runs: &[FragmentRun]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

fn table(
    events: &[(Event<'_>, std::ops::Range<usize>)],
    source: String,
) -> Result<Content, &'static str> {
    let Event::Start(Tag::Table(raw_alignments)) = &events[0].0 else {
        return Err("table-shape");
    };
    let alignments = raw_alignments
        .iter()
        .map(|a| match a {
            pulldown_cmark::Alignment::None => Alignment::None,
            pulldown_cmark::Alignment::Left => Alignment::Left,
            pulldown_cmark::Alignment::Center => Alignment::Center,
            pulldown_cmark::Alignment::Right => Alignment::Right,
        })
        .collect();
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell: Vec<FragmentRun> = Vec::new();
    let mut marks = Vec::new();
    for (event, _) in &events[1..] {
        match event {
            Event::Start(Tag::TableHead | Tag::TableRow) => row = vec![],
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => rows.push(std::mem::take(&mut row)),
            Event::Start(Tag::TableCell) => {
                cell = vec![];
                marks = vec![];
            }
            Event::End(TagEnd::TableCell) => {
                if cell.is_empty() {
                    cell.push(FragmentRun {
                        text: String::new(),
                        marks: vec![],
                    });
                }
                row.push(std::mem::take(&mut cell));
            }
            Event::Start(Tag::Strong) => marks.push((Mark::Bold, None)),
            Event::Start(Tag::Emphasis) => marks.push((Mark::Italic, None)),
            Event::Start(Tag::Strikethrough) => marks.push((Mark::Strike, None)),
            Event::Start(Tag::Link {
                dest_url, title, ..
            }) => {
                if !title.is_empty() {
                    return Err("table-link-title");
                }
                marks.push((
                    Mark::Link,
                    Some(normalize_link_url(dest_url).map_err(|_| "table-unadmitted-link")?),
                ));
            }
            Event::End(
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link,
            ) => {
                marks.pop().ok_or("table-inline-shape")?;
            }
            Event::Text(s) | Event::Code(s) => {
                let mut active = marks.clone();
                if matches!(event, Event::Code(_)) {
                    active.push((Mark::Code, None));
                }
                if let Some(last) = cell.last_mut().filter(|r| r.marks == active) {
                    last.text.push_str(s);
                } else {
                    cell.push(FragmentRun {
                        text: s.to_string(),
                        marks: active,
                    });
                }
            }
            Event::End(TagEnd::Table) => break,
            _ => return Err("table-cell-unsupported"),
        }
    }
    let value = Content::Table {
        alignments,
        rows,
        source,
    };
    value.validate().map_err(|_| "table-limit-or-shape")?;
    Ok(value)
}

pub fn import_with_codecs(source: &str, codecs: Codecs) -> Result<PreparedContent, ImportError> {
    let mut result = import(source)?;
    let events = profile::events(source);
    for (i, (event, span)) in events.iter().enumerate() {
        let descriptor = match event {
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) if codecs.images => {
                let mut alt = String::new();
                let mut depth = 1;
                for (event, _) in &events[i + 1..] {
                    match event {
                        Event::Start(Tag::Image { .. }) => depth += 1,
                        Event::End(TagEnd::Image) => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        Event::Text(s) | Event::Code(s) => alt.push_str(s),
                        Event::SoftBreak | Event::HardBreak => alt.push('\n'),
                        _ => {}
                    }
                }
                Content::Image {
                    url: dest_url.to_string(),
                    alt,
                    title: title.to_string(),
                    source: source[span.clone()].into(),
                }
            }
            Event::Start(Tag::Table(_)) if codecs.tables => {
                match table(&events[i..], source[span.clone()].into()) {
                    Ok(content) => content,
                    Err("table-limit-or-shape") => return Err(ImportError::ResourceLimit),
                    Err(code) => {
                        result.diagnostics.push(Diagnostic {
                            code,
                            range: span.clone(),
                        });
                        continue;
                    }
                }
            }
            _ => continue,
        };
        let block = result
            .blocks
            .iter()
            .position(|b| text(&b.runs) == source[span.clone()]);
        let Some(block) = block else {
            result.diagnostics.push(Diagnostic {
                code: "inline-image-layout",
                range: span.clone(),
            });
            continue;
        };
        let tuple = match descriptor.tuple() {
            Ok(tuple) => tuple,
            Err(ContentError::ResourceLimit) => return Err(ImportError::ResourceLimit),
            Err(_) => {
                result.diagnostics.push(Diagnostic {
                    code: "optional-content-unadmitted",
                    range: span.clone(),
                });
                continue;
            }
        };
        let fallback = match &descriptor {
            Content::InertExtension { .. } => descriptor.inert_source().unwrap().to_owned(),
            Content::RichTable { .. } => descriptor.rich_table_source().unwrap().to_owned(),
            Content::Reference { label, .. } => label.clone(),
            Content::Callout { kind, body, .. } => format!("{kind}\n{body}"),
            Content::Image { alt, .. } => alt.clone(),
            Content::Table { rows, .. } => rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|runs| text(runs))
                        .collect::<Vec<_>>()
                        .join("\t")
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        result.blocks[block].attributes = Some(BlockAttributes::Unknown(tuple));
        result.blocks[block].runs = vec![FragmentRun {
            text: fallback,
            marks: vec![],
        }];
        result.diagnostics.retain(|d| {
            !(d.range == *span && matches!(d.code, "image-codec-disabled" | "table-codec-disabled"))
        });
    }
    apply_callouts(source, &events, codecs.callouts, &mut result)?;
    Ok(result)
}

fn apply_callouts(
    source: &str,
    events: &[(Event<'_>, std::ops::Range<usize>)],
    enabled: bool,
    result: &mut PreparedContent,
) -> Result<(), ImportError> {
    let mut consumed = Vec::<std::ops::Range<usize>>::new();
    for (event, span) in events {
        if !matches!(event, Event::Start(Tag::BlockQuote(_)))
            || consumed
                .iter()
                .any(|r| r.start <= span.start && r.end >= span.end)
        {
            continue;
        }
        let raw = &source[span.clone()];
        let mut lines = raw.lines();
        let Some(first) = lines.next() else { continue };
        let marker = first.trim_start().strip_prefix('>').unwrap_or(first).trim();
        let Some(kind) = marker.strip_prefix("[!").and_then(|s| s.strip_suffix(']')) else {
            continue;
        };
        if !["NOTE", "TIP", "IMPORTANT", "WARNING", "CAUTION"].contains(&kind) {
            continue;
        }
        if !enabled {
            result.diagnostics.push(Diagnostic {
                code: "callout-codec-disabled",
                range: span.clone(),
            });
            continue;
        }
        let body = lines
            .map(|line| {
                let line = line.trim_start().strip_prefix('>').unwrap_or(line);
                line.strip_prefix(' ').unwrap_or(line)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let indices = result
            .ranges
            .iter()
            .enumerate()
            .filter(|(_, r)| r.end > span.start && r.start < span.end)
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        let Some(&start) = indices.first() else {
            continue;
        };
        let end = indices.last().unwrap() + 1;
        let mut containers = result.blocks[start].containers.clone();
        if matches!(
            containers.last().map(|c| &c.1),
            Some(editac_core::commands::lists::ContainerStyle::Quote)
        ) {
            containers.pop();
        }
        let tuple = Content::Callout {
            kind: kind.into(),
            body: body.clone(),
            source: raw.into(),
        }
        .tuple()
        .map_err(|_| ImportError::ResourceLimit)?;
        result.blocks.splice(
            start..end,
            [crate::FragmentBlock {
                attributes: Some(BlockAttributes::Unknown(tuple)),
                runs: vec![FragmentRun {
                    text: format!("{kind}\n{body}"),
                    marks: vec![],
                }],
                containers,
                checked: None,
            }],
        );
        result.ranges.splice(start..end, [span.clone()]);
        consumed.push(span.clone());
    }
    Ok(())
}
