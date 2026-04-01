//! HTTP reverse proxy — forwards requests to upstream APIs.
//!
//! Resolves the upstream from `ApiSpec.forward_url`, forwards headers and body,
//! returns the upstream response. Strips hop-by-hop and payment headers.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use pay_types::metering::{ApiSpec, AuthConfig};
use serde_json::json;
use tokio::sync::RwLock;

/// Headers to strip when forwarding to upstream.
const STRIP_HEADERS: &[&str] = &[
    "host",
    "connection",
    "transfer-encoding",
    "authorization",
    "payment-signature",
    "payment-required",
];

/// Forward a request to the upstream API defined in the spec.
///
/// - Builds the upstream URL from `api.forward.url` + request path
/// - Forwards all headers except hop-by-hop and payment headers
/// - Forwards the request body as-is
/// - Returns the upstream response (status, headers, body)
pub async fn forward_request(
    api: &ApiSpec,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());

    // Build upstream URL (with path rewrites), then inject query param auth if configured.
    let mut upstream_url = api.forward.upstream_url(path_and_query);

    if let Some(AuthConfig::QueryParam {
        key,
        value_from_env,
    }) = &api.forward.auth
    {
        let secret = std::env::var(value_from_env).unwrap_or_default();
        let separator = if upstream_url.contains('?') { "&" } else { "?" };
        upstream_url = format!("{upstream_url}{separator}{key}={secret}");
    }

    tracing::debug!(
        subdomain = %api.subdomain,
        upstream = %upstream_url,
        "Forwarding request"
    );

    let client = reqwest::Client::new();
    let mut upstream_req = client.request(
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap(),
        &upstream_url,
    );

    // Forward headers.
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        if STRIP_HEADERS.contains(&name_str) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            upstream_req = upstream_req.header(name_str, v);
        }
    }

    // Inject auth header if configured.
    match &api.forward.auth {
        Some(AuthConfig::Header {
            key,
            prefix,
            value_from_env,
        }) => {
            let secret = std::env::var(value_from_env).unwrap_or_default();
            let value = match prefix {
                Some(p) => format!("{p}{secret}"),
                None => secret,
            };
            upstream_req = upstream_req.header(key.as_str(), value);
        }
        Some(AuthConfig::Oauth2 {
            token_url,
            scopes,
            client_id_from_env,
            client_secret_from_env,
            headers,
        }) => {
            match oauth2_token(
                token_url,
                scopes,
                client_id_from_env.as_deref(),
                client_secret_from_env.as_deref(),
            )
            .await
            {
                Ok(token) => {
                    upstream_req = upstream_req.header("authorization", format!("Bearer {token}"));
                    for (header_name, env_ref) in headers {
                        if let Ok(val) = std::env::var(&env_ref.from_env) {
                            upstream_req = upstream_req.header(header_name.as_str(), val);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to fetch OAuth2 token");
                    return Err(error_response(
                        StatusCode::BAD_GATEWAY,
                        &format!("OAuth2 token error: {e}"),
                    ));
                }
            }
        }
        _ => {}
    }

    // Forward body.
    if !body.is_empty() {
        upstream_req = upstream_req.body(body.to_vec());
    }

    let upstream_resp = upstream_req.send().await.map_err(|e| {
        tracing::error!(error = %e, upstream = %upstream_url, "Upstream request failed");
        error_response(StatusCode::BAD_GATEWAY, &format!("Upstream error: {e}"))
    })?;

    // Build response.
    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response_headers = HeaderMap::new();
    // Skip headers that reqwest handles (it auto-decompresses gzip).
    let skip_response_headers = ["content-encoding", "content-length", "transfer-encoding"];
    for (name, value) in upstream_resp.headers() {
        let name_lower = name.as_str();
        if skip_response_headers.contains(&name_lower) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            axum::http::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            response_headers.insert(n, v);
        }
    }

    let response_body = upstream_resp.bytes().await.map_err(|e| {
        error_response(
            StatusCode::BAD_GATEWAY,
            &format!("Upstream body read error: {e}"),
        )
    })?;

    let mut resp = Response::builder().status(status);
    for (name, value) in &response_headers {
        resp = resp.header(name, value);
    }

    Ok(resp.body(Body::from(response_body)).unwrap())
}

