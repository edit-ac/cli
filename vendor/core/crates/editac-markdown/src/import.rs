//! Whole-document event mapping to bounded, inert portable fragments.
use crate::{FragmentBlock, FragmentRun, profile};
use editac_core::{
    commands::{
        format::{Mark, normalize_link_url},
        lists::ContainerStyle,
    },
    document::BlockAttributes,
};
use pulldown_cmark::{CodeBlockKind, Event, Tag, TagEnd};
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub range: Range<usize>,
}
#[derive(Clone, Debug)]
pub struct PreparedContent {
    pub source: String,
    pub blocks: Vec<FragmentBlock>,
    pub ranges: Vec<Range<usize>>,
    pub diagnostics: Vec<Diagnostic>,
}
pub use editac_core::commands::fragment::ImportError;

struct Builder {
    result: PreparedContent,
    current: Option<FragmentBlock>,
    start: usize,
    end: usize,
    containers: Vec<(String, ContainerStyle)>,
    marks: Vec<(Mark, Option<String>)>,
    next: usize,
    code: bool,
}
impl Builder {
    fn block(&mut self, attributes: BlockAttributes, span: &Range<usize>) {
        self.finish();
        self.current = Some(FragmentBlock {
            runs: vec![],
            attributes: Some(attributes),
            containers: self.containers.clone(),
            checked: None,
        });
        self.start = span.start;
        self.end = span.end;
    }
    fn finish(&mut self) {
        if let Some(mut block) = self.current.take() {
            if block.runs.is_empty() {
                block.runs.push(FragmentRun {
                    text: String::new(),
                    marks: vec![],
                });
            }
            self.result.blocks.push(block);
            self.result.ranges.push(self.start..self.end);
        }
    }
    fn text(&mut self, text: &str, span: &Range<usize>, code: bool) {
        if self.current.is_none() {
            self.block(BlockAttributes::Paragraph, span);
        }
        let mut marks = self.marks.clone();
        if code && !marks.iter().any(|(m, _)| *m == Mark::Code) {
            marks.push((Mark::Code, None));
        }
        let block = self.current.as_mut().unwrap();
        if let Some(last) = block.runs.last_mut() {
            if last.marks == marks {
                last.text.push_str(text);
                self.end = self.end.max(span.end);
                return;
            }
        }
        block.runs.push(FragmentRun {
            text: text.into(),
            marks,
        });
        self.end = self.end.max(span.end);
    }
    fn diagnose(&mut self, code: &'static str, range: Range<usize>) {
        self.result.diagnostics.push(Diagnostic { code, range });
    }
    fn container(&mut self, style: ContainerStyle) {
        self.finish();
        self.next += 1;
        self.containers.push((format!("md:{}", self.next), style));
    }
}

