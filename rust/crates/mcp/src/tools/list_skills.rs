use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// Force-refresh the catalog from all sources before listing.
    #[schemars(description = "Set to true to force-refresh the catalog from CDN before listing")]
    #[serde(default)]
    pub refresh: bool,
}

/// Lightweight entry returned to the LLM for skill selection.
#[derive(Debug, Serialize)]
struct SkillEntry {
    fqn: String,
    description: String,
    category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    use_case: Option<String>,
    endpoint_count: u32,
    has_metering: bool,
}

pub async fn run(params: Params) -> Result<CallToolResult, rmcp::ErrorData> {
    let catalog = if params.refresh {
        tokio::task::spawn_blocking(pay_core::skills::update_skills)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
    } else {
        tokio::task::spawn_blocking(pay_core::skills::load_skills)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
    };

    let entries: Vec<SkillEntry> = catalog
        .providers
        .iter()
        .map(|svc| SkillEntry {
            fqn: svc.fqn.clone(),
            description: svc.meta.description.clone(),
            category: svc.meta.category.clone(),
            use_case: svc.meta.use_case.clone(),
            endpoint_count: svc.endpoint_count,
            has_metering: svc.has_metering,
        })
        .collect();

    let json = serde_json::to_string_pretty(&entries)
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        json,
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_default_no_refresh() {
        let json = r#"{}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert!(!params.refresh);
    }

    #[test]
    fn params_with_refresh() {
        let json = r#"{"refresh": true}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert!(params.refresh);
    }
}