/// Resolve the API spec from a Host header subdomain.
pub fn resolve_api<'a>(apis: &'a [ApiSpec], host: &str) -> Option<&'a ApiSpec> {
    let subdomain = host.split('.').next().unwrap_or("");
    apis.iter().find(|a| a.subdomain == subdomain)
}

pub fn error_response(status: StatusCode, message: &str) -> Response {
    let body = json!({
        "error": status.as_str(),
        "message": message,
    });

    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
}

// =============================================================================
// GCP OAuth2 token cache
// =============================================================================

/// Cached OAuth2 token with expiry.
struct CachedToken {
    access_token: String,
    expires_at: std::time::Instant,
}

static OAUTH2_TOKEN_CACHE: std::sync::OnceLock<Arc<RwLock<Option<CachedToken>>>> =
    std::sync::OnceLock::new();

fn token_cache() -> &'static Arc<RwLock<Option<CachedToken>>> {
    OAUTH2_TOKEN_CACHE.get_or_init(|| Arc::new(RwLock::new(None)))
}

/// Fetch an OAuth2 access token, using a cached value if still valid.
async fn oauth2_token(
    token_url: &str,
    scopes: &[String],
    client_id_env: Option<&str>,
    client_secret_env: Option<&str>,
) -> Result<String, String> {
    // Check cache.
    {
        let cache = token_cache().read().await;
        if let Some(ref cached) = *cache
            && cached.expires_at > std::time::Instant::now() + std::time::Duration::from_secs(30)
        {
            return Ok(cached.access_token.clone());
        }
    }

    let token = fetch_oauth2_token(token_url, scopes, client_id_env, client_secret_env).await?;

    // Cache (assume ~1h validity, refresh 5min early).
    {
        let mut cache = token_cache().write().await;
        *cache = Some(CachedToken {
            access_token: token.clone(),
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(3300),
        });
    }

    Ok(token)
}

async fn fetch_oauth2_token(
    token_url: &str,
    scopes: &[String],
    client_id_env: Option<&str>,
    client_secret_env: Option<&str>,
) -> Result<String, String> {
    let client = reqwest::Client::new();

    // Special: GCP metadata server.
    if token_url == "gcp_metadata" {
        return fetch_gcp_metadata_token(&client, scopes).await;
    }

    // Standard OAuth2 client_credentials grant.
    let client_id = client_id_env
        .and_then(|e| std::env::var(e).ok())
        .ok_or("OAuth2 client_id env var not set")?;
    let client_secret = client_secret_env
        .and_then(|e| std::env::var(e).ok())
        .ok_or("OAuth2 client_secret env var not set")?;

    let mut params = vec![
        ("grant_type", "client_credentials".to_string()),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ];
    if !scopes.is_empty() {
        params.push(("scope", scopes.join(" ")));
    }

    let resp = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("OAuth2 token request failed: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Invalid OAuth2 response: {e}"))?;

    body["access_token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("No access_token in response: {body}"))
}

