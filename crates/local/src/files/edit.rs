use anyhow::{Context, Result, ensure};
use panda_coding::field;
use serde_json::Value;
use std::ops::Range;

struct Replacement<'a> {
    range: Range<usize>,
    text: &'a str,
}

fn locate<'a>(original: &str, item: &'a Value) -> Result<Replacement<'a>> {
    let old = field(item, "old")?;
    let text = field(item, "new")?;
    ensure!(!old.is_empty(), "old must not be empty");
    let start = original
        .find(old)
        .context("target missing from original file")?;
    // The last match also detects ambiguous occurrences that overlap the first.
    ensure!(
        original.rfind(old) == Some(start),
        "target is ambiguous in original file"
    );
    Ok(Replacement {
        range: start..start + old.len(),
        text,
    })
}

pub(super) fn apply(original: &str, args: &Value) -> Result<(String, String)> {
    let items = match args.get("edits") {
        None => std::slice::from_ref(args),
        Some(batch) => {
            ensure!(
                args.get("old").is_none() && args.get("new").is_none(),
                "use either edits or old/new"
            );
            let items = batch.as_array().context("edits must be an array")?;
            ensure!(
                !items.is_empty(),
                "edits must contain at least one replacement"
            );
            for (index, item) in items.iter().enumerate() {
                ensure!(
                    item.as_object().is_some_and(|pair| pair
                        .keys()
                        .all(|key| matches!(key.as_str(), "old" | "new"))),
                    "edit {} must be an object containing only old and new",
                    index + 1
                );
            }
            items.as_slice()
        }
    };
    let mut replacements = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        replacements.push(locate(original, item).with_context(|| format!("edit {}", index + 1))?);
    }
    replacements.sort_unstable_by_key(|edit| edit.range.start);
    let mut updated = String::with_capacity(original.len());
    let mut cursor = 0;
    let mut hunk_ends = Vec::new();
    for edit in replacements {
        ensure!(
            edit.range.start >= cursor,
            "edit targets overlap in original file"
        );
        let gap = &original[cursor..edit.range.start];
        // An untouched newline separates independent hunks. Split both snapshots
        // at the same unchanged newline, even if earlier edits shifted the new lines.
        if cursor != 0
            && let Some(newline) = gap.find('\n')
        {
            hunk_ends.push((cursor + newline + 1, updated.len() + newline + 1));
        }
        updated.push_str(gap);
        updated.push_str(edit.text);
        cursor = edit.range.end;
    }
    updated.push_str(&original[cursor..]);
    hunk_ends.push((original.len(), updated.len()));
    let report = edit_report(original, &updated, hunk_ends);
    Ok((updated, report))
}

fn edit_report(original: &str, updated: &str, hunk_ends: Vec<(usize, usize)>) -> String {
    let mut report = String::from("saved");
    let (mut old_start, mut new_start) = (0, 0);
    let (mut old_base, mut new_base) = (0, 0);
    for (old_end, new_end) in hunk_ends {
        let old: Vec<_> = original[old_start..old_end].split_inclusive('\n').collect();
        let new: Vec<_> = updated[new_start..new_end].split_inclusive('\n').collect();
        let prior_len = report.len();
        append_hunk(&mut report, &old, &new, old_base, new_base);
        const OMITTED: &str = "\n  …";
        if report.len() > crate::MAX_TOOL_RESULT_BYTES - OMITTED.len() {
            report.truncate(prior_len);
            report.push_str(OMITTED);
            break;
        }
        old_base += old.len();
        new_base += new.len();
        old_start = old_end;
        new_start = new_end;
    }
    report
}

fn append_hunk(report: &mut String, old: &[&str], new: &[&str], old_base: usize, new_base: usize) {
    let start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let common_end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut append = |mark: &str, first: usize, lines: &[&str]| {
        use std::fmt::Write;
        for (offset, line) in lines.iter().take(8).enumerate() {
            let mut chars = line.trim_end_matches(['\r', '\n']).chars();
            let excerpt: String = chars.by_ref().take(256).collect();
            let ellipsis = if chars.next().is_some() { "…" } else { "" };
            let _ = write!(
                report,
                "\n{mark} {}  {excerpt}{ellipsis}",
                first + offset + 1
            );
        }
        if lines.len() > 8 {
            report.push_str("\n  …");
        }
    };
    let context = start.saturating_sub(2);
    append(" ", old_base + context, &old[context..start]);
    append("−", old_base + start, &old[start..old.len() - common_end]);
    append("+", new_base + start, &new[start..new.len() - common_end]);
    let tail = new.len() - common_end;
    append(" ", new_base + tail, &new[tail..tail + common_end.min(2)]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    proptest! {
        #[test]
        fn replacements_use_original_offsets(values in prop::collection::vec(any::<String>(), 1..16)) {
            let targets: Vec<_> = (0..values.len()).map(|i| format!("<{i}>")).collect();
            let edits: Vec<_> = targets.iter().zip(&values).rev()
                .map(|(old, new)| json!({"old":old, "new":new})).collect();
            let separator = "\r\nλ\n";
            let (updated, _) = apply(&targets.join(separator), &json!({"edits":edits})).unwrap();
            prop_assert_eq!(updated, values.join(separator));
        }
    }
}
