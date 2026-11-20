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
    pub providers: BTreeMap<String, Endpoint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Endpoint {
    base_url: String,
    pub api_key: Option<String>,
    api: Option<String>,
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
                valid_provider(id) && !matches!(id.as_str(), "openai" | "codex"),
                "invalid models.json provider id {id}"
            );
            let url = panda_providers::endpoint_url(&ep.base_url)
                .with_context(|| format!("invalid baseUrl for {id} in models.json"))?;
            ensure!(url, "invalid baseUrl for {id} in models.json");
            ensure!(
                ep.api
                    .as_deref()
                    .is_none_or(|api| api == "openai-completions"),
                "unsupported API for {id} in models.json"
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
