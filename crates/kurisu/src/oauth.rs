//! OAuth 2.1 discovery, PKCE S256, authorize URL generation, and dynamic client registration (RFC 7591).

#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Injected HTTP client trait so network operations can be mocked in offline unit tests.
pub trait HttpClient: Send + Sync {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<HttpResponse>>;
    fn post_json<'a>(&'a self, url: &'a str, body: &'a Value) -> BoxFuture<'a, Result<HttpResponse>>;
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: HashMap::new(),
            body: body.into(),
        }
    }

    pub fn with_header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.insert(k.into().to_ascii_lowercase(), v.into());
        self
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(&self.body).context("failed to parse JSON response")
    }
}

impl HttpClient for reqwest::Client {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            let res = self.get(url).send().await?;
            let status = res.status().as_u16();
            let mut headers = HashMap::new();
            for (k, v) in res.headers() {
                if let Ok(v_str) = v.to_str() {
                    headers.insert(k.as_str().to_ascii_lowercase(), v_str.to_string());
                }
            }
            let body = res.text().await?;
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }

    fn post_json<'a>(&'a self, url: &'a str, body: &'a Value) -> BoxFuture<'a, Result<HttpResponse>> {
        Box::pin(async move {
            let res = self.post(url).json(body).send().await?;
            let status = res.status().as_u16();
            let mut headers = HashMap::new();
            for (k, v) in res.headers() {
                if let Ok(v_str) = v.to_str() {
                    headers.insert(k.as_str().to_ascii_lowercase(), v_str.to_string());
                }
            }
            let body = res.text().await?;
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}

/// Authorization Server metadata (RFC 8414).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthServerMeta {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

/// Compute the PKCE code challenge from a code verifier (S256).
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Generate a cryptographically random PKCE verifier (43 chars) and S256 challenge pair.
pub fn pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = pkce_challenge(&verifier);
    (verifier, challenge)
}

pub trait PkceChallenge {
    fn as_challenge(&self) -> &str;
}

impl<T: PkceChallenge + ?Sized> PkceChallenge for &T {
    fn as_challenge(&self) -> &str {
        (*self).as_challenge()
    }
}

impl PkceChallenge for str {
    fn as_challenge(&self) -> &str {
        self
    }
}

impl PkceChallenge for String {
    fn as_challenge(&self) -> &str {
        self
    }
}

impl PkceChallenge for (&str, &str) {
    fn as_challenge(&self) -> &str {
        self.1
    }
}

impl PkceChallenge for (String, String) {
    fn as_challenge(&self) -> &str {
        &self.1
    }
}

/// Build the authorization URL for initiating OAuth 2.1 code grant with PKCE S256.
pub fn authorize_url(
    meta: &AuthServerMeta,
    client_id: &str,
    redirect_uri: &str,
    pkce: impl PkceChallenge,
    state: &str,
) -> String {
    let challenge = pkce.as_challenge();
    if let Ok(mut url) = reqwest::Url::parse(&meta.authorization_endpoint) {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", client_id);
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("code_challenge", challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", state);
        if let Some(scopes) = &meta.scopes_supported {
            if !scopes.is_empty() {
                q.append_pair("scope", &scopes.join(" "));
            }
        }
        drop(q);
        url.to_string()
    } else {
        let sep = if meta.authorization_endpoint.contains('?') { '&' } else { '?' };
        let mut out = format!(
            "{}{}response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
            meta.authorization_endpoint, sep, client_id, redirect_uri, challenge, state
        );
        if let Some(scopes) = &meta.scopes_supported {
            if !scopes.is_empty() {
                out.push_str(&format!("&scope={}", scopes.join("+")));
            }
        }
        out
    }
}

/// Discover authorization server metadata for a given remote MCP server URL.
///
/// Steps:
/// 1. RFC 9728 protected-resource metadata (`/.well-known/oauth-protected-resource<path>`
///    and `/.well-known/oauth-protected-resource`).
/// 2. If the protected-resource doc returns 404 / is absent, falls back to deriving
///    the authorization server from `<origin>/.well-known/oauth-authorization-server`.
/// 3. RFC 8414 AS metadata retrieval and verification of S256 PKCE support.
pub async fn discover(server_url: &str, http: &dyn HttpClient) -> Result<AuthServerMeta> {
    let url = reqwest::Url::parse(server_url).with_context(|| format!("invalid server url: {server_url}"))?;
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');

    // 1. RFC 9728 protected-resource metadata probe
    let mut as_issuer = None;
    let mut resource_candidates = Vec::new();
    if !path.is_empty() && path != "/" {
        resource_candidates.push(format!("{origin}/.well-known/oauth-protected-resource{path}"));
    }
    resource_candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));

    for res_url in resource_candidates {
        if let Ok(resp) = http.get(&res_url).await {
            if resp.status == 200 {
                if let Ok(v) = resp.json::<Value>() {
                    if let Some(servers) = v.get("authorization_servers").and_then(Value::as_array) {
                        if let Some(first) = servers.first().and_then(Value::as_str) {
                            as_issuer = Some(first.to_string());
                            break;
                        }
                    } else if let Some(first) = v.get("authorization_server").and_then(Value::as_str) {
                        as_issuer = Some(first.to_string());
                        break;
                    }
                }
            }
        }
    }

    // Fallback: when protected-resource metadata 404s, derive AS issuer directly from origin
    let issuer = as_issuer.unwrap_or_else(|| origin.clone());

    // 2. RFC 8414 Authorization Server metadata
    let as_url = reqwest::Url::parse(&issuer).with_context(|| format!("invalid issuer URL: {issuer}"))?;
    let as_origin = as_url.origin().ascii_serialization();
    let as_path = as_url.path().trim_end_matches('/');

    let mut as_candidates = Vec::new();
    if !as_path.is_empty() && as_path != "/" {
        as_candidates.push(format!("{as_origin}/.well-known/oauth-authorization-server{as_path}"));
    }
    as_candidates.push(format!("{as_origin}/.well-known/oauth-authorization-server"));
    if !as_path.is_empty() && as_path != "/" {
        as_candidates.push(format!("{as_origin}/.well-known/openid-configuration{as_path}"));
    }
    as_candidates.push(format!("{as_origin}/.well-known/openid-configuration"));

    for candidate in as_candidates {
        if let Ok(resp) = http.get(&candidate).await {
            if resp.status == 200 {
                if let Ok(meta) = resp.json::<AuthServerMeta>() {
                    if meta.authorization_endpoint.is_empty() || meta.token_endpoint.is_empty() {
                        continue;
                    }
                    if let Some(methods) = &meta.code_challenge_methods_supported {
                        if !methods.iter().any(|m| m == "S256") {
                            bail!("authorization server does not support S256 code challenge method");
                        }
                    }
                    return Ok(meta);
                }
            }
        }
    }

    bail!("failed to discover OAuth authorization server metadata for {server_url}")
}

