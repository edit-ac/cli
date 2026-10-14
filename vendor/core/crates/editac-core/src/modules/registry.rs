//! Frozen, inert module registration. Presentation belongs to the host; inline
//! atoms use attributed fallback text in the existing text-only document profile.
use std::collections::{BTreeMap, HashMap};
use yrs::Any;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    InvalidDefinition,
    Duplicate,
    Incompatible,
    InvalidContent,
    ResourceLimit,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Mark,
    InlineAtom,
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum AttributeType {
    String(usize),
    Boolean,
    Number,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attribute {
    shape: AttributeType,
    required: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub type_name: String,
    pub version: u32,
    pub kind: ModuleKind,
    attributes: BTreeMap<String, Attribute>,
    max_utf16: usize,
}
type Result<T> = std::result::Result<T, RegistryError>;
fn object(v: &Any) -> Result<&HashMap<String, Any>> {
    match v {
        Any::Map(v) => Ok(v),
        _ => Err(RegistryError::InvalidDefinition),
    }
}
fn string(v: &Any) -> Result<&str> {
    match v {
        Any::String(v) => Ok(v),
        _ => Err(RegistryError::InvalidDefinition),
    }
}
fn exact(v: &HashMap<String, Any>, keys: &[&str]) -> Result<()> {
    if v.len() == keys.len() && keys.iter().all(|k| v.contains_key(*k)) {
        Ok(())
    } else {
        Err(RegistryError::InvalidDefinition)
    }
}
fn positive(v: &Any, max: u32) -> Result<u32> {
    if let Any::Number(n) = v {
        if let Some(n) = n.as_i64() {
            if n >= 1 && n <= i64::from(max) {
                return Ok(n as u32);
            }
        }
    }
    Err(RegistryError::InvalidDefinition)
}

fn segment(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn namespace(s: &str) -> bool {
    let parts: Vec<_> = s.split('.').collect();
    s.len() <= 128 && parts.len() == 2 && parts.iter().all(|p| segment(p))
}
fn field(s: &str) -> bool {
    s.len() <= 64
        && s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && s.bytes().all(|b| b.is_ascii_alphanumeric())
        && !matches!(s, "constructor" | "prototype" | "__proto__")
}
fn bounded(v: &Any, budget: &mut usize, depth: usize) -> Result<()> {
    if depth > 16 {
        return Err(RegistryError::ResourceLimit);
    }
    *budget = budget.checked_sub(1).ok_or(RegistryError::ResourceLimit)?;
    match v {
        Any::Null | Any::Bool(_) => {}
        Any::Number(n) if n.as_f64().is_some_and(f64::is_finite) => {}
        Any::String(s) => {
            *budget = budget
                .checked_sub(s.encode_utf16().count())
                .ok_or(RegistryError::ResourceLimit)?;
        }
        Any::Array(a) if a.len() <= 8192 => {
            for x in a.iter() {
                bounded(x, budget, depth + 1)?;
            }
        }
        Any::Map(m) => {
            for (k, x) in m.iter() {
                *budget = budget
                    .checked_sub(k.encode_utf16().count())
                    .ok_or(RegistryError::ResourceLimit)?;
                bounded(x, budget, depth + 1)?;
            }
        }
        _ => return Err(RegistryError::InvalidDefinition),
    }
    Ok(())
}
impl Definition {
    pub fn from_data(value: &Any) -> Result<Self> {
        bounded(value, &mut 65536, 0)?;
        let v = object(value)?;
        exact(
            v,
            &[
                "type",
                "version",
                "kind",
                "attributes",
                "content",
                "fallback",
            ],
        )?;
        let type_name = string(&v["type"])?;
        if !namespace(type_name) {
            return Err(RegistryError::InvalidDefinition);
        }
        let version = positive(&v["version"], u32::MAX)?;
        let kind = match string(&v["kind"])? {
            "mark" => ModuleKind::Mark,
            "inline-atom" => ModuleKind::InlineAtom,
            _ => return Err(RegistryError::InvalidDefinition),
        };
        let content = object(&v["content"])?;
        exact(content, &["type", "maxUtf16"])?;
        if string(&content["type"])? != "text" {
            return Err(RegistryError::InvalidDefinition);
        }
        let max_utf16 = positive(&content["maxUtf16"], 8192)? as usize;
        let fallback = object(&v["fallback"])?;
        exact(fallback, &["type", "source"])?;
        if string(&fallback["type"])? != "text" || string(&fallback["source"])? != "content" {
            return Err(RegistryError::InvalidDefinition);
        }
        let raw = object(&v["attributes"])?;
        if raw.len() > 32 {
            return Err(RegistryError::InvalidDefinition);
        }
        let mut attributes = BTreeMap::new();
        for (k, value) in raw {
            if !field(k) {
                return Err(RegistryError::InvalidDefinition);
            }
            let a = object(value)?;
            let shape = match string(a.get("type").ok_or(RegistryError::InvalidDefinition)?)? {
                "string" => {
                    exact(a, &["type", "required", "maxUtf16"])?;
                    AttributeType::String(positive(&a["maxUtf16"], 8192)? as usize)
                }
                "boolean" => {
                    exact(a, &["type", "required"])?;
                    AttributeType::Boolean
                }
                "number" => {
                    exact(a, &["type", "required"])?;
                    AttributeType::Number
                }
                _ => return Err(RegistryError::InvalidDefinition),
            };
            let Any::Bool(required) = a["required"] else {
                return Err(RegistryError::InvalidDefinition);
            };
            attributes.insert(k.clone(), Attribute { shape, required });
        }
        Ok(Self {
            type_name: type_name.into(),
            version,
            kind,
            attributes,
            max_utf16,
        })
    }
}
#[derive(Default, Debug)]
pub struct Registry {
    definitions: BTreeMap<String, Definition>,
}
#[derive(Debug)]
pub struct FrozenRegistry {
    definitions: BTreeMap<String, Definition>,
}
impl Registry {
    pub fn register(&mut self, value: &Any) -> Result<()> {
        let d = Definition::from_data(value)?;
        if let Some(old) = self.definitions.get(&d.type_name) {
            return Err(if old == &d {
                RegistryError::Duplicate
            } else {
                RegistryError::Incompatible
            });
        }
        if self.definitions.len() >= 256 {
            return Err(RegistryError::ResourceLimit);
        }
        self.definitions.insert(d.type_name.clone(), d);
        Ok(())
    }
    pub fn freeze(self) -> FrozenRegistry {
        FrozenRegistry {
            definitions: self.definitions,
        }
    }
}
impl FrozenRegistry {
    pub fn len(&self) -> usize {
        self.definitions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }
    /// Future envelopes are retained opaquely. Known values must match exactly;
    /// validation only reads borrowed data and never mutates a document.
    pub fn resolve(&self, key: &str, value: &Any, text: &str) -> Result<Option<&Definition>> {
        let Some(name) = key.strip_prefix("module:") else {
            return Ok(None);
        };
        let Some(d) = self.definitions.get(name) else {
            return Ok(None);
        };
        let Any::Array(envelope) = value else {
            return Ok(None);
        };
        if envelope.first() != Some(&Any::Number(f64::from(d.version).into())) {
            return Ok(None);
        }
        if envelope.len() != 2 || text.encode_utf16().count() > d.max_utf16 {
            return Err(RegistryError::InvalidContent);
        }
        let attrs = object(&envelope[1]).map_err(|_| RegistryError::InvalidContent)?;
        if attrs.keys().any(|k| !d.attributes.contains_key(k)) {
            return Err(RegistryError::InvalidContent);
        }
        for (k, a) in &d.attributes {
            let Some(v) = attrs.get(k) else {
                if a.required {
                    return Err(RegistryError::InvalidContent);
                } else {
                    continue;
                }
            };
            let valid = match (&a.shape, v) {
                (AttributeType::String(max), Any::String(s)) => s.encode_utf16().count() <= *max,
                (AttributeType::Boolean, Any::Bool(_)) => true,
                (AttributeType::Number, Any::Number(n)) => n.as_f64().is_some_and(f64::is_finite),
                _ => false,
            };
            if !valid {
                return Err(RegistryError::InvalidContent);
            }
        }
        Ok(Some(d))
    }
    pub fn validate_run(&self, text: &str, attributes: &BTreeMap<String, Any>) -> Result<()> {
        for (key, value) in attributes {
            self.resolve(key, value, text)?;
        }
        Ok(())
    }
}
