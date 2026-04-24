use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// The full `.md` file content (YAML frontmatter + markdown body).
    #[schemars(
        description = "Full .md file content with YAML frontmatter between --- delimiters, followed by markdown body"
    )]
    pub content: String,

    /// Optional path to write the validated file to disk.
    #[schemars(
        description = "Optional: file path to write the validated .md file (e.g. providers/myorg/my-api.md)"
    )]
    pub output_path: Option<String>,
}

#[derive(Debug)]
pub struct ValidatedProvider {
    pub spec: pay_types::registry::ProviderFrontmatter,
}

pub async fn run(params: Params) -> Result<CallToolResult, rmcp::ErrorData> {
    let content = params.content.clone();
    let output_path = params.output_path.clone();

    let result = tokio::task::spawn_blocking(move || validate(&content))
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    match result {
        Ok(validated) => {
            let spec_json = serde_json::to_string_pretty(&validated.spec).unwrap_or_default();
            let mut response = format!(
                "Provider spec is valid ({} endpoints).\n\n```json\n{spec_json}\n```\n",
                validated.spec.endpoints.len(),
            );

            if let Some(path) = output_path {
                match std::fs::create_dir_all(
                    std::path::Path::new(&path)
                        .parent()
                        .unwrap_or(std::path::Path::new(".")),
                )
                .and_then(|_| std::fs::write(&path, &params.content))
                {
                    Ok(_) => {
                        response.push_str(&format!("\nWrote to: {path}\n"));
                    }
                    Err(e) => {
                        response.push_str(&format!("\nFailed to write to {path}: {e}\n"));
                    }
                }
            } else {
                response.push_str(&format!(
                    "\n## Next steps\n\n\
                     1. Fork https://github.com/solana-foundation/pay-skills\n\
                     2. Add this file as `providers/<org>/{}.md`\n\
                     3. Open a PR — CI will validate automatically\n",
                    validated.spec.name
                ));
            }

            Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                response,
            )]))
        }
        Err(errors) => {
            let mut response = format!("Validation failed with {} error(s):\n\n", errors.len());
            for err in &errors {
                response.push_str(&format!("- {err}\n"));
            }
            let schema_json = pay_types::registry::provider_json_schema();
            response.push_str(&format!(
                "\n## JSON Schema for provider frontmatter\n\n```json\n{schema_json}\n```\n"
            ));

            Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                response,
            )]))
        }
    }
}

