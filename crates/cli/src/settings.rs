use anyhow::{Context, Result};
use panda_local::resources::optional_text;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize)]
struct Retention {
    #[serde(rename = "keepRecentTokens")]
    keep: Option<u64>,
}
#[derive(Default, Deserialize)]
struct Compaction {
    #[serde(default, rename = "modelOverrides")]
    models: BTreeMap<String, Retention>,
    #[serde(rename = "keepRecentTokens")]
    keep: Option<u64>,
}
#[derive(Default, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    compaction: Compaction,
    #[serde(rename = "skillIndex")]
    skill_index: Option<bool>,
}

#[derive(Default)]
pub struct Settings {
    keep: Option<u64>,
    models: BTreeMap<String, (u64, PathBuf)>,
    keep_source: Option<PathBuf>,
    pub skill_index: bool,
    index_source: Option<PathBuf>,
}

impl Settings {
    pub fn read(state: &Path, project: &Path) -> Result<Self> {
        let mut settings = Self::default();
        for path in [
            state.join("settings.json"),
            project.join(".panda/settings.json"),
        ] {
            let Some(text) = optional_text(&path)? else {
                continue;
            };
            let layer: SettingsFile = serde_json::from_str(&text)
                .with_context(|| format!("reading {}", path.display()))?;
            if let Some(value) = layer.compaction.keep {
                settings.keep = Some(value);
                settings.keep_source = Some(path.clone());
            }
            for (model, setting) in layer.compaction.models {
                if let Some(value) = setting.keep {
                    settings.models.insert(model, (value, path.clone()));
                }
            }
            if let Some(value) = layer.skill_index {
                settings.skill_index = value;
                settings.index_source = Some(path);
            }
        }
        Ok(settings)
    }

    fn model_keep(&self, provider: &str, model: &str) -> Option<&(u64, PathBuf)> {
        self.models.get(&format!("{provider}/{model}"))
    }
    pub fn retention(&self, provider: &str, model: &str) -> Option<u64> {
        self.model_keep(provider, model)
            .map(|(n, _)| *n)
            .or(self.keep)
    }
    pub fn describe(&self, cli: Option<u64>, provider: &str, model: &str) -> String {
        let source = |path: Option<&Path>| {
            path.map_or_else(|| "default".into(), |p| p.display().to_string())
        };
        let keep = cli
            .or(self.retention(provider, model))
            .map_or_else(|| "automatic".into(), |n| n.to_string());
        let origin = if cli.is_some() {
            "--keep-recent-tokens".into()
        } else if let Some((_, path)) = self.model_keep(provider, model) {
            format!("{} · model override {provider}/{model}", path.display())
        } else {
            source(self.keep_source.as_deref())
        };
        format!(
            "compaction.keepRecentTokens = {keep} ({origin})\nskillIndex = {} ({})",
            self.skill_index,
            source(self.index_source.as_deref())
        )
    }
}

pub fn system_file(
    state: &Path,
    project: &Path,
    trusted: bool,
    name: &str,
) -> Result<Option<String>> {
    if trusted && let Some(text) = optional_text(&project.join(".pi").join(name))? {
        return Ok(Some(text));
    }
    optional_text(&state.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn per_model_retention_overrides_global_and_zero_survives_layering() {
        let state = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".panda")).unwrap();
        std::fs::write(state.path().join("settings.json"),r#"{"compaction":{"keepRecentTokens":245,"modelOverrides":{"openrouter/org/model":{"keepRecentTokens":10},"deepseek/chat":{"keepRecentTokens":99}}}}"#).unwrap();
        std::fs::write(repo.path().join(".panda/settings.json"),r#"{"compaction":{"keepRecentTokens":100,"modelOverrides":{"openrouter/org/model":{"keepRecentTokens":0}}}}"#).unwrap();
        let settings = Settings::read(
            &state.path().canonicalize().unwrap(),
            &repo.path().canonicalize().unwrap(),
        )
        .unwrap();
        assert_eq!(settings.retention("deepseek", "chat"), Some(99));
        assert_eq!(settings.retention("openrouter", "org/model"), Some(0));
        assert_eq!(settings.retention("other", "chat"), Some(100));
        assert!(
            settings
                .describe(Some(21), "openrouter", "org/model")
                .contains("= 21 (--keep-recent-tokens)")
        );
        assert!(
            settings
                .describe(None, "openrouter", "org/model")
                .contains("= 0 (")
        );
    }
}
