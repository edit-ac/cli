//! Bounded JSON-only carrier. Payload/source are never interpreted as module code.
use super::content::ContentError;
use yrs::Any;
const LIMIT: usize = 1_048_576;
fn bounded(
    value: &Any,
    nodes: &mut usize,
    bytes: &mut usize,
    depth: usize,
) -> Result<(), ContentError> {
    *nodes = nodes.checked_sub(1).ok_or(ContentError::ResourceLimit)?;
    if depth > 16 {
        return Err(ContentError::ResourceLimit);
    }
    let mut charge = |n: usize| {
        *bytes = bytes.checked_sub(n).ok_or(ContentError::ResourceLimit)?;
        Ok::<_, ContentError>(())
    };
    match value {
        Any::Null | Any::Bool(_) => {}
        Any::Number(n) if n.as_f64().is_some_and(f64::is_finite) => {}
        Any::String(s) => charge(s.len())?,
        Any::Array(values) => {
            for v in values.iter() {
                bounded(v, nodes, bytes, depth + 1)?;
            }
        }
        Any::Map(values) => {
            for (key, v) in values.iter() {
                if ["__proto__", "constructor", "prototype"].contains(&key.as_str()) {
                    return Err(ContentError::InvalidShape);
                }
                *bytes = bytes
                    .checked_sub(key.len())
                    .ok_or(ContentError::ResourceLimit)?;
                bounded(v, nodes, bytes, depth + 1)?;
            }
        }
        _ => return Err(ContentError::InvalidShape),
    }
    Ok(())
}
fn name(value: &str) -> bool {
    let mut parts = value.split('.');
    let valid = |p: &str| {
        p.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && p.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    };
    value.len() <= 128
        && parts.next().is_some_and(valid)
        && parts.next().is_some_and(valid)
        && parts.next().is_none()
}
pub(super) fn validate(value: &Any) -> Result<(), ContentError> {
    let mut nodes = 65536;
    let mut bytes = LIMIT;
    bounded(value, &mut nodes, &mut bytes, 0)?;
    let Any::Map(data) = value else {
        return Err(ContentError::InvalidShape);
    };
    if data.len() != 5 || data.get("version") != Some(&Any::from(1u32))
        || !data.get("type").is_some_and(|v| matches!(v, Any::String(s) if name(s)))
        || !data.get("dataVersion").is_some_and(|v| matches!(v, Any::Number(n) if n.as_f64().is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n >= 1.0 && n <= u32::MAX as f64)))
        || !matches!(data.get("source"), Some(Any::String(_))) || !data.contains_key("payload") {
        return Err(ContentError::InvalidShape);
    }
    let mut json = String::new();
    value.to_json(&mut json);
    if json.len() > LIMIT {
        return Err(ContentError::ResourceLimit);
    }
    Ok(())
}
