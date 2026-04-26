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
}
