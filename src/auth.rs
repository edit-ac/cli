//! Host-configured credential storage. This port never discovers ambient keys.
//! Service adapters consume credentials; documents and diagnostics cannot serialize them.

pub struct Credential(String);

impl Credential {
    pub fn new(value: String) -> Result<Self, AuthError> {
        if value.is_empty() || value.len() > 8192 || value.chars().any(char::is_control) {
            return Err(AuthError::InvalidCredential);
        }
        Ok(Self(value))
    }

    /// Only the configured authorization adapter should consume this value.
    pub fn expose_to_adapter(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential([redacted])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    Unavailable,
    InvalidCredential,
}

/// Implemented by the embedding host's existing credential store. No filesystem,
/// environment-variable or interactive-login fallback is performed by the client.
/// Errors are codes, so provider messages cannot accidentally disclose tokens.
pub trait CredentialStore {
    fn load(&mut self, service_endpoint: &str) -> Result<Option<Credential>, AuthError>;
}
