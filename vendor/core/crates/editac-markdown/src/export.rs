//! Deterministic semantic Markdown, with retained data for every reported loss.
use crate::{FragmentBlock, import::Diagnostic};
use editac_core::modules::content::{Alignment, Content};
use editac_core::{
    commands::{format::Mark, lists::ContainerStyle},
    document::BlockAttributes,
};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ExportResult {
    pub markdown: String,
    pub diagnostics: Vec<Diagnostic>,
    pub recovery: Vec<FragmentBlock>,
}
fn escape(text: &str) -> String {
    let mut s = String::new();
    for ch in text.chars() {
        if "\\`*_{}[]()<>#+-.!|~".contains(ch) {
            s.push('\\');
        }
        s.push(ch);
    }
    s
}
fn longest_ticks(s: &str) -> usize {
    s.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}
fn inline_code(text: &str) -> String {
    let delimiter = "`".repeat(longest_ticks(text) + 1);
    let pad = text.starts_with('`')
        || text.ends_with('`')
        || (text.starts_with(' ') && text.ends_with(' ') && text.chars().any(|c| c != ' '));
    if pad {
        format!("{delimiter} {text} {delimiter}")
    } else {
        format!("{delimiter}{text}{delimiter}")
    }
}
fn order(mark: Mark) -> u8 {
    match mark {
        Mark::Link => 0,
        Mark::Bold => 1,
        Mark::Italic => 2,
        Mark::Strike => 3,
        Mark::Underline => 4,
        Mark::Code => 5,
    }
}
fn open(mark: &(Mark, Option<String>)) -> &'static str {
    match mark.0 {
        Mark::Bold => "**",
        Mark::Italic => "*",
        Mark::Strike => "~~",
        Mark::Link => "[",
        _ => "",
    }
}
fn close(mark: &(Mark, Option<String>)) -> String {
    if mark.0 == Mark::Link {
        let href = mark.1.as_deref().unwrap_or("");
        // Angle destinations avoid parentheses and spaces in admitted URLs.
        format!("](<{}>)", href.replace('<', "%3C").replace('>', "%3E"))
    } else {
        open(mark).into()
    }
}
fn inline(block: &FragmentBlock, diagnostics: &mut Vec<Diagnostic>) -> String {
    let mut s = String::new();
    let mut active = Vec::new();
    for run in &block.runs {
        let mut marks = run.marks.clone();
        marks.sort_by_key(|(m, _)| order(*m));
        marks.dedup();
        for unsupported in [Mark::Underline] {
            if marks.iter().any(|(m, _)| *m == unsupported) {
                diagnostics.push(Diagnostic {
                    code: "underline-unrepresentable",
                    range: 0..0,
                });
            }
        }
        let code = marks.iter().any(|(m, _)| *m == Mark::Code);
        marks.retain(|(m, _)| !matches!(m, Mark::Code | Mark::Underline));
        let common = active
            .iter()
            .zip(&marks)
            .take_while(|(a, b)| a == b)
            .count();
        for mark in active[common..].iter().rev() {
            s.push_str(&close(mark));
        }
        for mark in &marks[common..] {
            s.push_str(open(mark));
        }
        active = marks;
        if code {
            if run.text.contains('\n') {
                diagnostics.push(Diagnostic {
                    code: "inline-code-newline",
                    range: 0..0,
                });
            }
            s.push_str(&inline_code(&run.text));
        } else {
            s.push_str(&escape(&run.text).replace('\n', "\\\n"));
        }
    }
    for mark in active.iter().rev() {
        s.push_str(&close(mark));
    }
    s
}

