//! Install immutable snapshots of a package's declared Markdown resources.
use crate::{catalog, resources::optional_text};
use anyhow::{Context, Result, bail, ensure};
use globset::{Glob, GlobMatcher};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Deserialize)]
struct Manifest {
    name: String,
    #[serde(default)]
    pi: Option<Resources>,
}
#[derive(Default, Deserialize)]
struct Resources {
    #[serde(default)]
    prompts: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
}
#[derive(Default, Deserialize, Serialize)]
struct Bundle {
    prompts: BTreeMap<String, String>,
    skills: BTreeMap<String, String>,
}
#[derive(Default)]
pub struct Active {
    pub prompts: BTreeMap<String, String>,
    pub skills: BTreeMap<PathBuf, String>,
}

fn rules(root: &Path, patterns: Vec<String>) -> Result<Vec<(bool, GlobMatcher)>> {
    patterns
        .into_iter()
        .map(|pattern| {
            let (enable, raw) = pattern
                .strip_prefix('!')
                .map_or((true, pattern.as_str()), |s| (false, s));
            let raw = raw.trim_start_matches("./");
            ensure!(
                !raw.is_empty()
                    && Path::new(raw)
                        .components()
                        .all(|c| matches!(c, Component::Normal(_))),
                "invalid package resource path {raw}"
            );
            let pat = if root.join(raw).is_dir() {
                format!("{raw}/**")
            } else {
                raw.to_owned()
            };
            Ok((enable, Glob::new(&pat)?.compile_matcher()))
        })
        .collect()
}
fn matches(rules: &[(bool, GlobMatcher)], path: &Path) -> bool {
    rules
        .iter()
        .rfind(|(_, g)| g.is_match(path))
        .is_some_and(|(include, _)| *include)
}
fn collect(root: &Path) -> Result<(String, Bundle)> {
    let raw = optional_text(&root.join("package.json"))?.context("package.json missing")?;
    let manifest: Manifest = serde_json::from_str(&raw).context("invalid package.json")?;
    let parts: Vec<_> = manifest.name.split('/').collect();
    let valid_part = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    ensure!(
        manifest.name.len() <= 214
            && match parts.as_slice() {
                [name] => valid_part(name),
                [scope, name] =>
                    scope.strip_prefix('@').is_some_and(valid_part) && valid_part(name),
                _ => false,
            },
        "invalid package name"
    );
    let resources = manifest.pi.unwrap_or_else(|| Resources {
        prompts: vec!["prompts/**".into()],
        skills: vec!["skills/**".into()],
    });
    let prompts = rules(root, resources.prompts)?;
    let skills = rules(root, resources.skills)?;
    let mut bundle = Bundle::default();
    let mut total = 0usize;
    for f in walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(16)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | "node_modules")))
    {
        let f = f?;
        let path = f.path();
        if !f.file_type().is_file() || path.extension() != Some(OsStr::new("md")) {
            continue;
        }
        let rel = path.strip_prefix(root)?;
        let (files, label) = if matches(&skills, rel) && f.file_name() == "SKILL.md" {
            (&mut bundle.skills, path.parent().and_then(Path::file_name))
        } else if matches(&prompts, rel) {
            (&mut bundle.prompts, path.file_stem())
        } else {
            continue;
        };
        let label = label
            .and_then(OsStr::to_str)
            .context("invalid resource name")?;
        ensure!(!matches!(label, "" | "." | ".."), "invalid resource name");
        let body = optional_text(path)?.context("resource disappeared")?;
        total = total.saturating_add(body.len());
        ensure!(total <= 8 * 1024 * 1024, "package Markdown exceeds 8 MiB");
        ensure!(
            files.insert(label.into(), body).is_none(),
            "duplicate package resource {label}"
        );
    }
    Ok((manifest.name, bundle))
}
fn command(cmd: &mut Command) -> Result<()> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("starting {name}"))?;
    ensure!(status.success(), "{name} exited with {status}");
    Ok(())
}
fn load_source(source: &str) -> Result<(String, Bundle, String)> {
    let tmp = tempfile::tempdir()?;
    let mut stored_source = source.to_owned();
    let root = if let Some(spec) = source.strip_prefix("npm:") {
        ensure!(
            !spec.is_empty() && !spec.starts_with('-'),
            "missing npm package"
        );
        let spec = if Path::new(spec).is_file() {
            let path = Path::new(spec).canonicalize()?;
            stored_source = format!("npm:{}", path.display());
            path.to_str().context("non UTF-8 npm source")?.to_owned()
        } else {
            spec.to_owned()
        };
        let install = tmp.path().join("install");
        command(
            Command::new("npm")
                .current_dir(tmp.path())
                .args([
                    "install",
                    "--ignore-scripts",
                    "--no-audit",
                    "--no-fund",
                    "--no-package-lock",
                    "--save",
                    "--prefix",
                ])
                .arg(&install)
                .arg("--")
                .arg(spec),
        )?;
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(install.join("package.json"))?)?;
        let deps = config["dependencies"]
            .as_object()
            .context("npm installation has no dependency")?;
        ensure!(deps.len() == 1, "expected one npm package");
        let name = deps.keys().next().unwrap();
        let root = install.join("node_modules").join(name).canonicalize()?;
        ensure!(
            root.starts_with(tmp.path().canonicalize()?),
            "npm package escaped installation"
        );
        root
    } else if source.starts_with("git:") || source.starts_with("https://") {
        let spec = source.strip_prefix("git:").unwrap_or(source);
        let split = spec.rsplit_once('#').or_else(|| {
            spec.rsplit_once('@')
                .filter(|(url, r)| url.contains('/') && !r.contains('/'))
        });
        let (url, reference) = split.map_or((spec, None), |(u, r)| (u, Some(r)));
        let url = if url.starts_with("github.com/") {
            format!("https://{url}")
        } else if Path::new(url).exists() {
            Path::new(url)
                .canonicalize()?
                .to_str()
                .context("non UTF-8 Git source")?
                .into()
        } else {
            url.to_owned()
        };
        ensure!(
            !url.is_empty() && !url.starts_with('-'),
            "invalid Git source"
        );
        stored_source = match reference {
            Some(r) => format!("git:{url}#{r}"),
            None => format!("git:{url}"),
        };
        let dest = tmp.path().join("repo");
        let git = || {
            let mut c = Command::new("git");
            c.args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.file.allow=always",
                "-c",
                "protocol.https.allow=always",
                "-c",
                "protocol.ssh.allow=always",
            ]);
            c
        };
        command(
            git()
                .args(["clone", "--quiet", "--no-checkout", "--", &url])
                .arg(&dest),
        )?;
        let reference = reference.unwrap_or("HEAD");
        ensure!(
            !reference.is_empty() && !reference.starts_with('-'),
            "invalid Git ref"
        );
        let mut commit = None;
        for candidate in [
            reference.to_owned(),
            format!("refs/remotes/origin/{reference}"),
        ] {
            let rev = git()
                .arg("-C")
                .arg(&dest)
                .args(["rev-parse", "--verify", "--end-of-options"])
                .arg(format!("{candidate}^{{commit}}"))
                .output()?;
            if rev.status.success() {
                commit = Some(String::from_utf8(rev.stdout)?.trim().to_owned());
                break;
            }
        }
        let commit = commit.with_context(|| format!("unknown Git revision {reference}"))?;
        command(
            git()
                .arg("-C")
                .arg(&dest)
                .args(["checkout", "--quiet", "--detach", &commit]),
        )?;
        dest
    } else {
        let path = Path::new(source).canonicalize()?;
        let (name, b) = collect(&path)?;
        return Ok((name, b, path.to_str().context("non UTF-8 path")?.into()));
    };
    let (name, b) = collect(&root.canonicalize()?)?;
    Ok((name, b, stored_source))
}
fn validate_filter(filter: &str) -> Result<()> {
    for pattern in filter.split(',') {
        let kind = pattern.split('/').next().unwrap();
        ensure!(
            matches!(kind, "all" | "skills" | "prompts"),
            "unknown package filter {pattern}"
        );
        Glob::new(pattern)?;
    }
    Ok(())
}
fn enabled(filter: &str, kind: &str, name: &str) -> Result<bool> {
    let resource = format!("{kind}/{name}");
    for p in filter.split(',') {
        if p == "all" || p == kind || Glob::new(p)?.compile_matcher().is_match(&resource) {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn manage(state: &Path, project: &Path, args: &[String]) -> Result<String> {
    let mut args = args.iter().map(String::as_str).peekable();
    let action = args.next().context("expected package command")?;
    let mut project_scope = false;
    let mut filter = None;
    let mut target = None;
    while let Some(arg) = args.next() {
        match arg {
            "--project" => project_scope = true,
            "--only" => {
                let f = args.next().context("missing --only filter")?;
                validate_filter(f)?;
                filter = Some(f)
            }
            name => ensure!(
                target.replace(name).is_none(),
                "unexpected package argument {name}"
            ),
        }
    }
    let scope = if project_scope {
        catalog::scope(project)?
    } else {
        "user".to_owned()
    };
    let label = if project_scope { "project" } else { "user" };
    if action == "list" {
        let Some(db) = catalog::database(state, false)? else {
            return Ok(String::new());
        };
        let mut stmt=db.prepare("SELECT scope,name,source,filter FROM packages WHERE scope IN ('user',?) ORDER BY scope,name")?;
        let rows = stmt.query_map([catalog::scope(project)?], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (s, n, source, f) = row?;
            out.push(format!(
                "{} {n} · {source} · {f}",
                if s == "user" { "user" } else { "project" }
            ))
        }
        return Ok(out.join("\n"));
    }
    let target = target.context("expected package name or source")?;
    let db = catalog::database(state, true)?.unwrap();
    match action {
        "install" => {
            let (name, bundle, source) = load_source(target)?;
            db.execute(
                "INSERT INTO packages VALUES (?,?,?,?,?,1)",
                params![
                    scope,
                    name,
                    source,
                    filter.unwrap_or("all"),
                    serde_json::to_string(&bundle)?
                ],
            )
            .context("installing package (use update for an existing name)")?;
            Ok(format!("Installed {label} package {name}"))
        }
        "update" => {
            let (source, version): (String, i64) = db
                .query_row(
                    "SELECT source,version FROM packages WHERE scope=? AND name=?",
                    params![scope, target],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .context("package is not installed in this scope")?;
            let (name, bundle, _) = load_source(&source)?;
            ensure!(name == target, "updated package changed its name");
            let n=db.execute("UPDATE packages SET bundle=?,filter=coalesce(?,filter),version=version+1 WHERE scope=? AND name=? AND version=?",params![serde_json::to_string(&bundle)?,filter,scope,name,version])?;
            ensure!(n == 1, "package changed during update");
            Ok(format!("Updated {label} package {name}"))
        }
        "activate" => {
            let filter = filter.context("activate requires --only FILTER")?;
            ensure!(
                db.execute(
                    "UPDATE packages SET filter=?,version=version+1 WHERE scope=? AND name=?",
                    params![filter, scope, target]
                )? == 1,
                "package not found"
            );
            Ok(format!("Activated {filter} for {target}"))
        }
        "remove" => {
            ensure!(
                db.execute(
                    "DELETE FROM packages WHERE scope=? AND name=?",
                    params![scope, target]
                )? == 1,
                "package not found"
            );
            Ok(format!("Removed {label} package {target}"))
        }
        _ => bail!("use install, list, activate, update or remove"),
    }
}
pub fn active(state: &Path, project: &Path, trusted: bool) -> Result<Active> {
    let mut result = Active::default();
    let Some(db) = catalog::database(state, false)? else {
        return Ok(result);
    };
    let project = if trusted {
        catalog::scope(project)?
    } else {
        String::new()
    };
    let mut stmt=db.prepare("SELECT filter,bundle,name,scope FROM packages WHERE scope IN ('user',?) ORDER BY (scope='user') DESC,name")?;
    let mut rows = stmt.query([project])?;
    while let Some(row) = rows.next()? {
        let filter: String = row.get(0)?;
        let raw: String = row.get(1)?;
        let package: String = row.get(2)?;
        let scope: String = row.get(3)?;
        let bundle: Bundle = serde_json::from_str(&raw)?;
        for (name, body) in bundle.prompts {
            if enabled(&filter, "prompts", &name)? {
                result.prompts.entry(name).or_insert(body);
            }
        }
        for (name, body) in bundle.skills {
            if enabled(&filter, "skills", &name)? {
                let label = if scope == "user" { "user" } else { "project" };
                let id = Path::new("@packages")
                    .join(label)
                    .join(&package)
                    .join(name)
                    .join("SKILL.md");
                result.skills.insert(id, body);
            }
        }
    }
    Ok(result)
}