/// Register a client dynamically with the authorization server via RFC 7591.
pub async fn register_client(
    meta: &AuthServerMeta,
    redirect_uri: &str,
    http: &dyn HttpClient,
) -> Result<String> {
    let reg_url = meta
        .registration_endpoint
        .as_deref()
        .ok_or_else(|| anyhow!("AS lacks dynamic registration; pass --client-id"))?;

    let body = json!({
        "client_name": "Kurisu",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none"
    });

    let resp = http
        .post_json(reg_url, &body)
        .await
        .with_context(|| format!("DCR request to {reg_url} failed"))?;

    if resp.status != 200 && resp.status != 201 {
        bail!(
            "dynamic client registration failed (HTTP {}): {}",
            resp.status,
            resp.body
        );
    }

    let val: Value = resp.json()?;
    let client_id = val
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("DCR response missing client_id: {}", resp.body))?;

    Ok(client_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockHttpClient {
        responses: Mutex<HashMap<String, HttpResponse>>,
        posts: Mutex<Vec<(String, Value)>>,
    }

    impl MockHttpClient {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                posts: Mutex::new(Vec::new()),
            }
        }

        fn set_get(&self, url: impl Into<String>, resp: HttpResponse) {
            self.responses.lock().unwrap().insert(url.into(), resp);
        }
    }

    impl HttpClient for MockHttpClient {
        fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                let guard = self.responses.lock().unwrap();
                match guard.get(url) {
                    Some(resp) => Ok(resp.clone()),
                    None => Ok(HttpResponse::new(404, "not found")),
                }
            })
        }

        fn post_json<'a>(&'a self, url: &'a str, body: &'a Value) -> BoxFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                self.posts.lock().unwrap().push((url.to_string(), body.clone()));
                let guard = self.responses.lock().unwrap();
                match guard.get(url) {
                    Some(resp) => Ok(resp.clone()),
                    None => Ok(HttpResponse::new(201, json!({"client_id": "mock-client-123"}).to_string())),
                }
            })
        }
    }

    #[test]
    fn pkce_s256_rfc7636_appendix_b_vector() {
        // RFC 7636 Appendix B test vector
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_challenge(verifier), expected_challenge);
    }

    #[test]
    fn pkce_verifier_charset_and_length() {
        for _ in 0..10 {
            let (verifier, challenge) = pkce_pair();
            assert!(verifier.len() >= 43 && verifier.len() <= 128);
            assert!(verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' || c == '~'));
            assert_eq!(challenge, pkce_challenge(&verifier));
        }
    }

    #[test]
    fn authorize_url_format_and_params() {
        let meta = AuthServerMeta {
            issuer: "https://auth.example.com".into(),
            authorization_endpoint: "https://auth.example.com/oauth/authorize".into(),
            token_endpoint: "https://auth.example.com/oauth/token".into(),
            registration_endpoint: Some("https://auth.example.com/oauth/register".into()),
            scopes_supported: Some(vec!["mcp:read".into(), "mcp:write".into()]),
            code_challenge_methods_supported: Some(vec!["S256".into()]),
        };

        let pkce = pkce_pair();
        let url = authorize_url(
            &meta,
            "test-client-id",
            "http://127.0.0.1:8798/oauth/callback",
            &pkce,
            "state-xyz-123",
        );

        assert_eq!(url.matches("code_challenge=").count(), 1);
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=test-client-id"));
        assert!(url.contains("state=state-xyz-123"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8798%2Foauth%2Fcallback"));
        assert!(url.contains("scope=mcp%3Aread+mcp%3Awrite") || url.contains("scope=mcp:read+mcp:write") || url.contains("scope=mcp:read%20mcp:write"));
    }

    #[tokio::test]
    async fn discovery_rfc9728_and_fallback_on_404() {
        let mock = MockHttpClient::new();

        // Server URL: https://mcp.notion.com/v1/mcp
        // 1. Resource doc 404s on both /v1/mcp path and base
        // 2. Fallback derives origin: https://mcp.notion.com
        // 3. AS doc succeeds on https://mcp.notion.com/.well-known/oauth-authorization-server
        mock.set_get(
            "https://mcp.notion.com/.well-known/oauth-authorization-server",
            HttpResponse::new(
                200,
                json!({
                    "issuer": "https://mcp.notion.com",
                    "authorization_endpoint": "https://mcp.notion.com/oauth/authorize",
                    "token_endpoint": "https://mcp.notion.com/oauth/token",
                    "code_challenge_methods_supported": ["S256"]
                })
                .to_string(),
            ),
        );

        let meta = discover("https://mcp.notion.com/v1/mcp", &mock).await.unwrap();
        assert_eq!(meta.issuer, "https://mcp.notion.com");
        assert_eq!(meta.authorization_endpoint, "https://mcp.notion.com/oauth/authorize");
        assert_eq!(meta.token_endpoint, "https://mcp.notion.com/oauth/token");

        // Now test when RFC 9728 protected resource doc IS present
        let mock2 = MockHttpClient::new();
        mock2.set_get(
            "https://mcp.example.com/.well-known/oauth-protected-resource/api",
            HttpResponse::new(
                200,
                json!({
                    "resource": "https://mcp.example.com/api",
                    "authorization_servers": ["https://auth.example.com"]
                })
                .to_string(),
            ),
        );
        mock2.set_get(
            "https://auth.example.com/.well-known/oauth-authorization-server",
            HttpResponse::new(
                200,
                json!({
                    "issuer": "https://auth.example.com",
                    "authorization_endpoint": "https://auth.example.com/oauth/authorize",
                    "token_endpoint": "https://auth.example.com/oauth/token",
                    "registration_endpoint": "https://auth.example.com/oauth/register",
                    "code_challenge_methods_supported": ["S256"]
                })
                .to_string(),
            ),
        );

        let meta2 = discover("https://mcp.example.com/api", &mock2).await.unwrap();
        assert_eq!(meta2.issuer, "https://auth.example.com");
        assert_eq!(meta2.registration_endpoint, Some("https://auth.example.com/oauth/register".into()));
    }

    #[tokio::test]
    async fn dynamic_client_registration() {
        let mock = MockHttpClient::new();
        let meta = AuthServerMeta {
            issuer: "https://auth.example.com".into(),
            authorization_endpoint: "https://auth.example.com/oauth/authorize".into(),
            token_endpoint: "https://auth.example.com/oauth/token".into(),
            registration_endpoint: Some("https://auth.example.com/oauth/register".into()),
            scopes_supported: None,
            code_challenge_methods_supported: Some(vec!["S256".into()]),
        };

        mock.set_get(
            "https://auth.example.com/oauth/register",
            HttpResponse::new(201, json!({"client_id": "registered-client-999"}).to_string()),
        );

        let client_id = register_client(&meta, "http://127.0.0.1:8798/oauth/callback", &mock)
            .await
            .unwrap();
        assert_eq!(client_id, "registered-client-999");

        // Verify request payload sent to registration endpoint
        let posts = mock.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, "https://auth.example.com/oauth/register");
        assert_eq!(posts[0].1["token_endpoint_auth_method"], "none");
        assert_eq!(posts[0].1["response_types"], json!(["code"]));
        assert_eq!(posts[0].1["grant_types"], json!(["authorization_code", "refresh_token"]));

        // When registration_endpoint is None
        let meta_no_reg = AuthServerMeta {
            registration_endpoint: None,
            ..meta
        };
        assert!(register_client(&meta_no_reg, "http://127.0.0.1:8798/oauth/callback", &mock)
            .await
            .is_err());
    }
}
