//! Explicit descriptor reads and pure candidate migrations. This API has no live
//! document or epoch authority; callers separately use owning prepared admission.
use super::registry::{Definition, FrozenRegistry, Registry, RegistryError};
use std::collections::{BTreeMap, BTreeSet};
use yrs::Any;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvolutionError {
    InvalidEvolution,
    UnsupportedTransition,
    MigrationFailed,
    Registry(RegistryError),
}
impl From<RegistryError> for EvolutionError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedModuleMigration {
    pub type_name: String,
    pub source: Any,
    pub candidate: Any,
}
pub struct ModuleEvolution {
    type_name: String,
    readers: BTreeMap<u32, FrozenRegistry>,
    transitions: BTreeSet<(u32, u32)>,
}
impl ModuleEvolution {
    pub fn new(definitions: &[Any], transitions: &[(u32, u32)]) -> Result<Self, EvolutionError> {
        if definitions.is_empty() || definitions.len() > 32 || transitions.len() > 32 {
            return Err(EvolutionError::InvalidEvolution);
        }
        let first = Definition::from_data(&definitions[0])?;
        let mut readers = BTreeMap::new();
        for raw in definitions {
            let d = Definition::from_data(raw)?;
            if d.type_name != first.type_name
                || d.kind != first.kind
                || readers.contains_key(&d.version)
            {
                return Err(EvolutionError::InvalidEvolution);
            }
            let mut registry = Registry::default();
            registry.register(raw)?;
            readers.insert(d.version, registry.freeze());
        }
        let mut edges = BTreeSet::new();
        for &(from, to) in transitions {
            if from == to
                || !readers.contains_key(&from)
                || !readers.contains_key(&to)
                || !edges.insert((from, to))
            {
                return Err(EvolutionError::InvalidEvolution);
            }
        }
        Ok(Self {
            type_name: first.type_name,
            readers,
            transitions: edges,
        })
    }
    pub fn prepare(
        &self,
        envelope: &Any,
        text: &str,
        target: u32,
        migrate: impl FnOnce(&Any) -> Result<Any, EvolutionError>,
    ) -> Result<PreparedModuleMigration, EvolutionError> {
        let Any::Array(source) = envelope else {
            return Err(EvolutionError::UnsupportedTransition);
        };
        let Some(Any::Number(n)) = source.first() else {
            return Err(EvolutionError::UnsupportedTransition);
        };
        let Some(number) = n.as_f64() else {
            return Err(EvolutionError::UnsupportedTransition);
        };
        if !number.is_finite()
            || number.fract() != 0.0
            || !(1.0..=f64::from(u32::MAX)).contains(&number)
        {
            return Err(EvolutionError::UnsupportedTransition);
        }
        let version = number as u32;
        if source.len() != 2 || !self.transitions.contains(&(version, target)) {
            return Err(EvolutionError::UnsupportedTransition);
        }
        let key = format!("module:{}", self.type_name);
        self.readers[&version].resolve(&key, envelope, text)?;
        let attributes = migrate(&source[1])?;
        let candidate = Any::Array(vec![Any::from(target), attributes].into());
        self.readers[&target].resolve(&key, &candidate, text)?;
        Ok(PreparedModuleMigration {
            type_name: self.type_name.clone(),
            source: envelope.clone(),
            candidate,
        })
    }
}
