//! Nexus API HTTP client.
//!
//! Wraps `reqwest::Client` with Nexus-specific configuration:
//! - HTTPS enforcement
//! - Bearer token authentication
//! - Typed error mapping from HTTP status codes

use reqwest::StatusCode;
use tracing::debug;

use crate::api::types::{ApiError, AuthStatusResponse};
use crate::Error;

/// HTTP client for the Nexus API.
#[derive(Debug, Clone)]
pub struct NexusClient {
    client: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl NexusClient {
    /// Create a new Nexus API client.
    ///
    /// Enforces HTTPS for the base URL unless it targets localhost.
    pub fn new(base_url: &str, token: Option<String>) -> Result<Self, Error> {
        // Allow http for localhost/127.0.0.1 during development
        let is_local = base_url.contains("localhost") || base_url.contains("127.0.0.1");
        if !is_local && !base_url.starts_with("https://") {
            return Err(Error::Config(format!(
                "API URL must use HTTPS: {}",
                base_url
            )));
        }

        let client = reqwest::Client::builder()
            .user_agent(format!(
                "{}/{}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ))
            .build()?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Set or replace the authentication token.
    pub fn set_token(&mut self, token: String) {
        self.token = Some(token);
    }

    /// Check authentication status by calling the auth endpoint.
    pub async fn auth_status(&self) -> Result<AuthStatusResponse, Error> {
        self.get("/api/auth/me").await
    }

    /// Send a GET request and deserialize the JSON response.
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let url = format!("{}{}", self.base_url, path);
        debug!("GET {}", url);

        let mut req = self.client.get(&url);
        if let Some(ref token) = self.token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await?;
        self.handle_response(resp).await
    }

    /// Send a POST request with a JSON body and deserialize the response.
    #[allow(dead_code)]
    async fn post<T, B>(&self, path: &str, body: &B) -> Result<T, Error>
    where
        T: serde::de::DeserializeOwned,
        B: serde::Serialize,
    {
        let url = format!("{}{}", self.base_url, path);
        debug!("POST {}", url);

        let mut req = self.client.post(&url).json(body);
        if let Some(ref token) = self.token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await?;
        self.handle_response(resp).await
    }

    /// Map HTTP response status to typed errors or deserialize the body.
    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, Error> {
        let status = resp.status();

        if status.is_success() {
            return Ok(resp.json().await?);
        }

        // Try to extract a structured error message from the body
        let error_msg = match resp.json::<ApiError>().await {
            Ok(api_err) => api_err.to_string(),
            Err(_) => format!("HTTP {}", status),
        };

        match status {
            StatusCode::UNAUTHORIZED => Err(Error::Unauthorized(error_msg)),
            StatusCode::FORBIDDEN => Err(Error::Forbidden(error_msg)),
            StatusCode::NOT_FOUND => Err(Error::NotFound(error_msg)),
            _ => Err(Error::Api(error_msg)),
        }
    }
}
