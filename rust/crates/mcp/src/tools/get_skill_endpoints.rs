use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    #[schemars(description = "Service name (e.g. 'bigquery', 'translate', 'vision')")]
    pub service: String,
}

pub async fn run(params: Params) -> Result<CallToolResult, rmcp::ErrorData> {
    let service_name = params.service.clone();
    let mut catalog = tokio::task::spawn_blocking(pay_core::skills::load_skills)
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    tokio::task::spawn_blocking(move || {
        pay_core::skills::ensure_endpoints(&mut catalog, &service_name).map(|()| catalog)
    })
    .await
    .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
    .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))
    .and_then(|catalog| {
        let svc = catalog
            .providers
            .iter()
            .find(|s| {
                s.fqn.eq_ignore_ascii_case(&params.service)
                    || s.name().eq_ignore_ascii_case(&params.service)
            })
            .ok_or_else(|| {
                rmcp::ErrorData::invalid_params(
                    format!("Service `{}` not found", params.service),
                    None,
                )
            })?;

        let clean = pay_core::skills::SearchResultGroup {
            service: svc.fqn.clone(),
            title: svc.meta.title.clone(),
            url: svc.meta.service_url.clone(),
            endpoints: svc
                .endpoints
                .iter()
                .map(|ep| pay_core::skills::endpoint_to_hit(&svc.meta.service_url, ep))
                .collect(),
        };

        let json = serde_json::to_string_pretty(&clean)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            json,
        )]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_deserialize() {
        let json = r#"{"service": "bigquery"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.service, "bigquery");
    }

    #[test]
    fn params_accepts_fqn() {
        let json = r#"{"service": "solana-foundation/google/bigquery"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert!(params.service.contains('/'));
    }

    #[test]
    fn params_requires_service() {
        let json = r#"{}"#;
        let result = serde_json::from_str::<Params>(json);
        assert!(result.is_err());
    }
}
