//! Advisory endpoint context, never an OAuth audience or a fetch destination.

use crate::protocol::ApiError;

pub(crate) fn validate(value: &str) -> Result<(), ApiError> {
    let invalid = || ApiError::bad_request("invalid_request", "Invalid resource_uri.");
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    // Require an explicit authority; do not accept URL parser repair of
    // relative-looking inputs, whitespace, or backslashes.
    if value.chars().any(char::is_whitespace) || value.contains('\\') {
        return Err(invalid());
    }
    let url = url::Url::parse(value).map_err(|_| invalid())?;
    let authority = value
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .ok_or_else(invalid)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none_or(str::is_empty)
        || authority.is_empty()
        || authority.contains('@')
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn from_query(query: Option<&str>) -> Result<Option<String>, ApiError> {
    let mut resource = None;
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if key == "resource_uri" {
            if resource.is_some() {
                return Err(ApiError::bad_request(
                    "invalid_request",
                    "Duplicate resource_uri.",
                ));
            }
            validate(&value)?;
            resource = Some(value.into_owned());
        }
    }
    Ok(resource)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_without_normalizing() {
        for value in [
            "HTTPS://Example.COM:443/a?x=a%2Fb&x=two+words",
            "http://localhost:3000/paid",
        ] {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("resource_uri", value)
                .finish();
            assert_eq!(from_query(Some(&query)).unwrap().as_deref(), Some(value));
        }
        for value in [
            "",
            "/relative",
            "https:example.com",
            "https:///example.com",
            "file://example.com/a",
            "https://u:p@example.com",
            "https://@example.com",
            "https://example.com/#",
            " https://example.com",
            "https://example.com\n",
            "https://example.com\\evil",
            "https://example.com/\u{7f}",
        ] {
            assert!(validate(value).is_err(), "{value:?}");
        }
        let boundary = format!("https://example.com/{}", "a".repeat(4076));
        assert_eq!(boundary.len(), 4096);
        assert!(validate(&boundary).is_ok());
        assert!(validate(&(boundary + "a")).is_err());
        assert!(
            from_query(Some(
                "resource_uri=https://a.test&resource_uri=https://a.test"
            ))
            .is_err()
        );
        assert!(from_query(None).unwrap().is_none());
    }
}
