//! Tier 4 — mem0 vector recall client (REST against the local Hermes mem0 stack).
//!
//! mem0 is reused from Hermes' Docker compose (`~/.hermes/mem0-docker/`):
//! Postgres+pgvector + Neo4j + mem0-api at `localhost:8000`.
//!
//! Status: stub. Active wiring lands when we have months of diary entries to
//! justify vector recall. Until then, this module exposes the contract so we
//! don't reshape callers when we flip it on.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone)]
pub struct Mem0Client {
    base_url: String,
    user_id: String,
    http: reqwest::Client,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub memory: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub memory: String,
    pub score: f64,
}

impl Mem0Client {
    pub fn new(base_url: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            user_id: user_id.into(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Default mem0 endpoint matching Hermes' Docker compose. Reads MEM0_USER_ID
    /// from env (falls back to "user" if unset).
    pub fn local() -> Self {
        let user_id = std::env::var("MEM0_USER_ID").unwrap_or_else(|_| "user".into());
        Self::new("http://localhost:8000", user_id)
    }

    /// Health probe — `/health` returns 200 when mem0-api is up.
    pub async fn ping(&self) -> Result<bool> {
        let url = format!("{}/health", self.base_url);
        let r = self.http.get(&url).send().await.context("mem0 /health")?;
        Ok(r.status().is_success())
    }

    /// Add a memory. Returns the new memory id.
    pub async fn add(&self, content: &str, metadata: serde_json::Value) -> Result<String> {
        let url = format!("{}/memories", self.base_url);
        let body = json!({
            "messages": [{ "role": "user", "content": content }],
            "user_id": self.user_id,
            "metadata": metadata,
        });
        let r = self.http.post(&url).json(&body).send().await.context("mem0 add")?;
        let v: serde_json::Value = r.error_for_status()?.json().await?;
        // mem0's response shape varies a bit by version — try common keys.
        Ok(v.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string())
    }

    /// Vector search.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let url = format!("{}/search", self.base_url);
        let body = json!({
            "query": query,
            "user_id": self.user_id,
            "limit": limit,
        });
        let r = self.http.post(&url).json(&body).send().await.context("mem0 search")?;
        let v: serde_json::Value = r.error_for_status()?.json().await?;
        let results = v.get("results").cloned().unwrap_or(serde_json::json!([]));
        let parsed: Vec<SearchResult> = serde_json::from_value(results).unwrap_or_default();
        Ok(parsed)
    }
}
