//! Model registry: this proxy exposes exactly one upstream model —
//! `deepseek-v4.1-flash`. Client-requested model ids (Claude/Grok model
//! names, unknown ids, unset) all resolve to it; `/v1/models` lists it.

use serde_json::Value;

/// The exposed model definition.
#[derive(Debug, Clone)]
pub struct ModelDefinition {
    pub id: &'static str,
    pub display_name: &'static str,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub supports_tools: bool,
}

/// Result of resolving a client-requested model id.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Id placed on the upstream wire (always the single served model).
    pub upstream_model: String,
}

/// The gateway's model registry (single model by design).
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelRegistry;

impl ModelRegistry {
    pub fn new() -> Self {
        Self
    }

    pub fn definitions(&self) -> &[ModelDefinition] {
        const DEFINITIONS: [ModelDefinition; 1] = [ModelDefinition {
            id: "deepseek-v4.1-flash",
            display_name: "DeepSeek v4.1 Flash (LobsterAI)",
            max_input_tokens: 131_072,
            max_output_tokens: 131_072,
            supports_tools: true,
        }];
        &DEFINITIONS
    }

    /// Resolve a client-requested model id. Everything resolves to the
    /// served model: this is a fixed-model proxy and clients send their own
    /// model names routinely. Unknown explicit ids are mapped, not rejected.
    pub fn resolve(&self, _client_model: Option<&str>, default_upstream: &str) -> ResolvedModel {
        ResolvedModel {
            upstream_model: default_upstream.to_owned(),
        }
    }
}

/// Anthropic `/v1/models` list entry shape.
pub fn model_list_entry(definition: &ModelDefinition, created_at: &str) -> Value {
    serde_json::json!({
        "type": "model",
        "id": definition.id,
        "display_name": definition.display_name,
        "created_at": created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_resolves_to_the_served_model() {
        let registry = ModelRegistry::new();
        for requested in [
            Some("deepseek-v4.1-flash"),
            Some("claude-sonnet-4-5"),
            Some("grok-4"),
            Some(""),
            None,
        ] {
            let resolved = registry.resolve(requested, "deepseek-v4.1-flash");
            assert_eq!(resolved.upstream_model, "deepseek-v4.1-flash");
        }
    }

    #[test]
    fn list_entry_follows_the_anthropic_shape() {
        let registry = ModelRegistry::new();
        let entry = model_list_entry(&registry.definitions()[0], "2026-01-01T00:00:00Z");
        assert_eq!(entry["type"], "model");
        assert_eq!(entry["id"], "deepseek-v4.1-flash");
        assert_eq!(entry["created_at"], "2026-01-01T00:00:00Z");
    }
}
