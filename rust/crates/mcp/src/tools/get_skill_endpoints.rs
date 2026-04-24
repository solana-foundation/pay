use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    #[schemars(
        description = "Fully qualified name returned by search_skills (e.g. 'solana-foundation/google/bigquery')"
    )]
    pub fqn: String,
}

/// Full skill detail returned to the LLM after selection.
#[derive(Debug, Serialize)]
struct SkillDetail {
    fqn: String,
    title: String,
    description: String,
    service_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_service_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    use_case: Option<String>,
    /// Usage notes from the detail file (markdown body of the .md file).
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    endpoints: Vec<EndpointEntry>,
}

#[derive(Debug, Serialize)]
struct EndpointEntry {
    method: String,
    path: String,
    url: String,
    description: String,
    metered: bool,
}

/// Minimal shape to extract `content` from the raw detail JSON.
#[derive(Debug, Deserialize)]
struct DetailContent {
    #[serde(default)]
    content: Option<String>,
}

pub async fn run(params: Params) -> Result<CallToolResult, rmcp::ErrorData> {
    let fqn = params.fqn.clone();
    let mut catalog = tokio::task::spawn_blocking(pay_core::skills::load_skills)
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    // Load endpoints from detail file
    let fqn_clone = fqn.clone();
    let catalog = tokio::task::spawn_blocking(move || {
        pay_core::skills::ensure_endpoints(&mut catalog, &fqn_clone).map(|()| catalog)
    })
    .await
    .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
    .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    let svc = catalog
        .providers
        .iter()
        .find(|s| s.fqn.eq_ignore_ascii_case(&fqn) || s.name().eq_ignore_ascii_case(&fqn))
        .ok_or_else(|| {
            rmcp::ErrorData::invalid_params(format!("Service `{}` not found", fqn), None)
        })?;

    // Read content from the cached detail file (not in the Service struct by design)
    let content = read_detail_content(&svc.sha);

    let base_url = &svc.meta.service_url;
    let detail = SkillDetail {
        fqn: svc.fqn.clone(),
        title: svc.meta.title.clone(),
        description: svc.meta.description.clone(),
        service_url: svc.meta.service_url.clone(),
        sandbox_service_url: svc.meta.sandbox_service_url.clone(),
        use_case: svc.meta.use_case.clone(),
        content,
        endpoints: svc
            .endpoints
            .iter()
            .map(|ep| EndpointEntry {
                method: ep.method.clone(),
                path: ep.path.clone(),
                url: format!("{}/{}", base_url.trim_end_matches('/'), &ep.path),
                description: ep.description.clone(),
                metered: ep.pricing.is_some(),
            })
            .collect(),
    };

    let json = serde_json::to_string_pretty(&detail)
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        json,
    )]))
}

/// Read `content` from the cached detail JSON file on disk.
fn read_detail_content(sha: &str) -> Option<String> {
    if sha.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").ok()?;
    let cache_file = std::path::PathBuf::from(home)
        .join(".config/pay/skills/detail")
        .join(format!("{sha}.json"));
    let raw = std::fs::read_to_string(cache_file).ok()?;
    let detail: DetailContent = serde_json::from_str(&raw).ok()?;
    detail.content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_deserialize() {
        let json = r#"{"fqn": "solana-foundation/google/bigquery"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.fqn, "solana-foundation/google/bigquery");
    }

    #[test]
    fn params_requires_fqn() {
        let json = r#"{}"#;
        let result = serde_json::from_str::<Params>(json);
        assert!(result.is_err());
    }
}
