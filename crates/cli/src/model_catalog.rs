//! User-owned OpenAI-compatible providers from the state directory's models.json.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    #[serde(default)]
    pub providers: BTreeMap<String, Endpoint>,
    #[serde(default, rename = "virtualModels")]
    pub routes: BTreeMap<String, crate::routing::Route>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Endpoint {
    #[serde(default)]
    pub transport: panda_providers::native::Transport,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api: Option<String>,
    pub models: Vec<Model>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub name: Option<String>,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(default = "text_only")]
    pub input: Vec<String>,
    pub cost: Option<panda_core::TokenPrices>,
}

fn text_only() -> Vec<String> {
    vec!["text".into()]
}
pub fn valid_provider(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id.as_bytes()[0].is_ascii_alphabetic()
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

impl Catalog {
    pub fn read(state: &Path) -> Result<Self> {
        let path = state.join("models.json");
        if !path.try_exists()? {
            return Ok(Self::default());
        }
        let result: Self =
            serde_json::from_slice(&fs::read(&path)?).context("invalid models.json")?;
        for (id, ep) in &result.providers {
            ensure!(
                valid_provider(id) && !matches!(id.as_str(), "openai" | "codex" | "router"),
                "invalid models.json provider id {id}"
            );
            let url = panda_providers::endpoint_url(&ep.base_url)
                .with_context(|| format!("invalid baseUrl for {id} in models.json"))?;
            ensure!(url, "invalid baseUrl for {id} in models.json");
            ensure!(
                ep.api
                    .as_deref()
                    .is_none_or(|api| api == "openai-completions"
                        || panda_providers::native::Family::parse(api).is_ok()),
                "unsupported API for {id} in models.json"
            );
            ensure!(
                ep.transport != panda_providers::native::Transport::Websocket
                    || matches!(
                        ep.api.as_deref(),
                        Some("openai-responses" | "azure-openai-responses")
                    ),
                "WebSocket transport requires a Responses API for {id}"
            );
            if let Some(key) = &ep.api_key {
                ensure!(
                    !key.trim().is_empty() && !key.starts_with('!'),
                    "invalid apiKey for {id} in models.json"
                );
            }
            ensure!(!ep.models.is_empty(), "no models for {id} in models.json");
            let mut seen = BTreeSet::new();
            for model in &ep.models {
                ensure!(
                    !model.id.is_empty()
                        && !model.id.chars().any(char::is_whitespace)
                        && seen.insert(&model.id),
                    "empty, duplicate or invalid model ID for {id}"
                );
                ensure!(
                    model.context_window >= model.max_tokens && model.max_tokens > 0,
                    "invalid contextWindow/maxTokens for {id}/{}",
                    model.id
                );
                ensure!(
                    model.input.iter().any(|s| s == "text")
                        && model.input.iter().all(|s| s == "text" || s == "image"),
                    "invalid input capabilities for {id}/{}",
                    model.id
                );
                ensure!(
                    model.cost.as_ref().is_none_or(|p| p.valid()),
                    "invalid cost for {id}/{}",
                    model.id
                );
            }
        }
        for (name, route) in &result.routes {
            ensure!(
                !name.is_empty() && !name.chars().any(char::is_whitespace),
                "invalid virtual model ID"
            );
            for target in route.targets() {
                ensure!(
                    target.provider != "router" && !target.model.is_empty(),
                    "virtual model {name} must target a physical model"
                );
                if let Some(ep) = result.providers.get(&target.provider) {
                    ep.model(&target.model)?;
                } else {
                    ensure!(
                        crate::PROVIDERS.contains(&target.provider.as_str()),
                        "unknown backing provider {}",
                        target.provider
                    );
                }
            }
        }
        Ok(result)
    }

    pub fn ids(&self) -> Vec<&str> {
        crate::PROVIDERS
            .iter()
            .copied()
            .chain(
                self.providers
                    .keys()
                    .map(String::as_str)
                    .filter(|id| !crate::PROVIDERS.contains(id)),
            )
            .chain((!self.routes.is_empty()).then_some("router"))
            .collect()
    }
}

impl Endpoint {
    pub fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
    pub fn model(&self, id: &str) -> Result<&Model> {
        self.models
            .iter()
            .find(|m| m.id == id)
            .with_context(|| format!("model {id} missing from models.json"))
    }
    pub fn key(&self, state: &Path, id: &str) -> Result<String> {
        let Some(config) = self.api_key.as_deref() else {
            return Ok(String::new());
        };
        let key = if config
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            std::env::var(config)
                .or_else(|_| fs::read_to_string(state.join(format!("{id}.key"))))
                .with_context(|| format!("set {config} or use :login {id}"))?
        } else {
            config.to_owned()
        };
        ensure!(!key.trim().is_empty(), "empty credential for {id}");
        Ok(key.trim().into())
    }
}

pub fn logged_out(state: &Path, id: &str) -> Result<bool> {
    ensure!(valid_provider(id), "invalid provider ID");
    Ok(state.join(format!("{id}.logged-out")).try_exists()?)
}
pub fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => Ok(result?),
    }
}

type ModelRows = Vec<(String, String)>;

