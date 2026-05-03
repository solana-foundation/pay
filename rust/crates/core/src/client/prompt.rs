pub(crate) struct PaymentPromptContext {
    pub reason: String,
    pub operator: String,
}

pub(crate) fn payment_prompt_context(
    reason: Option<&str>,
    resource_urls: &[Option<&str>],
) -> PaymentPromptContext {
    PaymentPromptContext {
        reason: reason
            .and_then(normalized_nonempty)
            .unwrap_or_else(|| "API access".to_string()),
        operator: resource_urls
            .iter()
            .filter_map(|url| url.and_then(operator_label))
            .next()
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

pub(crate) fn payment_description(
    challenge_description: Option<&str>,
    resource_urls: &[Option<&str>],
) -> String {
    if let Some(description) = meaningful_description(challenge_description) {
        return description.to_string();
    }

    resource_urls
        .iter()
        .filter_map(|url| url.and_then(api_label))
        .next()
        .map(|label| format!("accessing API {label}"))
        .unwrap_or_else(|| "accessing API".to_string())
}

fn meaningful_description(description: Option<&str>) -> Option<&str> {
    let description = description?.trim();
    if description.is_empty() || is_generic_api_access(description) {
        return None;
    }
    Some(description)
}

fn normalized_nonempty(value: &str) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let value = value.trim();
    (!value.is_empty()).then(|| display_reason(value))
}

fn display_reason(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_lowercase().chain(chars).collect()
}

fn is_generic_api_access(description: &str) -> bool {
    description.eq_ignore_ascii_case("api access")
}

fn api_label(resource_url: &str) -> Option<String> {
    let resource_url = resource_url.trim();
    if resource_url.is_empty() {
        return None;
    }

    let domain = domain_from_url(resource_url)?;
    crate::skills::service_fqn_for_resource_url(resource_url).or(Some(domain))
}

fn operator_label(resource_url: &str) -> Option<String> {
    let domain = domain_from_url(resource_url)?;
    Some(operator_from_domain(&domain))
}

fn operator_from_domain(domain: &str) -> String {
    domain
        .strip_prefix("api.")
        .or_else(|| domain.strip_prefix("www."))
        .unwrap_or(domain)
        .to_string()
}

fn domain_from_url(resource_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(resource_url).ok()?;
    url.host_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_description_preserves_specific_challenge_description() {
        assert_eq!(
            payment_description(
                Some("Run a SQL query"),
                &[Some("https://api.example.com/v1/query")]
            ),
            "Run a SQL query"
        );
    }

    #[test]
    fn payment_description_replaces_generic_api_access_with_domain() {
        assert_eq!(
            payment_description(
                Some("API access"),
                &[Some("https://api.example.com/v1/query")]
            ),
            "accessing API api.example.com"
        );
    }

    #[test]
    fn payment_description_uses_domain_when_description_is_missing() {
        assert_eq!(
            payment_description(None, &[Some("https://api.example.com/v1/query")]),
            "accessing API api.example.com"
        );
    }

    #[test]
    fn payment_description_ignores_empty_resource_candidates() {
        assert_eq!(
            payment_description(Some("API access"), &[Some(""), None]),
            "accessing API"
        );
    }

    #[test]
    fn payment_prompt_context_uses_challenge_description_and_operator() {
        let context = payment_prompt_context(
            Some(" Run   a SQL query "),
            &[Some("https://api.gateway-402.com/v1/query")],
        );

        assert_eq!(context.reason, "run a SQL query");
        assert_eq!(context.operator, "gateway-402.com");
    }

    #[test]
    fn payment_prompt_context_lowercases_reason_first_letter() {
        let context = payment_prompt_context(
            Some(" Run   request "),
            &[Some("https://api.gateway-402.com/v1/query")],
        );

        assert_eq!(context.reason, "run request");
        assert_eq!(context.operator, "gateway-402.com");
    }

    #[test]
    fn payment_prompt_context_omits_empty_description() {
        let context = payment_prompt_context(Some("  "), &[Some("https://example.com/v1")]);

        assert_eq!(context.reason, "API access");
        assert_eq!(context.operator, "example.com");
    }
}
