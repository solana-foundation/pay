use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    #[schemars(description = "Search keyword (e.g. 'bigquery', 'translate', 'vision')")]
    pub query: Option<String>,
    #[schemars(
        description = "Filter by category: ai_ml, data, compute, maps, search, translation, productivity"
    )]
    pub category: Option<String>,
}

pub async fn run(params: Params) -> Result<CallToolResult, rmcp::ErrorData> {
    let catalog = tokio::task::spawn_blocking(pay_core::skills::load_skills)
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    let hits = pay_core::skills::search(
        &catalog,
        params.query.as_deref(),
        params.category.as_deref(),
    );

    let grouped = pay_core::skills::group_search_results(&hits);
    let condensed: Vec<_> = grouped
        .into_iter()
        .map(|mut g| {
            let metered: Vec<_> = g.endpoints.iter().filter(|e| e.metered).cloned().collect();
            let free: Vec<_> = g.endpoints.iter().filter(|e| !e.metered).cloned().collect();
            let mut capped: Vec<_> = metered.into_iter().take(5).collect();
            capped.extend(free.into_iter().take(3));
            g.endpoints = capped;
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
    fn params_all_optional() {
        let json = r#"{}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert!(params.query.is_none());
        assert!(params.category.is_none());
    }

    #[test]
    fn params_with_query() {
        let json = r#"{"query": "bigquery"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.query.unwrap(), "bigquery");
    }

    #[test]
    fn params_with_category() {
        let json = r#"{"category": "ai_ml"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.category.unwrap(), "ai_ml");
    }
}
