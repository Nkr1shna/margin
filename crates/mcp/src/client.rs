//! A small HTTP client for the service's context API and device sign-in
//! (`margin_shared::routes`). It always talks to the URL it was given: there is no
//! default server, so a test can only ever reach its own fake.

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use margin_shared::context::{
    ContextChanges, ContextIngest, ContextQuery, Document, DocumentOutline, Hit,
};
use margin_shared::{
    ApiError, LoginState, PollLoginRequest, StartLogin, StartLoginRequest, routes,
};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;

/// `DELETE /api/context/documents/{id}`: removes a document an agent ingested, with its
/// passages; `purge` and `ingest-dir` use it.
pub use routes::context_document;

#[derive(Clone)]
pub struct Client {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Client {
    /// A client for `base` (e.g. `https://margin.krishnanelloore.com`), signed in with
    /// `token` when there is one (the sign-in routes need none).
    pub fn new(base: &str, token: Option<String>) -> anyhow::Result<Self> {
        let base = normalize_url(base)?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("margin-mcp/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .context("couldn't set up the HTTP client")?;
        Ok(Self { base, token, http })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let req = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }

    /// `POST routes::CONTEXT_SEARCH`.
    pub async fn search(&self, query: &ContextQuery) -> anyhow::Result<Vec<Hit>> {
        let req = self
            .request(Method::POST, routes::CONTEXT_SEARCH)
            .json(query);
        json(req.send().await).await
    }

    /// `GET routes::context_document_outline(id)`.
    pub async fn outline(&self, document_id: &str) -> anyhow::Result<DocumentOutline> {
        let req = self.request(Method::GET, &routes::context_document_outline(document_id));
        json(req.send().await).await
    }

    /// `GET routes::context_document_passages(id)?from&to` (inclusive `ord`s).
    pub async fn passages(
        &self,
        document_id: &str,
        from: u32,
        to: u32,
    ) -> anyhow::Result<Vec<Hit>> {
        let req = self
            .request(Method::GET, &routes::context_document_passages(document_id))
            .query(&[("from", from), ("to", to)]);
        json(req.send().await).await
    }

    /// `POST routes::CONTEXT_INGEST`.
    pub async fn ingest(&self, doc: &ContextIngest) -> anyhow::Result<Document> {
        let req = self.request(Method::POST, routes::CONTEXT_INGEST).json(doc);
        json(req.send().await).await
    }

    /// `GET routes::CONTEXT_CHANGES?since`.
    pub async fn changes(&self, since: u64) -> anyhow::Result<ContextChanges> {
        let req = self
            .request(Method::GET, routes::CONTEXT_CHANGES)
            .query(&[("since", since)]);
        json(req.send().await).await
    }

    /// Every document of the project (the change feed from the beginning).
    pub async fn documents(&self) -> anyhow::Result<Vec<Document>> {
        Ok(self.changes(0).await?.changed)
    }

    /// `DELETE context_document(id)`. A document that is already gone is not an error.
    pub async fn delete_document(&self, document_id: &str) -> anyhow::Result<()> {
        let resp = self
            .request(Method::DELETE, &context_document(document_id))
            .send()
            .await;
        match checked(resp).await {
            Ok(_) => Ok(()),
            Err(e) if e.status == Some(StatusCode::NOT_FOUND) && e.code == "not_found" => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// `POST routes::AUTH_START`.
    pub async fn start_login(&self, device_name: &str) -> anyhow::Result<StartLogin> {
        let req = self
            .request(Method::POST, routes::AUTH_START)
            .json(&StartLoginRequest {
                device_name: device_name.into(),
            });
        json(req.send().await).await
    }

    /// `POST routes::AUTH_POLL`.
    pub async fn poll_login(&self, login: &StartLogin) -> anyhow::Result<LoginState> {
        let req = self
            .request(Method::POST, routes::AUTH_POLL)
            .json(&PollLoginRequest {
                login_id: login.login_id.clone(),
                poll_secret: login.poll_secret.clone(),
            });
        json(req.send().await).await
    }
}

/// `https://host/` → `https://host`; only http(s) URLs.
pub fn normalize_url(url: &str) -> anyhow::Result<String> {
    let url = url.trim().trim_end_matches('/');
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() <= "https://".len()
    {
        bail!("{url:?} isn't a Margin service URL (it must start with https:// or http://)");
    }
    Ok(url.to_string())
}

/// A failed call: the service's `ApiError` when it sent one.
#[derive(Debug)]
pub struct CallError {
    pub status: Option<StatusCode>,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(
                f,
                "Margin answered {status} ({}): {}",
                self.code, self.message
            ),
            None => write!(f, "couldn't reach Margin: {}", self.message),
        }
    }
}

impl std::error::Error for CallError {}

async fn checked(resp: reqwest::Result<Response>) -> Result<Response, CallError> {
    let resp = resp.map_err(|e| CallError {
        status: None,
        code: "unreachable".into(),
        message: e.to_string(),
    })?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let (code, message) = match serde_json::from_str::<ApiError>(&body) {
        Ok(e) => (e.code, e.message),
        Err(_) => (
            status.as_str().to_string(),
            if body.trim().is_empty() {
                status.canonical_reason().unwrap_or("error").to_string()
            } else {
                body.chars().take(300).collect()
            },
        ),
    };
    let message = if status == StatusCode::UNAUTHORIZED {
        format!("{message} (sign in again with `margin-mcp login`)")
    } else {
        message
    };
    Err(CallError {
        status: Some(status),
        code,
        message,
    })
}

async fn json<T: DeserializeOwned>(resp: reqwest::Result<Response>) -> anyhow::Result<T> {
    let resp = checked(resp).await?;
    let bytes = resp.bytes().await.context("Margin's answer was cut off")?;
    serde_json::from_slice(&bytes).map_err(|e| anyhow!("Margin's answer didn't parse: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_normalized() {
        assert_eq!(
            normalize_url(" https://margin.example.com/ ").unwrap(),
            "https://margin.example.com"
        );
        assert_eq!(
            normalize_url("http://127.0.0.1:9").unwrap(),
            "http://127.0.0.1:9"
        );
        assert!(normalize_url("margin.example.com").is_err());
        assert!(normalize_url("https://").is_err());
        assert_eq!(context_document("d1"), "/api/context/documents/d1");
    }
}
