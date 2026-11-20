use crate::{picker, ui};
use anyhow::{Context, Result, ensure};
use panda_local::store::Disk;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub enum Choice {
    Pick,
    Open(i64),
    Fork(i64),
}

pub fn select(
    choice: Choice,
    here: &Path,
    dir: &Path,
    interactive: bool,
) -> Result<Option<(PathBuf, Disk)>> {
    let rows = Disk::all_workspaces(dir)?;
    let row = match choice {
        Choice::Pick => {
            ensure!(
                interactive,
                "--all-sessions needs an interactive terminal. Use --resume-any ID or --fork-here ID."
            );
            ensure!(!rows.is_empty(), "no saved sessions");
            let labels = rows
                .iter()
                .map(|r| {
                    ui::inline(
                        &format!("#{}  {}  {}", r.id, r.name, r.workspace.display()),
                        512,
                    )
                })
                .collect::<Vec<_>>();
            let Some(i) = picker::choose("Sessions across workspaces", &labels, 0)? else {
                return Ok(None);
            };
            &rows[i]
        }
        Choice::Open(id) | Choice::Fork(id) => rows
            .iter()
            .find(|r| r.id == id)
            .with_context(|| format!("unknown session {id}"))?,
    };
    let original = &row.workspace;
    let available = original
        .canonicalize()
        .is_ok_and(|p| p == *original && p.is_dir());
    enum Action {
        Open,
        Fork,
        Continue,
    }
    let action = match choice {
        Choice::Fork(_) => Action::Fork,
        Choice::Open(_) if available => Action::Open,
        Choice::Open(_) if !interactive => anyhow::bail!(
            "original workspace unavailable: {}. Use --fork-here {} to fork into the current workspace.",
            original.display(),
            row.id
        ),
        _ => {
            let first = if available {
                "Open original workspace"
            } else {
                "Continue here"
            };
            let choices = [
                first.to_owned(),
                "Fork into current workspace".into(),
                "Cancel".into(),
            ];
            let title = if available {
                "Choose workspace"
            } else {
                "Original workspace unavailable"
            };
            match picker::choose(title, &choices, 0)? {
                Some(0) if available => Action::Open,
                Some(0) => Action::Continue,
                Some(1) => Action::Fork,
                _ => return Ok(None),
            }
        }
    };
    let mut source = Disk::open(dir, original, Some(row.id))?;
    let result = match action {
        Action::Open => (original.clone(), source),
        Action::Continue => {
            source.continue_here(original, here)?;
            (here.into(), source)
        }
        Action::Fork => (here.into(), source.fork(dir, here, None, &row.name)?),
    };
    Ok(Some(result))
}