type SemanticBlock = (
    BlockAttributes,
    Option<bool>,
    Vec<(usize, String)>,
    Vec<(char, Vec<(Mark, Option<String>)>)>,
);
fn semantics(blocks: &[FragmentBlock]) -> Vec<SemanticBlock> {
    let mut keys: BTreeMap<String, usize> = BTreeMap::new();
    blocks
        .iter()
        .map(|b| {
            let containers: Vec<_> = b
                .containers
                .iter()
                .map(|(key, style)| {
                    let next = keys.len();
                    let id = *keys.entry(key.clone()).or_insert(next);
                    (id, format!("{style:?}"))
                })
                .collect();
            let text: Vec<_> = b
                .runs
                .iter()
                .flat_map(|run| {
                    let mut marks = run.marks.clone();
                    marks.sort_by_key(|(m, _)| order(*m));
                    marks.dedup();
                    run.text.chars().map(move |ch| (ch, marks.clone()))
                })
                .collect();
            let mut attributes = b.attributes.clone().unwrap_or(BlockAttributes::Paragraph);
            if let BlockAttributes::Unknown(tuple) = &attributes {
                if let Ok(Some(mut content)) = Content::from_tuple(tuple) {
                    match &mut content {
                        Content::Reference { .. }
                        | Content::RichTable { .. }
                        | Content::InertExtension { .. } => {}
                        Content::Image { source, .. }
                        | Content::Table { source, .. }
                        | Content::Callout { source, .. } => source.clear(),
                    };
                    attributes = BlockAttributes::Unknown(content.tuple().unwrap());
                }
            }
            (attributes, b.checked, containers, text)
        })
        .collect()
}

pub fn equivalent(a: &[FragmentBlock], b: &[FragmentBlock]) -> bool {
    semantics(a) == semantics(b)
}

