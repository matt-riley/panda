//! Query the selected provider's catalogue with credentials scoped to its own origin.
use crate::{auth, checked, wire};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{path::Path, time::Duration};
pub struct Catalogue {
    pub models: Vec<(String, Option<u64>)>,
    pub efforts: std::collections::BTreeMap<String, Vec<String>>,
    pub prices: std::collections::BTreeMap<String, panda_core::TokenPrices>,
    pub output_limits: std::collections::BTreeMap<String, u64>,
}

pub fn list(provider: &str, state: &Path, key: Option<&str>) -> Result<Vec<(String, Option<u64>)>> {
    Ok(catalogue(provider, state, key)?.models)
}

pub fn catalogue(provider: &str, state: &Path, key: Option<&str>) -> Result<Catalogue> {
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
                "openai-api" => client
                    .get("https://api.openai.com/v1/models")
                    .bearer_auth(key.context("OpenAI API key required")?),
                "anthropic" => client
                    .get("https://api.anthropic.com/v1/models")
                    .header("anthropic-version", "2023-06-01")
                    .header("x-api-key", key.context("Anthropic key required")?),
                "google" => client
                    .get("https://generativelanguage.googleapis.com/v1beta/models")
                    .header("x-goog-api-key", key.context("Gemini key required")?),
                "openrouter" => client.get("https://openrouter.ai/api/v1/models"),
                "openai-codex" => {
                    let cred = auth::credential(state, auth::CODEX, &client).await?;
                    client
                        .get("https://chatgpt.com/backend-api/codex/models")
                        // Codex filters this catalogue by its client protocol version.
                        .query(&[("client_version", "0.169.0")])
                        .bearer_auth(wire::string(&cred, "access")?)
                        .header("chatgpt-account-id", wire::string(&cred, "accountId")?)
                }
                _ => bail!("unknown provider"),
            };
            let v: Value = checked(request.send().await?).await?.json().await?;
            let mut efforts = std::collections::BTreeMap::new();
            let mut prices = std::collections::BTreeMap::new();
            let mut output_limits = std::collections::BTreeMap::new();
            let mut ids: Vec<_> = v["data"]
                .as_array()
                .or(v["models"].as_array())
                .context("missing model catalogue")?
                .iter()
                .filter(|m| m["visibility"] != "hide")
                .filter_map(|m| {
                    let id = if provider == "google" {
                        if !m["supportedGenerationMethods"]
                            .as_array()?
                            .iter()
                            .any(|v| v == "generateContent")
                        {
                            return None;
                        }
                        m["name"].as_str()?.strip_prefix("models/")?
                    } else {
                        m["id"].as_str().or(m["slug"].as_str())?
                    };
                    if let Some(price) = catalogue_price(&m["pricing"]) {
                        prices.insert(id.to_owned(), price);
                    }
                    if let Some(limit) = m["top_provider"]["max_completion_tokens"]
                        .as_u64()
                        .filter(|&n| n > 0)
                    {
                        output_limits.insert(id.to_owned(), limit);
                    }
                    if let Some(levels) = m["supported_reasoning_levels"].as_array() {
                        efforts.insert(
                            id.to_owned(),
                            levels
                                .iter()
                                .filter_map(|l| l["effort"].as_str().map(str::to_owned))
                                .collect(),
                        );
                    }
                    Some((
                        id.to_owned(),
                        m["context_window"]
                            .as_u64()
                            .or(m["context_length"].as_u64())
                            .or(m["inputTokenLimit"].as_u64()),
                    ))
                })
                .collect();
            ids.sort();
            ids.dedup();
            Ok(Catalogue {
                models: ids,
                efforts,
                output_limits,
                prices,
            })
        })
}

pub fn codex_configured(state: &Path) -> Result<bool> {
    let path = state.join("subscriptions.sqlite3");
    if !path.try_exists()? {
        return Ok(false);
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM credentials WHERE id=?)",
        [auth::CODEX],
        |r| r.get(0),
    )?)
}

pub fn logout(state: &Path) -> Result<()> {
    let path = state.join("subscriptions.sqlite3");
    if path.try_exists()? {
        rusqlite::Connection::open(path)?
            .execute("DELETE FROM credentials WHERE id=?", [auth::CODEX])?;
    }
    Ok(())
}

fn catalogue_price(pricing: &Value) -> Option<panda_core::TokenPrices> {
    let rate = |key: &str| {
        let value = &pricing[key];
        let dollars = value
            .as_f64()
            .or_else(|| value.as_str()?.parse::<f64>().ok())?;
        let million = dollars * 1_000_000.;
        (million.is_finite() && million >= 0.).then_some(million)
    };
    Some(panda_core::TokenPrices {
        input: rate("prompt")?,
        output: rate("completion")?,
        cache_read: rate("input_cache_read"),
        cache_write: rate("input_cache_write"),
    })
}

#[test]
fn catalogue_prices_convert_per_token_units() {
    let prices = catalogue_price(&serde_json::json!({"prompt":"0.000002","completion":0.000006,"input_cache_read":"0.000001"})).unwrap();
    assert_eq!(
        (
            prices.input,
            prices.output,
            prices.cache_read,
            prices.cache_write
        ),
        (2., 6., Some(1.), None)
    );
    assert!(catalogue_price(&serde_json::json!({"prompt":"-1","completion":0})).is_none());
}