pub fn validate(content: &str) -> Result<ValidatedProvider, Vec<String>> {
    let mut errors = Vec::new();

    let (yaml_str, _body) = match pay_core::skills::build::parse_frontmatter(content) {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("frontmatter parse error: {e}"));
            return Err(errors);
        }
    };

    if yaml_str.is_empty() {
        errors.push("no YAML frontmatter found — file must start with ---".to_string());
        return Err(errors);
    }

    let spec: pay_types::registry::ProviderFrontmatter = match serde_yml::from_str(&yaml_str) {
        Ok(s) => s,
        Err(e) => {
            errors.push(format!("YAML parse error: {e}"));
            errors
                .push("check that all required fields are present and correctly typed".to_string());
            return Err(errors);
        }
    };

    let validation_errors = pay_types::registry::validate_provider(&spec, &spec.name);
    if !validation_errors.is_empty() {
        return Err(validation_errors);
    }

    if spec.meta.description.len() > 255 {
        errors.push(format!(
            "description is {} chars (max 255): \"{}\"",
            spec.meta.description.len(),
            spec.meta.description
        ));
    }

    if !spec.meta.service_url.starts_with("https://") {
        errors.push(format!(
            "service_url must start with https:// (got \"{}\")",
            spec.meta.service_url
        ));
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(ValidatedProvider { spec })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_md() -> &'static str {
        "---\nname: test-api\ntitle: \"Test API\"\ndescription: \"A test API for unit tests\"\ncategory: devtools\nservice_url: https://test.example.com\nendpoints:\n  - method: POST\n    path: \"v1/run\"\n    description: \"Run a test\"\n---\n\nSome markdown body.\n"
    }

    #[test]
    fn validate_valid_spec() {
        let result = validate(valid_md());
        assert!(result.is_ok());
        let v = result.unwrap();
        assert_eq!(v.spec.name, "test-api");
        assert_eq!(v.spec.endpoints.len(), 1);
    }

    #[test]
    fn validate_no_frontmatter() {
        let result = validate("Just some text, no frontmatter");
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert!(errs.iter().any(|e| e.contains("frontmatter")));
    }

    #[test]
    fn validate_empty_frontmatter() {
        let result = validate("---\n---\n");
        assert!(result.is_err());
    }

    #[test]
    fn validate_missing_required_fields() {
        let result = validate("---\nname: x\n---\n");
        assert!(result.is_err());
        let errs = result.unwrap_err();
        // Fields default to empty strings, caught by validate_provider
        assert!(!errs.is_empty());
    }

    #[test]
    fn validate_bad_category() {
        let md = "---\nname: x\ntitle: X\ndescription: X\ncategory: nonsense\nservice_url: https://x.com\nendpoints:\n  - method: GET\n    path: v1\n    description: Do thing\n---\n";
        let result = validate(md);
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert!(errs.iter().any(|e| e.contains("unknown category")));
    }

    #[test]
    fn validate_no_endpoints() {
        let md = "---\nname: x\ntitle: X\ndescription: X\ncategory: data\nservice_url: https://x.com\nendpoints: []\n---\n";
        let result = validate(md);
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert!(errs.iter().any(|e| e.contains("at least one endpoint")));
    }

    #[test]
    fn validate_long_description() {
        let long_desc = "A".repeat(121);
        let md = format!(
            "---\nname: x\ntitle: X\ndescription: \"{long_desc}\"\ncategory: data\nservice_url: https://x.com\nendpoints:\n  - method: GET\n    path: v1\n    description: Do thing\n---\n"
        );
        let result = validate(&md);
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert!(errs.iter().any(|e| e.contains("255")));
    }

    #[test]
    fn validate_http_service_url() {
        let md = "---\nname: x\ntitle: X\ndescription: X\ncategory: data\nservice_url: http://insecure.com\nendpoints:\n  - method: GET\n    path: v1\n    description: Do thing\n---\n";
        let result = validate(md);
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert!(errs.iter().any(|e| e.contains("https://")));
    }

    #[test]
    fn validate_endpoint_missing_method() {
        let md = "---\nname: x\ntitle: X\ndescription: X\ncategory: data\nservice_url: https://x.com\nendpoints:\n  - path: v1\n    description: Do thing\n---\n";
        let result = validate(md);
        assert!(result.is_err());
    }

    #[test]
    fn validate_endpoint_missing_description() {
        // EndpointSpec.description defaults to "" via serde, which validate_provider catches
        let md = "---\nname: x\ntitle: X\ndescription: X\ncategory: data\nservice_url: https://x.com\nendpoints:\n  - method: GET\n    path: v1\n    description: \"\"\n---\n";
        let result = validate(md);
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert!(errs.iter().any(|e| e.contains("description")));
    }

    #[test]
    fn validate_with_pricing() {
        let md = "---\nname: x\ntitle: X\ndescription: X\ncategory: data\nservice_url: https://x.com\nendpoints:\n  - method: POST\n    path: v1/search\n    description: Search\n    pricing:\n      dimensions:\n        - direction: usage\n          unit: requests\n          scale: 1\n          tiers:\n            - price_usd: 0.01\n---\n";
        let result = validate(md);
        assert!(result.is_ok());
        let v = result.unwrap();
        assert!(v.spec.endpoints[0].pricing.is_some());
    }

    #[test]
    fn validate_with_optional_fields() {
        let md = "---\nname: x\ntitle: X\ndescription: X\ncategory: data\nservice_url: https://x.com\nversion: v2\nopenapi_url: https://x.com/openapi.json\naffiliate_policy:\n  enabled: true\n  default_percent: 10\nendpoints:\n  - method: GET\n    path: v1\n    description: Do thing\n    resource: things\n---\n";
        let result = validate(md);
        assert!(result.is_ok());
        let v = result.unwrap();
        assert_eq!(v.spec.version, "v2");
        assert!(v.spec.openapi_url.is_some());
        assert!(v.spec.affiliate_policy.is_some());
        assert_eq!(v.spec.endpoints[0].resource.as_deref(), Some("things"));
    }

    #[test]
    fn params_deserialize() {
        let json = r#"{"content": "---\nname: test\n---\n"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert!(params.content.contains("name: test"));
        assert!(params.output_path.is_none());
    }

    #[test]
    fn params_with_output_path() {
        let json = r#"{"content": "---\n---\n", "output_path": "/tmp/test.md"}"#;
        let params: Params = serde_json::from_str(json).unwrap();
        assert_eq!(params.output_path.unwrap(), "/tmp/test.md");
    }
}
