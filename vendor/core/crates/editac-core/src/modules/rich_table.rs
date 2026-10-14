//! Pure v2 table admission. Geometry and cell blocks stay inert in one tuple.
use super::content::{Content, ContentError};
use std::{collections::HashMap, sync::Arc};
use yrs::Any;
type Result<T> = std::result::Result<T, ContentError>;
fn object(value: &Any, keys: &[&str]) -> Result<Arc<HashMap<String, Any>>> {
    let Any::Map(map) = value else {
        return Err(ContentError::InvalidShape);
    };
    if map.len() != keys.len() || keys.iter().any(|key| !map.contains_key(*key)) {
        return Err(ContentError::InvalidShape);
    }
    Ok(map.clone())
}
fn array(value: &Any) -> Result<&[Any]> {
    let Any::Array(values) = value else {
        return Err(ContentError::InvalidShape);
    };
    Ok(values)
}
fn integer(value: &Any, maximum: usize) -> Result<usize> {
    let Any::Number(n) = value else {
        return Err(ContentError::InvalidShape);
    };
    let n = n.as_f64().ok_or(ContentError::InvalidShape)?;
    if n.fract() != 0.0 || !(1.0..=maximum as f64).contains(&n) {
        return Err(ContentError::InvalidShape);
    }
    Ok(n as usize)
}
pub(super) fn validate(value: &Any) -> Result<()> {
    let data = object(value, &["alignments", "rows", "source"])?;
    let alignments = array(&data["alignments"])?;
    let rows = array(&data["rows"])?;
    let Any::String(source) = &data["source"] else {
        return Err(ContentError::InvalidShape);
    };
    if alignments.is_empty()
        || alignments.len() > 32
        || rows.is_empty()
        || rows.len() > 128
        || source.len() > 1_048_576
    {
        return Err(ContentError::ResourceLimit);
    }
    let width = alignments.len();
    let mut covered = vec![vec![false; width]; rows.len()];
    let mut legacy_rows = Vec::new();
    let mut paragraphs = 0usize;
    let mut run_count = 0usize;
    for (r, row) in rows.iter().enumerate() {
        let cells = array(row)?;
        if cells.len() != width {
            return Err(ContentError::InvalidShape);
        }
        let mut legacy_cells = Vec::new();
        for (c, cell) in cells.iter().enumerate() {
            if covered[r][c] {
                if *cell != Any::Null {
                    return Err(ContentError::InvalidShape);
                }
                legacy_cells.push(Any::Array(vec![].into()));
                continue;
            }
            let cell = object(cell, &["paragraphs", "rowSpan", "columnSpan", "header"])?;
            if !matches!(cell["header"], Any::Bool(_)) {
                return Err(ContentError::InvalidShape);
            }
            let height = integer(&cell["rowSpan"], rows.len() - r)?;
            let length = integer(&cell["columnSpan"], width - c)?;
            for line in covered.iter_mut().skip(r).take(height) {
                for occupied in line.iter_mut().skip(c).take(length) {
                    if *occupied {
                        return Err(ContentError::InvalidShape);
                    }
                    *occupied = true;
                }
            }
            let blocks = array(&cell["paragraphs"])?;
            if blocks.is_empty() {
                return Err(ContentError::InvalidShape);
            }
            paragraphs += blocks.len();
            if paragraphs > 1024 {
                return Err(ContentError::ResourceLimit);
            }
            let mut runs = Vec::new();
            for block in blocks {
                let block = object(block, &["style", "runs"])?;
                let style = array(&block["style"])?;
                match style {
                    [Any::String(tag)] if tag.as_ref() == "paragraph" => {}
                    [Any::String(tag), level] if tag.as_ref() == "heading" => {
                        integer(level, 6)?;
                    }
                    [Any::String(tag), Any::String(language)]
                        if tag.as_ref() == "code"
                            && language.len() <= 64
                            && language
                                .bytes()
                                .all(|c| c.is_ascii_alphanumeric() || b"+._-".contains(&c)) => {}
                    _ => return Err(ContentError::InvalidShape),
                }
                let block_runs = array(&block["runs"])?;
                run_count += block_runs.len();
                if run_count > 1024 {
                    return Err(ContentError::ResourceLimit);
                }
                runs.extend(block_runs.iter().cloned());
            }
            legacy_cells.push(Any::Array(runs.into()));
        }
        legacy_rows.push(Any::Array(legacy_cells.into()));
    }
    // Reuse the qualified v1 mark/link/text/alignment and aggregate run bounds.
    let legacy = Any::Array(
        vec![
            Any::from("module:editac.table"),
            Any::from(1u32),
            Any::Map(Arc::new(HashMap::from([
                ("alignments".into(), data["alignments"].clone()),
                ("rows".into(), Any::Array(legacy_rows.into())),
                ("source".into(), data["source"].clone()),
            ]))),
        ]
        .into(),
    );
    Content::from_tuple(&legacy)?.ok_or(ContentError::InvalidShape)?;
    Ok(())
}