pub fn export(blocks: &[FragmentBlock]) -> ExportResult {
    let mut diagnostics = Vec::new();
    let mut pieces = Vec::new();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut widths: BTreeMap<String, usize> = BTreeMap::new();
    for block in blocks {
        let text: String = block.runs.iter().map(|r| r.text.as_str()).collect();
        let body = match block
            .attributes
            .as_ref()
            .unwrap_or(&BlockAttributes::Paragraph)
        {
            BlockAttributes::Paragraph => inline(block, &mut diagnostics),
            BlockAttributes::Heading(level) => format!(
                "{} {}",
                "#".repeat(*level as usize),
                inline(block, &mut diagnostics)
            ),
            BlockAttributes::Code(language) => {
                let fence = "`".repeat((longest_ticks(&text) + 1).max(3));
                if !text.is_empty() && !text.ends_with('\n') {
                    diagnostics.push(Diagnostic {
                        code: "code-terminal-newline",
                        range: 0..0,
                    });
                }
                format!(
                    "{fence}{language}\n{}{fence}",
                    if text.is_empty() || text.ends_with('\n') {
                        text.clone()
                    } else {
                        format!("{text}\n")
                    }
                )
            }
            BlockAttributes::Separator => "---".into(),
            BlockAttributes::Unknown(tuple) => match Content::from_tuple(tuple) {
                Ok(Some(content @ Content::InertExtension { .. })) => {
                    diagnostics.push(Diagnostic {
                        code: "inert-extension-source-fallback",
                        range: 0..0,
                    });
                    // Escape executable Markdown/HTML; typed recovery retains exact source/data.
                    escape(content.inert_source().unwrap())
                }
                Ok(Some(Content::Reference { label, .. })) => {
                    diagnostics.push(Diagnostic {
                        code: "reference-identity-not-exported",
                        range: 0..0,
                    });
                    escape(&label)
                }
                Ok(Some(content @ Content::RichTable { .. })) => {
                    diagnostics.push(Diagnostic {
                        code: "rich-table-source-fallback",
                        range: 0..0,
                    });
                    content.rich_table_source().unwrap().to_owned()
                }
                Ok(Some(Content::Callout { source, .. })) => source.trim_end().to_owned(),
                Ok(Some(Content::Image {
                    url, alt, title, ..
                })) => {
                    let title = if title.is_empty() {
                        String::new()
                    } else {
                        format!(" \"{}\"", title.replace('\\', "\\\\").replace('"', "\\\""))
                    };
                    format!("![{}](<{url}>{title})", escape(&alt))
                }
                Ok(Some(Content::Table {
                    alignments, rows, ..
                })) => {
                    let row = |cells: &Vec<Vec<crate::FragmentRun>>,
                               diagnostics: &mut Vec<Diagnostic>| {
                        format!(
                            "| {} |",
                            cells
                                .iter()
                                .map(|runs| inline(
                                    &FragmentBlock {
                                        runs: runs.clone(),
                                        attributes: Some(BlockAttributes::Paragraph),
                                        containers: vec![],
                                        checked: None
                                    },
                                    diagnostics
                                )
                                .replace("\\|", "|")
                                .replace('|', "\\|"))
                                .collect::<Vec<_>>()
                                .join(" | ")
                        )
                    };
                    let mut lines = vec![
                        row(&rows[0], &mut diagnostics),
                        format!(
                            "| {} |",
                            alignments
                                .iter()
                                .map(|a| match a {
                                    Alignment::None => "---",
                                    Alignment::Left => ":---",
                                    Alignment::Center => ":---:",
                                    Alignment::Right => "---:",
                                })
                                .collect::<Vec<_>>()
                                .join(" | ")
                        ),
                    ];
                    lines.extend(rows[1..].iter().map(|cells| row(cells, &mut diagnostics)));
                    lines.join("\n")
                }
                _ => {
                    diagnostics.push(Diagnostic {
                        code: "unknown-content",
                        range: 0..0,
                    });
                    inline(block, &mut diagnostics)
                }
            },
        };
        let mut first = String::new();
        let mut continuation = String::new();
        for (depth, (key, style)) in block.containers.iter().enumerate() {
            if *style == ContainerStyle::Quote {
                first.push_str("> ");
                continuation.push_str("> ");
                continue;
            }
            let parent = depth + 1 < block.containers.len();
            let marker = if parent {
                " ".repeat(*widths.get(key).unwrap_or(&2))
            } else {
                match style {
                    ContainerStyle::Unordered => "- ".into(),
                    ContainerStyle::Checklist => format!(
                        "- [{}] ",
                        if block.checked == Some(true) {
                            "x"
                        } else {
                            " "
                        }
                    ),
                    ContainerStyle::Ordered { start } => {
                        let count = counts.entry(key.clone()).or_insert(*start);
                        let marker = format!("{}. ", *count);
                        *count = count.saturating_add(1);
                        marker
                    }
                    ContainerStyle::Quote => unreachable!(),
                }
            };
            widths.insert(key.clone(), marker.len());
            first.push_str(&marker);
            continuation.push_str(&" ".repeat(marker.len()));
        }
        // Prefix every source line, preserving fenced code whitespace verbatim.
        let mut rendered = String::new();
        for (i, line) in body.split('\n').enumerate() {
            if i > 0 {
                rendered.push('\n');
            }
            rendered.push_str(if i == 0 { &first } else { &continuation });
            rendered.push_str(line);
        }
        pieces.push(rendered);
    }
    let markdown = if pieces.is_empty() {
        String::new()
    } else {
        format!("{}\n", pieces.join("\n\n"))
    };
    // Some arbitrary overlapping mark boundaries have no faithful spelling in
    // this dialect. A failed semantic check must expose loss and retain data.
    if !blocks.is_empty()
        && !crate::optional::import_with_codecs(
            &markdown,
            crate::optional::Codecs {
                callouts: true,
                images: true,
                tables: true,
            },
        )
        .is_ok_and(|p| semantics(&p.blocks) == semantics(blocks))
    {
        diagnostics.push(Diagnostic {
            code: "markdown-unrepresentable",
            range: 0..markdown.len(),
        });
    }
    let recovery = if diagnostics.is_empty() {
        vec![]
    } else {
        blocks.to_vec()
    };
    ExportResult {
        markdown,
        diagnostics,
        recovery,
    }
}
