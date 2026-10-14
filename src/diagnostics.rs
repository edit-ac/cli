//! Secondary bounded interchange over shared codecs, without hosted transport.
//!
//! JSON is the existing v1 clipboard fragment, not collaborative history.
//! Update JSON is the existing `{identity, bytes}` replay envelope (byte array,
//! never an anonymous Yrs file). Complete updates admit through shared restore;
//! dependency-bearing updates require an explicit initialized base. A valid empty
//! update is a no-op, never proof of document initialization. Inspection emits
//! counts/codes only, never source text, document IDs or credential-like payloads.
use crate::{
    engine::{
        self,
        commands::editor::FragmentBlock,
        document::{Document, DocumentIdentity, InitializedCheckpoint},
    },
    files::{self, FailureKind, Filesystem, NativeFilesystem},
    markdown,
    storage::StoredIdentity,
    target::{self, Baseline, Format as LocalFormat, LocalTarget, ResolveOptions},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MAX_INPUT_BYTES: usize = 1_048_576;
const SCHEMA: &str = engine::document::STABLE_EDITOR_SCHEMA;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Markdown,
    Json,
    Update,
}
impl Format {
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "markdown" => Ok(Self::Markdown),
            "json" => Ok(Self::Json),
            "update" => Ok(Self::Update),
            _ => Err(error("unsupported-format")),
        }
    }
}
#[derive(Debug)]
pub struct Error {
    pub code: &'static str,
    pub publication: Option<FailureKind>,
    pub recovery: Option<Vec<u8>>,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code)
    }
}
impl std::error::Error for Error {}
fn error(code: &'static str) -> Error {
    Error {
        code,
        publication: None,
        recovery: None,
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub code: String,
    pub start: usize,
    pub end: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub format: Format,
    pub input_bytes: usize,
    pub blocks: Option<usize>,
    pub update_bytes: Option<usize>,
    pub diagnostics: Vec<Diagnostic>,
    pub durability: &'static str,
    pub update_scope: Option<&'static str>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fragment {
    version: f64,
    schema: String,
    blocks: Vec<engine::interchange::TransferBlock>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Identified {
    identity: StoredIdentity,
    bytes: Vec<u8>,
}
fn text(bytes: &[u8]) -> Result<&str, Error> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(error("resource-limit"));
    }
    std::str::from_utf8(bytes).map_err(|_| error("invalid-utf8"))
}
fn diagnostics(ds: &[markdown::import::Diagnostic]) -> Vec<Diagnostic> {
    ds.iter()
        .map(|d| Diagnostic {
            code: d.code.into(),
            start: d.range.start,
            end: d.range.end,
        })
        .collect()
}
fn blocks(bytes: &[u8], format: Format) -> Result<(Vec<FragmentBlock>, Vec<Diagnostic>), Error> {
    let source = text(bytes)?;
    match format {
        Format::Markdown => {
            let p = markdown::import::import(source).map_err(|_| error("markdown-rejected"))?;
            Ok((p.blocks, diagnostics(&p.diagnostics)))
        }
        Format::Json => {
            let p: Fragment = engine::interchange::parse(source, "cli-diagnostics")
                .map_err(|_| error("invalid-json"))?;
            if p.version != 1.0 || p.schema != SCHEMA {
                return Err(error("unsupported-version"));
            }
            engine::clipboard::validate(source, SCHEMA).map_err(|_| error("invalid-fragment"))?;
            let blocks = engine::interchange::transfer_blocks(p.blocks)
                .map_err(|_| error("invalid-fragment"))?;
            Ok((blocks, Vec::new()))
        }
        Format::Update => Err(error("update-is-not-semantic-interchange")),
    }
}
fn identified(bytes: &[u8]) -> Result<(Identified, DocumentIdentity), Error> {
    let p: Identified = engine::interchange::parse(text(bytes)?, "cli-update")
        .map_err(|_| error("invalid-update-envelope"))?;
    // v5 is the qualified CLI profile; never guess/migrate an unknown profile.
    let i = DocumentIdentity::v5(&p.identity.document_id, &p.identity.epoch)
        .map_err(|_| error("invalid-identity"))?;
    if p.identity != StoredIdentity::from(&i) {
        return Err(error("unsupported-version"));
    }
    if p.bytes.is_empty() || p.bytes.len() > MAX_INPUT_BYTES {
        return Err(error("invalid-update-size"));
    }
    Ok((p, i))
}
fn admit_update(bytes: &[u8], base: Option<&[u8]>) -> Result<Identified, Error> {
    let (p, identity) = identified(bytes)?;
    let (checkpoint, journal) = if let Some(base) = base {
        let (b, bi) = identified(base)?;
        if bi != identity {
            return Err(error("base-identity-mismatch"));
        }
        (
            b.bytes,
            vec![engine::document::ReplayUpdate {
                identity: identity.clone(),
                bytes: p.bytes.clone(),
            }],
        )
    } else {
        (p.bytes.clone(), Vec::new())
    };
    Document::restore(
        &identity,
        &InitializedCheckpoint {
            identity: identity.clone(),
            initialized: true,
            writer_id: 0,
            bytes: checkpoint,
        },
        &journal,
    )
    .map_err(|_| error("invalid-or-incomplete-update"))?;
    Ok(p)
}
/// Validate on an inert fragment or disposable shared replica. A base is only
/// meaningful for identified updates; it is never silently ignored.
pub fn validate(bytes: &[u8], format: Format, base: Option<&[u8]>) -> Result<Report, Error> {
    if format != Format::Update && base.is_some() {
        return Err(error("unexpected-update-base"));
    }
    let mut report = Report {
        format,
        input_bytes: bytes.len(),
        blocks: None,
        update_bytes: None,
        diagnostics: Vec::new(),
        durability: "not-assessed",
        update_scope: if format == Format::Update {
            Some(if base.is_some() {
                "replay-with-explicit-base;initialization-not-assessed"
            } else {
                "self-contained-replay;initialization-not-assessed"
            })
        } else {
            None
        },
    };
    if format == Format::Update {
        report.update_bytes = Some(admit_update(bytes, base)?.bytes.len());
    } else {
        let (b, d) = blocks(bytes, format)?;
        report.blocks = Some(b.len());
        report.diagnostics = d;
    }
    Ok(report)
}
/// Safe inspection intentionally returns the same bounded metadata as validation.
pub fn inspect(bytes: &[u8], format: Format, base: Option<&[u8]>) -> Result<Report, Error> {
    validate(bytes, format, base)
}
#[derive(Debug)]
pub struct Conversion {
    pub bytes: Vec<u8>,
    pub diagnostics: Vec<Diagnostic>,
    pub lossy: bool,
}
pub fn convert(
    bytes: &[u8],
    input: Format,
    output: Format,
    allow_lossy: bool,
) -> Result<Conversion, Error> {
    if input == Format::Update || output == Format::Update {
        if input != output {
            return Err(error("update-is-not-semantic-interchange"));
        }
        let p = admit_update(bytes, None)?;
        let bytes = serde_json::to_vec(&p).map_err(|_| error("serialization-failed"))?;
        return Ok(Conversion {
            bytes,
            diagnostics: Vec::new(),
            lossy: false,
        });
    }
    let (b, mut ds) = blocks(bytes, input)?;
    let (result, loss) = match output {
        Format::Json => (serde_json::to_vec(&serde_json::json!({"version":1,"schema":SCHEMA,"blocks":b.iter().map(engine::interchange::transfer_value).collect::<Vec<_>>()})).map_err(|_| error("serialization-failed"))?, false),
        Format::Markdown => { let e = markdown::export::export(&b); let loss = !e.recovery.is_empty() || !e.diagnostics.is_empty(); ds.extend(diagnostics(&e.diagnostics)); (e.markdown.into_bytes(), loss) }
        Format::Update => unreachable!(),
    };
    // Import fallback diagnostics also require explicit consent: converting
    // unsupported source to semantic JSON cannot silently lose that exact source.
    let lossy = loss || !ds.is_empty();
    if lossy && !allow_lossy {
        return Err(Error {
            code: "lossy-conversion-requires-consent",
            publication: None,
            recovery: Some(bytes.to_vec()),
        });
    }
    if result.len() > MAX_INPUT_BYTES {
        return Err(error("resource-limit"));
    }
    // Qualify output through the same shared admission before any I/O.
    validate(&result, output, None)?;
    Ok(Conversion {
        bytes: result,
        diagnostics: ds,
        lossy,
    })
}
/// Read explicit regular UTF-8 input with a cap before allocation. No suffix or
/// MIME guessing, FIFO/device opens, final symlink following or credential use.
pub fn read(path: &Path) -> Result<Vec<u8>, Error> {
    let target = target::resolve(
        path,
        ResolveOptions {
            create: false,
            format: Some(LocalFormat::Text),
        },
    )
    .map_err(|e| error(e.code()))?;
    text(target.source_bytes())?;
    Ok(target.source_bytes().to_vec())
}
/// Prepare the complete conversion before taking a destination baseline. Refuse
/// the source itself and hard-link aliases, even with overwrite consent.
pub fn convert_file(
    source: &Path,
    destination: &Path,
    input: Format,
    output: Format,
    allow_lossy: bool,
    overwrite: bool,
) -> Result<(PathBuf, Conversion), Error> {
    convert_file_with(
        source,
        destination,
        input,
        output,
        allow_lossy,
        overwrite,
        &mut NativeFilesystem,
    )
}
pub fn convert_file_with(
    source: &Path,
    destination: &Path,
    input: Format,
    output: Format,
    allow_lossy: bool,
    overwrite: bool,
    io: &mut impl Filesystem,
) -> Result<(PathBuf, Conversion), Error> {
    let bytes = read(source)?;
    let conversion = convert(&bytes, input, output, allow_lossy)?;
    let target = target::resolve(
        destination,
        ResolveOptions {
            create: true,
            format: Some(LocalFormat::Text),
        },
    )
    .map_err(|_| error("invalid-destination"))?;
    require_destination(source, &target, overwrite)?;
    match files::publish_bytes(&target, &conversion.bytes, io) {
        Ok((_, path)) => Ok((path, conversion)),
        Err((kind, _cause)) => Err(Error {
            code: if kind == FailureKind::PublicationUncertain {
                "publication-uncertain"
            } else {
                "publication-failed"
            },
            publication: Some(kind),
            recovery: Some(conversion.bytes),
        }),
    }
}
fn require_destination(source: &Path, target: &LocalTarget, overwrite: bool) -> Result<(), Error> {
    let source = std::fs::canonicalize(source).map_err(|_| error("input-io"))?;
    if target.resolved_path() == source {
        return Err(error("source-destination-alias"));
    }
    if let Baseline::Existing { .. } = target.baseline() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = std::fs::metadata(&source).map_err(|_| error("input-io"))?;
            let b =
                std::fs::metadata(target.resolved_path()).map_err(|_| error("destination-io"))?;
            if a.dev() == b.dev() && a.ino() == b.ino() {
                return Err(error("source-destination-alias"));
            }
        }
        if !overwrite {
            return Err(error("overwrite-required"));
        }
    }
    Ok(())
}