struct ScopePattern {
    matcher: globset::GlobMatcher,
    effort: Option<String>,
}
fn patterns(scope: &str) -> Result<Vec<ScopePattern>> {
    scope
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|part| {
            let (pattern, effort) = part
                .trim()
                .rsplit_once(':')
                .map_or((part.trim(), None), |(p, e)| (p, Some(e.to_owned())));
            ensure!(!pattern.is_empty(), "empty model pattern");
            let glob = if pattern.contains(['*', '?', '[']) {
                pattern.into()
            } else {
                format!("*{}*", globset::escape(pattern))
            };
            Ok(ScopePattern {
                matcher: globset::GlobBuilder::new(&glob)
                    .case_insensitive(true)
                    .build()?
                    .compile_matcher(),
                effort,
            })
        })
        .collect()
}
pub fn scope_effort(scope: &str, provider: &str, model: &str) -> Result<Option<String>> {
    Ok(patterns(scope)?
        .into_iter()
        .find(|p| p.matcher.is_match(model) || p.matcher.is_match(format!("{provider}/{model}")))
        .and_then(|p| p.effort))
}
pub fn available_models(state: &Path, scope: &str) -> Result<(ModelRows, Vec<String>)> {
    let catalog = Catalog::read(state)?;
    let patterns = patterns(scope)?;
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for id in catalog.ids() {
        if !crate::configured(state, id)? {
            continue;
        }
        match crate::model_entries(state, id, true) {
            Ok(models) => rows.extend(
                models
                    .into_iter()
                    .filter(|(m, _)| {
                        catalog.providers.contains_key(id) || crate::canonical_model(id, m).0 == id
                    })
                    .map(|(m, _)| (id.to_owned(), m)),
            ),
            Err(e) => errors.push(format!("{id}: {e:#}")),
        }
    }
    if patterns.is_empty() {
        return Ok((rows, errors));
    }
    let mut filtered = Vec::new();
    for pat in patterns {
        for (id, m) in &rows {
            if (pat.matcher.is_match(m) || pat.matcher.is_match(format!("{id}/{m}")))
                && !filtered.contains(&(id.clone(), m.clone()))
            {
                filtered.push((id.clone(), m.clone()));
            }
        }
    }
    Ok((filtered, errors))
}

/// Return public wire metadata. Keep provider credentials in the connection factory.
pub fn rpc_model(state: &Path, id: &panda_core::ModelId) -> Result<serde_json::Value> {
    use serde_json::json;
    ensure!(valid_provider(&id.provider), "invalid model provider");
    let configured = Catalog::read(state)?;
    if let Some(endpoint) = configured.providers.get(&id.provider) {
        let m = endpoint.model(&id.model)?;
        let mut value = json!({"id":m.id,"name":m.name.as_ref().unwrap_or(&m.id),"provider":id.provider,
            "api":endpoint.api.as_deref().unwrap_or("openai-completions"),"baseUrl":endpoint.base_url,"reasoning":true,"input":m.input,"contextWindow":m.context_window,"maxTokens":m.max_tokens});
        if let Some(cost) = &m.cost {
            value["cost"] = json!(cost);
        }
        return Ok(value);
    }
    if id.provider == "router" {
        let route = configured
            .routes
            .get(&id.model)
            .context("unknown virtual model")?;
        return Ok(
            json!({"id":id.model,"name":id.model,"provider":id.provider,"contextWindow":route.window(&configured),
            "reasoning":!route.efforts.is_empty(),"input":if route.image.is_some(){vec!["text","image"]}else{vec!["text"]}}),
        );
    }
    let (api, url) = match id.provider.as_str() {
        "deepseek" => ("openai-completions", "https://api.deepseek.com"),
        "openrouter" => ("openai-completions", "https://openrouter.ai/api/v1"),
        "openai-codex" => ("openai-codex-responses", "https://chatgpt.com/backend-api"),
        name if panda_providers::native::builtin(name).is_some() => {
            let (family, url) = panda_providers::native::builtin(name).unwrap();
            (family.name(), url)
        }
        _ => return Ok(json!({"id":id.model,"name":id.model,"provider":id.provider})),
    };
    let mut value = json!({"id":id.model,"name":id.model,"provider":id.provider,"api":api,"baseUrl":url,
        "reasoning":true,"input":if id.provider=="deepseek"{vec!["text"]}else{vec!["text","image"]}});
    let path = state.join(format!("catalog-{}.json", id.provider));
    if path.try_exists()? {
        let rows: Vec<(String, Option<u64>)> = serde_json::from_slice(&fs::read(path)?)?;
        if let Some(n) = rows
            .into_iter()
            .find(|(m, _)| m == &id.model)
            .and_then(|(_, n)| n)
        {
            value["contextWindow"] = json!(n);
        }
    }
    if let Some(n) = crate::output_limit(state, &id.provider, &id.model)? {
        value["maxTokens"] = json!(n);
    }
    if id.provider == "openrouter" && state.join("catalog-openrouter-prices.json").try_exists()? {
        let prices: BTreeMap<String, panda_core::TokenPrices> =
            serde_json::from_slice(&fs::read(state.join("catalog-openrouter-prices.json"))?)?;
        if let Some(cost) = prices.get(&id.model) {
            value["cost"] = json!(cost);
        }
    }
    Ok(value)
}
