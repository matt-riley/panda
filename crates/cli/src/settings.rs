use anyhow::{Context, Result};
use panda_local::resources::optional_text;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Controls {
    soft_at: Option<u64>,
    hard_at: Option<u64>,
    cache_at: Option<u64>,
    cache_for_minutes: Option<u64>,
}
impl Controls {
    fn values(&self) -> [(&'static str, &'static str, Option<u64>); 4] {
        [
            ("softAt", "--compact-soft-at", self.soft_at),
            ("hardAt", "--compact-at", self.hard_at),
            ("cacheAt", "--compact-cache-at", self.cache_at),
            (
                "cacheForMinutes",
                "--compact-cache-for",
                self.cache_for_minutes,
            ),
        ]
    }
    pub fn set(&mut self, flag: &str, n: u64) -> Result<()> {
        let target = match flag {
            "--compact-soft-at" => &mut self.soft_at,
            "--compact-at" => &mut self.hard_at,
            "--compact-cache-at" => &mut self.cache_at,
            "--compact-cache-for" => &mut self.cache_for_minutes,
            _ => anyhow::bail!("unknown compaction control {flag}"),
        };
        *target = Some(n);
        self.validate()
    }
    fn validate(self) -> Result<()> {
        for n in [self.soft_at, self.hard_at, self.cache_at]
            .into_iter()
            .flatten()
        {
            anyhow::ensure!(
                (1..=100).contains(&n),
                "compaction percentages must be 1 to 100"
            );
        }
        anyhow::ensure!(
            self.cache_for_minutes.is_none_or(|n| n <= u64::MAX / 60),
            "cache duration is too large"
        );
        Ok(())
    }
    fn overlay(&mut self, other: Self) {
        self.soft_at = other.soft_at.or(self.soft_at);
        self.hard_at = other.hard_at.or(self.hard_at);
        self.cache_at = other.cache_at.or(self.cache_at);
        self.cache_for_minutes = other.cache_for_minutes.or(self.cache_for_minutes);
    }
}
#[derive(Default, Deserialize)]
struct Retention {
    #[serde(rename = "keepRecentTokens")]
    keep: Option<u64>,
}
#[derive(Default, Deserialize)]
struct Compaction {
    #[serde(flatten)]
    controls: Controls,
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
    controls: Controls,
    control_sources: BTreeMap<&'static str, PathBuf>,
    pub invocation_controls: Controls,
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
            layer
                .compaction
                .controls
                .validate()
                .with_context(|| format!("reading {}", path.display()))?;
            for (name, _, value) in layer.compaction.controls.values() {
                if value.is_some() {
                    settings.control_sources.insert(name, path.clone());
                }
            }
            settings.controls.overlay(layer.compaction.controls);
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

    pub fn thresholds(&self) -> panda_core::native_compaction::Thresholds {
        let mut c = self.controls;
        c.overlay(self.invocation_controls);
        let hard = c.hard_at.unwrap_or(70).min(90);
        panda_core::native_compaction::Thresholds {
            soft: c.soft_at.unwrap_or(50).min(hard.saturating_sub(1)) as f64 / 100.,
            hard: hard as f64 / 100.,
            cache_share: c.cache_at.unwrap_or(70) as f64 / 100.,
            cache_age_secs: c.cache_for_minutes.unwrap_or(30) * 60,
        }
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
        let controls = self.thresholds();
        let effective = [
            format!("{:.0}%", controls.soft * 100.),
            format!("{:.0}%", controls.hard * 100.),
            format!("{:.0}%", controls.cache_share * 100.),
            (controls.cache_age_secs / 60).to_string(),
        ];
        let mut lines = vec![
            format!("compaction.keepRecentTokens = {keep} ({origin})"),
            format!(
                "skillIndex = {} ({})",
                self.skill_index,
                source(self.index_source.as_deref())
            ),
        ];
        for ((name, flag, invocation), value) in
            self.invocation_controls.values().into_iter().zip(effective)
        {
            let origin = if invocation.is_some() {
                flag.into()
            } else {
                source(self.control_sources.get(name).map(PathBuf::as_path))
            };
            lines.push(format!("compaction.{name} = {value} ({origin})"));
        }
        lines.join("\n")
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