/// Fetch token from GCP metadata server (Cloud Run / GCE).
/// Falls back to Application Default Credentials for local dev.
async fn fetch_gcp_metadata_token(
    client: &reqwest::Client,
    scopes: &[String],
) -> Result<String, String> {
    let scopes_param = scopes.join(",");

    // 1. Metadata server.
    let url = format!(
        "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token?scopes={scopes_param}"
    );
    if let Ok(resp) = client
        .get(&url)
        .header("Metadata-Flavor", "Google")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        && resp.status().is_success()
        && let Ok(body) = resp.json::<serde_json::Value>().await
        && let Some(token) = body["access_token"].as_str()
    {
        tracing::debug!("OAuth2 token from GCP metadata server");
        return Ok(token.to_string());
    }

    // 2. Application Default Credentials (local dev).
    let adc_path = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.config/gcloud/application_default_credentials.json")
    });
    let adc_content = std::fs::read_to_string(&adc_path)
        .map_err(|e| format!("No metadata server and can't read ADC at {adc_path}: {e}"))?;
    let adc: serde_json::Value =
        serde_json::from_str(&adc_content).map_err(|e| format!("Invalid ADC: {e}"))?;

    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", adc["client_id"].as_str().unwrap_or_default()),
            (
                "client_secret",
                adc["client_secret"].as_str().unwrap_or_default(),
            ),
            (
                "refresh_token",
                adc["refresh_token"].as_str().unwrap_or_default(),
            ),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|e| format!("ADC token refresh failed: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Invalid token response: {e}"))?;

    body["access_token"]
        .as_str()
        .map(|s| {
            tracing::debug!("OAuth2 token from ADC");
            s.to_string()
        })
        .ok_or_else(|| format!("No access_token: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_types::metering::ForwardConfig;

    fn make_api(subdomain: &str) -> ApiSpec {
        ApiSpec {
            name: "test".to_string(),
            subdomain: subdomain.to_string(),
            title: "Test".to_string(),
            description: "".to_string(),
            category: pay_types::metering::ApiCategory::AiMl,
            version: "1.0".to_string(),
            env: std::collections::HashMap::new(),
            forward: ForwardConfig {
                url: "https://api.example.com".to_string(),
                path_rewrites: vec![],
                auth: None,
            },
            accounting: pay_types::metering::AccountingMode::Pooled,
            endpoints: vec![],
            free_tier: None,
            quotas: None,
            notes: None,
            operator: None,
        }
    }

    #[test]
    fn resolve_api_finds_matching_subdomain() {
        let apis = vec![make_api("vision"), make_api("translate")];
        let result = resolve_api(&apis, "vision.agents.solana.com");
        assert!(result.is_some());
        assert_eq!(result.unwrap().subdomain, "vision");
    }

    #[test]
    fn resolve_api_no_match() {
        let apis = vec![make_api("vision")];
        let result = resolve_api(&apis, "translate.agents.solana.com");
        assert!(result.is_none());
    }

    #[test]
    fn resolve_api_empty_list() {
        let apis: Vec<ApiSpec> = vec![];
        assert!(resolve_api(&apis, "vision.agents.solana.com").is_none());
    }

    #[test]
    fn error_response_has_correct_status() {
        let resp = error_response(StatusCode::BAD_GATEWAY, "upstream error");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn error_response_has_json_content_type() {
        let resp = error_response(StatusCode::INTERNAL_SERVER_ERROR, "oops");
        let ct = resp.headers().get("content-type").unwrap();
        assert_eq!(ct, "application/json");
    }

    #[test]
    fn strip_headers_contains_expected() {
        assert!(STRIP_HEADERS.contains(&"host"));
        assert!(STRIP_HEADERS.contains(&"authorization"));
        assert!(STRIP_HEADERS.contains(&"connection"));
    }

    /// Spin up a one-shot axum server, return its base URL.
    async fn spawn_upstream(handler: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            axum::serve(listener, handler).await.ok();
        });
        // Give the server a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        (url, handle)
    }

    #[tokio::test]
    async fn forward_request_get() {
        let app = axum::Router::new().route(
            "/v1/test",
            axum::routing::get(|| async { "hello from upstream" }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            forward: ForwardConfig {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/test").parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"hello from upstream");
    }

    #[tokio::test]
    async fn forward_request_post_with_body() {
        let app = axum::Router::new().route(
            "/v1/echo",
            axum::routing::post(|body: String| async move { format!("echo: {body}") }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            forward: ForwardConfig {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/echo").parse().unwrap();
        let body = Bytes::from("test payload");
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/plain".parse().unwrap());

        let result = forward_request(&api, Method::POST, &uri, &headers, body).await;

        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"echo: test payload");
    }

    #[tokio::test]
    async fn forward_request_strips_auth_header() {
        use std::sync::{Arc, Mutex};

        let received_headers: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = received_headers.clone();

        let app = axum::Router::new().route(
            "/v1/check",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let keys: Vec<String> = headers.keys().map(|k| k.to_string()).collect();
                captured.lock().unwrap().extend(keys);
                async { "ok" }
            }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            forward: ForwardConfig {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/check").parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("x-custom", "kept".parse().unwrap());

        let result = forward_request(&api, Method::GET, &uri, &headers, Bytes::new()).await;
        assert!(result.is_ok());

        let fwd = received_headers.lock().unwrap();
        assert!(!fwd.contains(&"authorization".to_string()));
        assert!(fwd.contains(&"x-custom".to_string()));
    }

    #[tokio::test]
    async fn forward_request_preserves_status_code() {
        let app = axum::Router::new().route(
            "/v1/notfound",
            axum::routing::get(|| async { (StatusCode::NOT_FOUND, "nope") }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            forward: ForwardConfig {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/notfound").parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        assert_eq!(result.unwrap().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn forward_request_upstream_down() {
        let api = ApiSpec {
            forward: ForwardConfig {
                url: "http://127.0.0.1:1".to_string(),
                path_rewrites: vec![],
                auth: None,
            }, // nothing listening
            ..make_api("test")
        };

        let uri: Uri = "http://127.0.0.1:1/v1/test".parse().unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        // Should return an error response (502 Bad Gateway)
        let err_resp = result.unwrap_err();
        assert_eq!(err_resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn forward_request_preserves_query_string() {
        let app = axum::Router::new().route(
            "/v1/search",
            axum::routing::get(|uri: axum::http::Uri| async move {
                uri.query().unwrap_or("none").to_string()
            }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        let api = ApiSpec {
            forward: ForwardConfig {
                url: base_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            ..make_api("test")
        };

        let uri: Uri = format!("{base_url}/v1/search?q=hello&limit=10")
            .parse()
            .unwrap();
        let result =
            forward_request(&api, Method::GET, &uri, &HeaderMap::new(), Bytes::new()).await;

        let resp = result.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let qs = String::from_utf8(body.to_vec()).unwrap();
        assert!(qs.contains("q=hello"));
        assert!(qs.contains("limit=10"));
    }

    #[tokio::test]
    async fn forward_request_with_path_rewrite() {
        use pay_types::metering::PathRewrite;

        // Upstream expects the operator's project ID in the path.
        let app = axum::Router::new().route(
            "/v3/projects/operator-proj/translate",
            axum::routing::post(|| async { "translated" }),
        );
        let (base_url, _handle) = spawn_upstream(app).await;

        // SAFETY: test-only, single-threaded
        unsafe { std::env::set_var("_TEST_FWD_PROJECT", "operator-proj") };
        let api = ApiSpec {
            forward: ForwardConfig {
                url: base_url.clone(),
                path_rewrites: vec![PathRewrite {
                    prefix: "v3/projects/{projectId}".to_string(),
                    env: "_TEST_FWD_PROJECT".to_string(),
                }],
                auth: None,
            },
            ..make_api("test")
        };

        // Client sends their own project ID — rewrite substitutes it.
        let uri: Uri = format!("{base_url}/v3/projects/client-proj/translate")
            .parse()
            .unwrap();
        let result =
            forward_request(&api, Method::POST, &uri, &HeaderMap::new(), Bytes::new()).await;

        let resp = result.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"translated");

        unsafe { std::env::remove_var("_TEST_FWD_PROJECT") };
    }
}
