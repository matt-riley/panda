use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use serde::Deserialize;
use std::{cell::RefCell, collections::BTreeMap, fs, io, path::Path};

const ACTIONS: [(&str, KeyCode, &str); 6] = [
    ("up", KeyCode::Up, "Up"),
    ("down", KeyCode::Down, "Down"),
    ("select", KeyCode::Enter, "Enter"),
    ("close", KeyCode::Esc, "Esc"),
    ("pageUp", KeyCode::PageUp, "PageUp"),
    ("pageDown", KeyCode::PageDown, "PageDown"),
];
type Chord = (KeyCode, KeyModifiers);

#[derive(Default)]
struct Config {
    keys: BTreeMap<String, (Chord, String)>,
    name: String,
    colors: BTreeMap<String, Color>,
}
thread_local! { static CONFIG: RefCell<Config> = RefCell::new(Config::default()); }

fn chord(value: &str) -> Result<Chord> {
    for (_, code, name) in ACTIONS {
        if value.eq_ignore_ascii_case(name) {
            return Ok((code, KeyModifiers::NONE));
        }
    }
    let lower = value.to_ascii_lowercase();
    if let Some(n) = lower
        .strip_prefix('f')
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|n| (1..=12).contains(n))
    {
        return Ok((KeyCode::F(n), KeyModifiers::NONE));
    }
    if let Some(key) = lower
        .strip_prefix("ctrl-")
        .filter(|s| s.len() == 1 && s.as_bytes()[0].is_ascii_lowercase())
    {
        let c = key.as_bytes()[0] as char;
        ensure!(
            !"cdflnprsizmhjq".contains(c),
            "reserved picker or terminal shortcut {value}"
        );
        return Ok((KeyCode::Char(c), KeyModifiers::CONTROL));
    }
    bail!("unknown key {value} (use a navigation key, F1–F12, or Ctrl-letter)")
}

fn optional<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Theme {
    accent: Option<String>,
    muted: Option<String>,
    selection: Option<String>,
}
fn palette(state: &Path, name: &str) -> Result<BTreeMap<String, Color>> {
    if name == "default" {
        return Ok(BTreeMap::new());
    }
    ensure!(
        !name.is_empty()
            && name.len() <= 80
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid theme name"
    );
    let path = state.join("themes").join(format!("{name}.json"));
    let theme = optional::<Theme>(&path)?
        .with_context(|| format!("theme file missing: {}", path.display()))?;
    let mut colors = BTreeMap::new();
    for (key, value) in [
        ("accent", theme.accent),
        ("muted", theme.muted),
        ("selection", theme.selection),
    ] {
        if let Some(value) = value {
            let rgb = value
                .strip_prefix('#')
                .filter(|s| s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|s| u32::from_str_radix(s, 16).ok())
                .with_context(|| format!("{}: {key} needs #RRGGBB", path.display()))?;
            colors.insert(
                key.to_owned(),
                Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8),
            );
        }
    }
    Ok(colors)
}
pub fn load(state: &Path) -> Result<()> {
    let path = state.join("keybindings.json");
    let mut values = optional::<BTreeMap<String, String>>(&path)?.unwrap_or_default();
    let mut keys: BTreeMap<String, (Chord, String)> = BTreeMap::new();
    for (name, _, default) in ACTIONS {
        let label = values.remove(name).unwrap_or_else(|| default.into());
        let key = chord(&label).with_context(|| format!("{}: {name}", path.display()))?;
        if let Some((other, _)) = keys.iter().find(|(_, v)| v.0 == key) {
            bail!("{}: {name} and {other} both use {label}", path.display());
        }
        keys.insert(name.into(), (key, label));
    }
    if let Some(unknown) = values.keys().next() {
        bail!("{}: unknown picker action {unknown}", path.display());
    }
    let name = optional::<String>(&state.join("theme.json"))?.unwrap_or_else(|| "default".into());
    let colors = palette(state, &name)?;
    CONFIG.with(|v| *v.borrow_mut() = Config { keys, name, colors });
    Ok(())
}
pub fn theme(state: &Path, name: Option<&str>) -> Result<String> {
    if let Some(name) = name {
        let colors = palette(state, name)?;
        let mut tmp = tempfile::NamedTempFile::new_in(state)?;
        serde_json::to_writer(tmp.as_file_mut(), name)?;
        tmp.persist(state.join("theme.json"))?;
        CONFIG.with(|v| {
            let mut c = v.borrow_mut();
            c.name = name.into();
            c.colors = colors;
        });
    }
    Ok(CONFIG.with(|v| v.borrow().name.clone()))
}
pub fn color(role: &str) -> Option<Color> {
    CONFIG.with(|v| v.borrow().colors.get(role).copied())
}

pub fn label(action: &str) -> String {
    CONFIG
        .with(|v| v.borrow().keys.get(action).map(|v| v.1.clone()))
        .unwrap_or_else(|| ACTIONS.iter().find(|a| a.0 == action).unwrap().2.into())
}
pub fn help() -> String {
    ACTIONS
        .iter()
        .map(|(name, _, _)| format!("{} {name}", label(name)))
        .collect::<Vec<_>>()
        .join(" · ")
}
pub fn picker_event(mut event: KeyEvent) -> Option<KeyEvent> {
    CONFIG.with(|v| {
        let config = v.borrow();
        if config.keys.is_empty() {
            return Some(event);
        }
        for (name, standard, _) in ACTIONS {
            if config.keys[name].0 == (event.code, event.modifiers) {
                event.code = standard;
                event.modifiers = KeyModifiers::NONE;
                return Some(event);
            }
        }
        (!ACTIONS
            .iter()
            .any(|(_, code, _)| *code == event.code && event.modifiers.is_empty()))
        .then_some(event)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_and_conflicting_files_name_the_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keybindings.json");
        for (json, expected) in [
            (r#"{"select":"Down"}"#, "both use"),
            (r#"{"close":"Ctrl-C"}"#, "reserved"),
            ("broken", "line 1"),
            (r#"{"approve":"F2"}"#, "approve"),
        ] {
            fs::write(&path, json).unwrap();
            let message = format!("{:#}", load(dir.path()).unwrap_err());
            assert!(
                message.contains("keybindings.json") && message.contains(expected),
                "{message}"
            );
        }
    }
}
