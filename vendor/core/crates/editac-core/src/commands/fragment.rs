//! Shared prepared-fragment validation, independent of Markdown syntax.
use super::{
    editor::FragmentBlock,
    format::{Mark, normalize_link_url},
};
use crate::{document::BlockAttributes, modules::content::Content};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportError {
    ResourceLimit,
    InvalidStructure,
}
pub fn validate(blocks: &[FragmentBlock]) -> Result<(), ImportError> {
    if blocks.len() > 128
        || blocks.iter().map(|b| b.runs.len()).sum::<usize>() > 1024
        || blocks
            .iter()
            .flat_map(|b| &b.runs)
            .map(|r| r.text.len())
            .sum::<usize>()
            > 1_048_576
    {
        return Err(ImportError::ResourceLimit);
    }
    for b in blocks {
        if b.containers.len() > 16 {
            return Err(ImportError::ResourceLimit);
        }
        match &b.attributes {
            Some(BlockAttributes::Heading(level)) if !(1..=6).contains(level) => {
                return Err(ImportError::InvalidStructure);
            }
            Some(BlockAttributes::Code(language))
                if language.len() > 64
                    || !language
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"+._-".contains(&c)) =>
            {
                return Err(ImportError::InvalidStructure);
            }
            _ => {}
        }
        if let Some(BlockAttributes::Unknown(t)) = &b.attributes {
            if Content::from_tuple(t)
                .map_err(|_| ImportError::InvalidStructure)?
                .is_none()
            {
                return Err(ImportError::InvalidStructure);
            }
        }
        for r in &b.runs {
            if r.marks.len() > 6 {
                return Err(ImportError::ResourceLimit);
            }
            for (m, href) in &r.marks {
                if *m == Mark::Link {
                    normalize_link_url(href.as_deref().ok_or(ImportError::InvalidStructure)?)
                        .map_err(|_| ImportError::InvalidStructure)?;
                } else if href.is_some() {
                    return Err(ImportError::InvalidStructure);
                }
            }
        }
    }
    Ok(())
}
