use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// Force-refresh the catalog from all sources before listing.
    #[schemars(description = "Set to true to force-refresh the catalog from CDN before listing")]
    #[serde(default)]
    pub refresh: bool,
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

    let hits = pay_core::skills::search(&catalog, None, None);
    let grouped = pay_core::skills::group_search_results(&hits);
    let condensed: Vec<_> = grouped
        .into_iter()
        .map(|mut g| {
            let metered: Vec<_> = g.endpoints.iter().filter(|e| e.metered).cloned().collect();
            g.endpoints = metered.into_iter().take(3).collect();
            g
        })
        .collect();

    let json = serde_json::to_string_pretty(&condensed)
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
