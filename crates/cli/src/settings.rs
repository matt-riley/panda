use anyhow::{Context, Result};
use panda_local::resources::optional_text;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Default, Deserialize)]
struct Compaction {
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
    pub keep: Option<u64>,
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
            if let Some(value) = layer.skill_index {
                settings.skill_index = value;
                settings.index_source = Some(path);
            }
        }
        Ok(settings)
    }

    pub fn describe(&self, cli: Option<u64>) -> String {
        let source = |path: Option<&Path>| {
            path.map_or_else(|| "default".into(), |p| p.display().to_string())
        };
        let keep = cli
            .or(self.keep)
            .map_or_else(|| "automatic".into(), |n| n.to_string());
        let origin = if cli.is_some() {
            "--keep-recent-tokens".into()
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
