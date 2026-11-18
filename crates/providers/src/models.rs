//! Query the selected provider's catalogue with credentials scoped to its own origin.
use crate::{auth, checked, wire};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{path::Path, time::Duration};
pub fn list(provider: &str, state: &Path, key: Option<&str>) -> Result<Vec<(String, Option<u64>)>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            let request = match provider {
                "deepseek" => client
                    .get("https://api.deepseek.com/models")
                    .bearer_auth(key.context("DeepSeek key required")?),
                "openrouter" => client.get("https://openrouter.ai/api/v1/models"),
                "openai-codex" => {
                    let cred = auth::credential(state, auth::CODEX, &client).await?;
                    client
                        .get("https://chatgpt.com/backend-api/codex/models")
                        .query(&[("client_version", env!("CARGO_PKG_VERSION"))])
                        .bearer_auth(wire::string(&cred, "access")?)
                        .header("chatgpt-account-id", wire::string(&cred, "accountId")?)
                }
                _ => bail!("unknown provider"),
            };
            let v: Value = checked(request.send().await?).await?.json().await?;
            let mut ids: Vec<_> = v["data"]
                .as_array()
                .or(v["models"].as_array())
                .context("missing model catalogue")?
                .iter()
                .filter(|m| m["visibility"] != "hide")
                .filter_map(|m| {
                    Some((
                        m["id"].as_str().or(m["slug"].as_str())?.to_owned(),
                        m["context_window"]
                            .as_u64()
                            .or(m["context_length"].as_u64()),
                    ))
                })
                .collect();
            ids.sort();
            ids.dedup();
            Ok(ids)
        })
}
