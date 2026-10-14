//! Trusted, ordered semantic hooks after the qualified whole-document parser.
use crate::{
    FragmentBlock,
    import::{ImportError, PreparedContent},
    optional::{Codecs, import_with_codecs},
};

pub trait Extension {
    fn name(&self) -> &str;
    fn import(
        &self,
        _prepared: &PreparedContent,
    ) -> Result<Option<Vec<FragmentBlock>>, ImportError> {
        Ok(None)
    }
    fn export(&self, _blocks: &[FragmentBlock]) -> Option<String> {
        None
    }
}
pub use editac_core::commands::fragment::validate;
pub fn import(
    source: &str,
    codecs: Codecs,
    extensions: &[&dyn Extension],
) -> Result<PreparedContent, ImportError> {
    if extensions.len() > 32 {
        return Err(ImportError::ResourceLimit);
    }
    let mut p = import_with_codecs(source, codecs)?;
    let mut names = std::collections::BTreeSet::new();
    for extension in extensions {
        let name = extension.name();
        if name.is_empty() || name.len() > 64 || !names.insert(name) {
            return Err(ImportError::InvalidStructure);
        }
        if let Some(blocks) = extension.import(&p)? {
            validate(&blocks)?;
            p.ranges = blocks.iter().map(|_| 0..source.len()).collect();
            p.blocks = blocks;
        }
    }
    Ok(p)
}
pub fn export(
    blocks: &[FragmentBlock],
    extensions: &[&dyn Extension],
) -> Result<crate::export::ExportResult, ImportError> {
    validate(blocks)?;
    for e in extensions {
        if let Some(markdown) = e.export(blocks) {
            if markdown.len() > 1_048_576 {
                return Err(ImportError::ResourceLimit);
            }
            let parsed = import(
                &markdown,
                Codecs {
                    images: true,
                    tables: true,
                    callouts: true,
                },
                extensions,
            )?;
            let equivalent = crate::export::equivalent(blocks, &parsed.blocks);
            return Ok(crate::export::ExportResult {
                markdown,
                diagnostics: if equivalent {
                    vec![]
                } else {
                    vec![crate::import::Diagnostic {
                        code: "extension-unrepresentable",
                        range: 0..0,
                    }]
                },
                recovery: if equivalent { vec![] } else { blocks.to_vec() },
            });
        }
    }
    Ok(crate::export::export(blocks))
}
