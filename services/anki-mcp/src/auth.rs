// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use anyhow::Result;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use jsonwebtoken::decode;
use jsonwebtoken::decode_header;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::Algorithm;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::Validation;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;

#[derive(Clone, Deserialize)]
pub struct Principal {
    pub sub: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub scp: Vec<String>,
    #[serde(default)]
    nonce: String,
    auth_time: Option<serde_json::Value>,
}

impl Principal {
    // TODO: Update the ChatGPT plugin OAuth configuration to request write_notes
    // and maintenance, reauthorize it, then remove the legacy sync permission
    // fallback.
    pub fn has_permission(&self, scope: &str) -> bool {
        self.has_scope(scope)
            || (matches!(scope, "anki:write_notes" | "anki:maintenance")
                && self.has_scope("anki:sync"))
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.scope
            .split_whitespace()
            .chain(self.scp.iter().map(String::as_str))
            .any(|s| s == scope)
    }
}

pub struct Auth {
    keys: RwLock<JwkSet>,
    validation: Validation,
    subject: String,
    requests: Semaphore,
    pub issuer: String,
    pub resource: String,
    metadata_url: String,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

impl Auth {
    pub async fn new(issuer: String, resource: String, subject: String) -> Result<Arc<Self>> {
        if subject.is_empty() {
            bail!("owner subject is required");
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let base = reqwest::Url::parse(&issuer)?;
        if base.scheme() != "https" {
            bail!("OIDC issuer must use HTTPS");
        }
        let discovery: Discovery = client
            .get(format!("{issuer}/.well-known/openid-configuration"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let key_url = reqwest::Url::parse(&discovery.jwks_uri)?;
        if discovery.issuer != issuer || key_url.origin() != base.origin() {
            bail!("OIDC discovery issuer/key origin mismatch");
        }
        let keys = client
            .get(key_url.clone())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let validation = validation(&issuer, &resource);
        let metadata_url = protected_metadata_url(&resource)?;
        let auth = Arc::new(Self {
            keys: RwLock::new(keys),
            validation,
            subject,
            requests: Semaphore::new(8),
            issuer,
            resource,
            metadata_url,
        });
        let refreshed = auth.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            interval.tick().await;
            loop {
                interval.tick().await;
                if let Ok(response) = client.get(key_url.clone()).send().await {
                    if let Ok(response) = response.error_for_status() {
                        if let Ok(keys) = response.json().await {
                            *refreshed.keys.write().await = keys;
                        }
                    }
                }
            }
        });
        Ok(auth)
    }

    async fn verify(&self, token: &str) -> Option<Principal> {
        if token.len() > 16384 {
            return None;
        }
        let header = decode_header(token).ok()?;
        if header.alg != Algorithm::RS256
            || header.typ.as_deref().is_some_and(|typ| {
                typ.eq_ignore_ascii_case("id_token") || typ.eq_ignore_ascii_case("id-token")
            })
        {
            return None;
        }
        let keys = self.keys.read().await;
        let jwk = keys.find(header.kid.as_deref()?).cloned()?;
        let key = DecodingKey::from_jwk(&jwk).ok()?;
        let principal = decode::<Principal>(token, &key, &self.validation)
            .ok()?
            .claims;
        if principal.sub != self.subject
            || !principal.nonce.is_empty()
            || principal.auth_time.is_some()
        {
            return None;
        }
        Some(principal)
    }
}

fn protected_metadata_url(resource: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(resource)?;
    if url.scheme() != "https" || url.query().is_some() || url.fragment().is_some() {
        bail!("MCP resource must be an HTTPS URL without query or fragment");
    }
    url.set_path(&format!(
        "/.well-known/oauth-protected-resource{}",
        url.path()
    ));
    Ok(url.into())
}

fn validation(issuer: &str, resource: &str) -> Validation {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[resource]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.validate_nbf = true;
    validation.leeway = 0;
    validation
}

pub async fn authenticate(
    State(auth): State<Arc<Auth>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Ok(_permit) = auth.requests.try_acquire() else {
        return (StatusCode::TOO_MANY_REQUESTS, "server busy").into_response();
    };
    let principal = match request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        Some(token) => auth.verify(token).await,
        None => None,
    };
    match principal {
        Some(principal) if principal.has_scope("anki:read") => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Some(_) => (StatusCode::FORBIDDEN, "insufficient_scope: anki:read").into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                format!("Bearer resource_metadata=\"{}\"", auth.metadata_url),
            )],
            "invalid_token",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::encode;
    use jsonwebtoken::EncodingKey;
    use jsonwebtoken::Header;
    use serde_json::json;

