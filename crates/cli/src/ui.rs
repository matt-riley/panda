use panda_core::Call;
use reedline::{DefaultPrompt, Prompt, PromptEditMode, PromptHistorySearch};
use std::borrow::Cow;

pub struct Status<'a>(pub &'a crate::screen::Screen);

impl Prompt for Status<'_> {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        self.0.body().into()
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        "".into()
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        "❯ ".into()
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        "│ ".into()
    }
    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        DefaultPrompt::default()
            .render_prompt_history_search_indicator(search)
            .into_owned()
            .into()
    }
}

pub fn inline(text: &str, limit: usize) -> String {
    let mut chars = text.chars().map(|c| if c.is_control() { ' ' } else { c });
    let mut out: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

pub fn header(call: &Call) -> String {
    let detail = ["path", "command", "query"]
        .into_iter()
        .find_map(|key| call.args[key].as_str());
    let mut line = format!("▸ {}", inline(&call.name, 32));
    if let Some(detail) = detail {
        line.push_str("  ");
        line.push_str(&inline(detail, 96));
    }
    line
}

pub fn result(text: &str, expanded: bool) -> String {
    if expanded {
        return super::safe(text);
    }
    if text.is_empty() {
        return "  │ (no output)".into();
    }
    let mut lines = text.lines();
    let mut clipped = false;
    let mut preview = Vec::new();
    for line in lines.by_ref().take(6) {
        let display = inline(line, 120);
        clipped |= line.chars().take(121).count() > 120;
        preview.push(format!("  │ {display}"));
    }
    if clipped || lines.next().is_some() {
        preview.push("  └ … :tools for full output".into());
    }
    preview.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn write_header_names_destination_without_dumping_contents() {
        let call = Call {
            name: "write".into(),
            id: "id".into(),
            args: json!({"path":"src/main.rs","text":"source"}),
        };
        assert_eq!(header(&call), "▸ write  src/main.rs");
    }

    #[test]
    fn long_results_advertise_full_output() {
        let preview = result(&"one\n".repeat(12), false);
        assert_eq!(preview.matches("one").count(), 6);
        assert!(preview.contains(":tools"));
        assert_eq!(result(&"one\n".repeat(12), true).matches("one").count(), 12);
    }

    proptest! {
        #[test]
        fn arbitrary_tool_text_stays_bounded_and_terminal_safe(input in any::<String>()) {
            let preview = result(&input, false);
            prop_assert!(preview.lines().count() <= 7);
            prop_assert!(!preview.chars().any(|c| c.is_control() && c != '\n'));
        }
    }
}

#[cfg(test)]
mod references {
    #[test]
    fn resolves_quoted_and_plain_files_but_skips_email() {
        assert_eq!(
            crate::attachments::paths("look @src/lib.rs and @\"my file.png\" a@host.test"),
            vec!["src/lib.rs", "my file.png"]
        );
    }
}
