//! Named, persistent Scheme workers shared by every client of this endpoint.

use std::{collections::BTreeMap, sync::Arc};

use rmcp::ErrorData as McpError;

use crate::scheme::SchemeHandle;

pub const DEFAULT_SESSION: &str = "default";
pub const MAX_SESSIONS: usize = 16;

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "session names must contain 1–64 ASCII letters, digits, hyphens, or underscores"
                .to_owned(),
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Sessions {
    default: SchemeHandle,
    additional: Arc<BTreeMap<String, SchemeHandle>>,
}

impl Sessions {
    pub fn new(default: SchemeHandle) -> Self {
        Self {
            default,
            additional: Arc::default(),
        }
    }

    pub fn insert(&mut self, name: String, worker: SchemeHandle) -> Result<(), String> {
        validate_name(&name)?;
        if name == DEFAULT_SESSION || self.additional.contains_key(&name) {
            return Err(format!("duplicate session name: {name}"));
        }
        if self.additional.len() + 1 >= MAX_SESSIONS {
            return Err(format!("at most {MAX_SESSIONS} sessions may be configured"));
        }
        Arc::make_mut(&mut self.additional).insert(name, worker);
        Ok(())
    }

    pub fn default_worker(&self) -> &SchemeHandle {
        &self.default
    }

    pub fn get(&self, name: Option<&str>) -> Result<&SchemeHandle, McpError> {
        let name = name.unwrap_or(DEFAULT_SESSION);
        if name == DEFAULT_SESSION {
            return Ok(&self.default);
        }
        self.additional.get(name).ok_or_else(|| {
            McpError::invalid_params(
                format!("unknown Blender session: {name}"),
                Some(serde_json::json!({"code": "unknown_session", "session": name})),
            )
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &SchemeHandle)> {
        std::iter::once((DEFAULT_SESSION, &self.default)).chain(
            self.additional
                .iter()
                .map(|(name, worker)| (name.as_str(), worker)),
        )
    }
}

pub(crate) fn artifact_uri(session: &str, id: &str) -> String {
    if session == DEFAULT_SESSION {
        format!("blender-mcp://artifact/{id}")
    } else {
        format!("resources://blender/sessions/{session}/artifact/{id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_are_unambiguous_uri_segments() {
        for name in ["default", "game-2", "ai_preview", "a"] {
            assert!(validate_name(name).is_ok());
        }
        for name in ["", "..", "a/b", "a%2fb", "a?b", "a#b", "a b", "é"] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        assert!(validate_name(&"a".repeat(65)).is_err());
    }
}