    use super::*;

    fn auth() -> Auth {
        Auth {
            keys: RwLock::new(
                serde_json::from_str(include_str!("../tests/fixtures/jwks.json")).unwrap(),
            ),
            validation: validation("https://issuer.example", "https://mcp.example/mcp"),
            subject: "owner".into(),
            requests: Semaphore::new(8),
            issuer: "https://issuer.example".into(),
            resource: "https://mcp.example/mcp".into(),
            metadata_url: protected_metadata_url("https://mcp.example/mcp").unwrap(),
        }
    }

    #[test]
    fn discovery_challenge_preserves_mcp_hostname() {
        assert_eq!(
            protected_metadata_url("https://mcp.jupiter.timdumol.com/mcp").unwrap(),
            "https://mcp.jupiter.timdumol.com/.well-known/oauth-protected-resource/mcp"
        );
    }

    fn token(claims: serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("fixture".into());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_der(include_bytes!("../tests/fixtures/signing-key.der")),
        )
        .unwrap()
    }

    fn claims() -> serde_json::Value {
        json!({"iss":"https://issuer.example", "aud":"https://mcp.example/mcp", "sub":"owner", "exp":4102444800u64, "scope":"anki:read"})
    }

    #[tokio::test]
    async fn legacy_sync_grants_writes_and_maintenance_but_read_only_does_not() {
        let auth = auth();
        for (scopes, allowed) in [("anki:read", false), ("anki:read anki:sync", true)] {
            let mut claims = claims();
            claims["scope"] = json!(scopes);
            let principal = auth.verify(&token(claims)).await.unwrap();
            for permission in ["anki:write_notes", "anki:maintenance"] {
                assert_eq!(
                    principal.has_permission(permission),
                    allowed,
                    "{scopes}: {permission}"
                );
            }
            assert!(!principal.has_permission("anki:unknown"));
        }
    }

    #[tokio::test]
    async fn oauth_rejects_wrong_issuer_audience_expiry_and_owner() {
        let auth = auth();
        assert!(auth
            .verify(&token(claims()))
            .await
            .unwrap()
            .has_scope("anki:read"));
        for (key, value) in [
            ("iss", json!("https://wrong.example")),
            ("aud", json!("client-id")),
            ("exp", json!(1)),
            ("sub", json!("other-user")),
            ("nonce", json!("id-token")),
            ("auth_time", json!(1)),
        ] {
            let mut modified = claims();
            modified[key] = value;
            assert!(
                auth.verify(&token(modified)).await.is_none(),
                "accepted invalid {key}"
            );
        }
        let mut missing_exp = claims();
        missing_exp.as_object_mut().unwrap().remove("exp");
        assert!(auth.verify(&token(missing_exp)).await.is_none());
    }

    #[tokio::test]
    async fn oauth_rejects_bad_signature_and_does_not_grant_unissued_scopes() {
        let auth = auth();
        let token = token(claims());
        let principal = auth.verify(&token).await.unwrap();
        assert!(!principal.has_scope("anki:sync"));
        let mut corrupted = token.into_bytes();
        let index = corrupted.len() - 20;
        corrupted[index] = if corrupted[index] == b'A' { b'B' } else { b'A' };
        assert!(auth
            .verify(std::str::from_utf8(&corrupted).unwrap())
            .await
            .is_none());
    }
}
