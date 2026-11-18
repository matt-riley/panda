//! A reply keeps its Markdown source until layout. Token boundaries never become
//! document boundaries, so later input can complete fences and reference links.
use ratatui::{
    prelude::IntoCrossterm,
    style::Style,
    text::{Line, Span},
};
use std::sync::Mutex;
use unicode_width::UnicodeWidthStr;

#[derive(Clone)]
struct Theme;
impl tui_markdown::StyleSheet for Theme {
    fn heading(&self, _: u8) -> Style {
        Style::new().fg(crate::renderer::ACCENT).bold()
    }
    fn heading_marker(&self, _: u8) -> &str {
        ""
    }
    fn code(&self) -> Style {
        Style::new().light_yellow()
    }
    fn code_block_fence(&self) -> &str {
        "──"
    }
}

#[derive(Default)]
pub struct Reply {
    source: String,
    layout: Mutex<Option<(usize, Vec<Line<'static>>)>>,
}
impl Reply {
    pub fn push(&mut self, text: &str) {
        self.source.push_str(text);
        *self.layout.get_mut().unwrap() = None;
    }
    pub fn finish(&mut self, text: &str) {
        if self.source != text {
            self.source.clear();
            self.push(text);
        }
    }
    pub fn rows(&self, width: usize) -> Vec<Line<'static>> {
        let mut cached = self.layout.lock().unwrap();
        if let Some((w, rows)) = &*cached
            && *w == width
        {
            return rows.clone();
        }
        let input = crate::safe(&self.source).replace('\t', "    ");
        let options =
            tui_markdown::Options::new(Theme).table_width(width.clamp(1, u16::MAX as usize) as u16);
        let text = tui_markdown::from_str_with_options(&input, &options);
        let mut rows = Vec::new();
        for line in &text.lines {
            let mut row = Line::default();
            let mut used = 0;
            let mut append = |s: &str, style: Style| {
                let cells = s.width();
                if used > 0 && used + cells > width {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                if cells > width {
                    return;
                }
                if let Some(last) = row.spans.last_mut()
                    && last.style == style
                {
                    last.content.to_mut().push_str(s);
                } else {
                    row.spans.push(Span::styled(s.to_owned(), style));
                }
                used += cells;
            };
            for g in line.styled_graphemes(text.style) {
                // Character entities may decode to controls after source sanitization.
                if g.symbol.chars().any(char::is_control) {
                    continue;
                }
                if g.symbol.width() > width {
                    for ch in g.symbol.chars() {
                        append(ch.encode_utf8(&mut [0; 4]), g.style);
                    }
                } else {
                    append(g.symbol, g.style);
                }
            }
            rows.push(row);
        }
        *cached = Some((width, rows.clone()));
        rows
    }
}

pub fn ansi(line: &Line<'_>, color: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for span in &line.spans {
        if color && span.style != Style::default() {
            let style: ratatui::crossterm::style::ContentStyle = span.style.into_crossterm();
            write!(out, "{}", style.apply(span.content.as_ref())).unwrap();
        } else {
            out.push_str(&span.content);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SAMPLE: &str = "## Review\n\n- **bold** and `inline`\n- [see][doc]\n\n```rust\nlet x = \"**literal**\";\n```\n\n[doc]: https://example.org";

    #[test]
    fn every_character_boundary_including_partial_fences() {
        for width in [2, 11, 44, 80] {
            let mut resumed = Reply::default();
            resumed.finish(SAMPLE);
            let expected = resumed.rows(width);
            for (split, _) in SAMPLE.char_indices() {
                let mut live = Reply::default();
                live.push(&SAMPLE[..split]);
                let _ = live.rows(width);
                live.push(&SAMPLE[split..]);
                assert_eq!(live.rows(width), expected);
            }
        }
        let mut reply = Reply::default();
        reply.finish(SAMPLE);
        let rows = reply.rows(100);
        let plain = rows
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("Review") && !plain.contains("## Review"));
        assert!(plain.contains("bold and inline") && plain.contains("**literal**"));
        assert!(rows.iter().flat_map(|l| &l.spans).any(|s| {
            s.content.contains("bold")
                && s.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::BOLD)
        }));
    }

    proptest! {
        #[test]
        fn chunk_partitions_match_one_shot_after_each_redraw(
            tokens in prop::collection::vec(prop_oneof![
                Just("**".to_owned()), Just("\n```".to_owned()), Just("~".to_owned()),
                Just("\n- ".to_owned()),Just("&#27;".to_owned()),Just("🪀𑩗".to_owned()),
                Just("\n|---|---|\n".to_owned()),".{0,7}"
            ],0..36),
            cuts in prop::collection::vec(1usize..14,1..12), width in 2usize..96
        ) {
            let source=tokens.concat();
            let characters:Vec<_>=source.char_indices().map(|(i,_)|i).chain([source.len()]).collect();
            let mut live=Reply::default();
            let mut start=0;
            for amount in cuts.iter().cycle(){
                if start+1>=characters.len(){break;}
                let end=(start+amount).min(characters.len()-1);
                live.push(&source[characters[start]..characters[end]]);
                let mut resumed=Reply::default();
                resumed.finish(&source[..characters[end]]);
                let rows=live.rows(width);
                prop_assert!(rows.iter().all(|l|l.width()<=width && !ansi(l,false).chars().any(char::is_control)));
                prop_assert_eq!(rows,resumed.rows(width));
                start=end;
            }
        }
    }
}