pub fn import(source: &str) -> Result<PreparedContent, ImportError> {
    if source.len() > 1_048_576 {
        return Err(ImportError::ResourceLimit);
    }
    let mut b = Builder {
        result: PreparedContent {
            source: source.into(),
            blocks: vec![],
            ranges: vec![],
            diagnostics: vec![],
        },
        current: None,
        start: 0,
        end: 0,
        containers: vec![],
        marks: vec![],
        next: 0,
        code: false,
    };
    let mut skip: Option<(TagEnd, usize)> = None;
    let mut inline_stack: Vec<Option<(Mark, Option<String>)>> = Vec::new();
    let mut item_blocks = Vec::new();
    for (event, span) in profile::events(source) {
        if let Some((end, depth)) = skip.as_mut() {
            if matches!(&event,Event::Start(tag) if tag.to_end()==*end) {
                *depth += 1;
            }
            if event == Event::End(*end) {
                *depth -= 1;
                if *depth == 0 {
                    skip = None;
                }
            }
            continue;
        }
        match event {
            Event::Start(Tag::Paragraph) => b.block(BlockAttributes::Paragraph, &span),
            Event::End(TagEnd::Paragraph) => b.finish(),
            Event::Start(Tag::Heading { level, .. }) => {
                b.block(BlockAttributes::Heading(level as u8), &span)
            }
            Event::End(TagEnd::Heading(_)) => b.finish(),
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Indented => String::new(),
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                };
                if language.len() > 64
                    || !language
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"+._-".contains(&c))
                {
                    b.block(BlockAttributes::Paragraph, &span);
                    b.text(&source[span.clone()], &span, false);
                    b.finish();
                    b.diagnose("code-language-unadmitted", span);
                    skip = Some((TagEnd::CodeBlock, 1));
                    continue;
                }
                b.block(BlockAttributes::Code(language), &span);
                b.code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                b.finish();
                b.code = false;
            }
            Event::Start(Tag::List(start)) => {
                if start == Some(0) {
                    b.block(BlockAttributes::Paragraph, &span);
                    b.text(&source[span.clone()], &span, false);
                    b.finish();
                    b.diagnose("zero-start-list-unadmitted", span);
                    skip = Some((TagEnd::List(true), 1));
                    continue;
                }
                let style = match start {
                    Some(n) => ContainerStyle::Ordered {
                        start: u32::try_from(n)
                            .ok()
                            .filter(|n| (1..=2147483647).contains(n))
                            .ok_or(ImportError::ResourceLimit)?,
                    },
                    None => ContainerStyle::Unordered,
                };
                b.container(style);
            }
            Event::Start(Tag::BlockQuote(_)) => b.container(ContainerStyle::Quote),
            Event::End(TagEnd::List(_) | TagEnd::BlockQuote(_)) => {
                b.finish();
                b.containers.pop().ok_or(ImportError::InvalidStructure)?;
            }
            Event::Start(Tag::Item) => {
                b.finish();
                item_blocks.push(b.result.blocks.len());
            }
            Event::End(TagEnd::Item) => {
                b.finish();
                let start = item_blocks.pop().ok_or(ImportError::InvalidStructure)?;
                if b.result.blocks.len() == start {
                    b.block(BlockAttributes::Paragraph, &span);
                    b.finish();
                }
                // The exact source remains recoverable when the host model has
                // no multi-paragraph item identity. Never silently claim fidelity.
                let count = b.result.blocks[start..]
                    .iter()
                    .filter(|block| block.containers.len() == b.containers.len())
                    .count();
                if count > 1 {
                    b.diagnose("multi-paragraph-list-item", span.clone());
                }
            }
            Event::TaskListMarker(checked) => {
                if b.current.is_none() {
                    b.block(BlockAttributes::Paragraph, &span);
                }
                let block = b.current.as_mut().unwrap();
                block.checked = Some(checked);
                if let Some((_, style)) = block.containers.last_mut() {
                    *style = ContainerStyle::Checklist;
                }
            }
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough) => {
                let mark = match event {
                    Event::Start(Tag::Emphasis) => Mark::Italic,
                    Event::Start(Tag::Strong) => Mark::Bold,
                    _ => Mark::Strike,
                };
                let entry = (mark, None);
                b.marks.push(entry.clone());
                inline_stack.push(Some(entry));
            }
            Event::Start(Tag::Link {
                dest_url, title, ..
            }) => {
                if !title.is_empty() {
                    b.diagnose("link-title", span.clone());
                }
                let entry = match normalize_link_url(&dest_url) {
                    Ok(url) => Some((Mark::Link, Some(url))),
                    Err(_) => {
                        b.diagnose("unadmitted-link", span.clone());
                        None
                    }
                };
                if let Some(entry) = &entry {
                    b.marks.push(entry.clone());
                }
                inline_stack.push(entry);
            }
            Event::End(
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link,
            ) => {
                if inline_stack
                    .pop()
                    .ok_or(ImportError::InvalidStructure)?
                    .is_some()
                {
                    b.marks.pop();
                }
            }
            Event::Text(text) => b.text(&text, &span, false),
            Event::Code(text) => b.text(&text, &span, true),
            Event::SoftBreak | Event::HardBreak => b.text("\n", &span, false),
            Event::Rule => {
                b.block(BlockAttributes::Separator, &span);
                b.finish();
            }
            Event::Start(Tag::Image { .. }) => {
                b.text(&source[span.clone()], &span, false);
                b.diagnose("image-codec-disabled", span);
                skip = Some((TagEnd::Image, 1));
            }
            Event::Start(Tag::Table(_)) => {
                b.block(BlockAttributes::Paragraph, &span);
                b.text(&source[span.clone()], &span, false);
                b.finish();
                b.diagnose("table-codec-disabled", span);
                skip = Some((TagEnd::Table, 1));
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                b.text(&source[span.clone()], &span, false);
                b.diagnose("raw-html-inert", span);
            }
            Event::Start(Tag::HtmlBlock) => b.block(BlockAttributes::Paragraph, &span),
            Event::End(TagEnd::HtmlBlock) => b.finish(),
            _ => return Err(ImportError::InvalidStructure),
        }
        if b.containers.len() > 16
            || b.result.blocks.len() > 128
            || b.result.blocks.iter().map(|x| x.runs.len()).sum::<usize>()
                + b.current.as_ref().map_or(0, |x| x.runs.len())
                > 1024
            || b.result.diagnostics.len() > 1024
        {
            return Err(ImportError::ResourceLimit);
        }
    }
    b.finish();
    if b.result.blocks.is_empty() {
        b.block(BlockAttributes::Paragraph, &(0..source.len()));
        b.finish();
    }
    if b.result.blocks.len() > 128 {
        return Err(ImportError::ResourceLimit);
    }
    Ok(b.result)
}
