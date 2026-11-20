use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ToolDetails {
    Edit {
        first_changed_line: Option<usize>,
        patch: Vec<Replacement>,
    },
    Codemode {
        elapsed_ms: u64,
        success: bool,
        images: Vec<ImagePosition>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<OutputItem>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Replacement {
    pub start_byte: usize,
    pub end_byte: usize,
    pub replacement: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImagePosition {
    pub offset: usize,
    pub label: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Text { start: usize, end: usize },
    Console { start: usize, end: usize },
    Image { index: usize },
}

pub enum Part<'a> {
    Text(&'a str),
    Image(&'a str),
}

/// Positions are byte offsets into the bounded UTF-8 text preview. Legacy results
/// place every image after the text. Invalid archived positions use that same layout.
pub fn parts<'a>(
    text: &'a str,
    urls: &'a [String],
    details: Option<&ToolDetails>,
) -> Vec<Part<'a>> {
    let mut parts = Vec::new();
    if let Some(ToolDetails::Codemode { items, images, .. }) = details
        && !items.is_empty()
        && images.len() == urls.len()
    {
        let ordered: Option<Vec<_>> = items
            .iter()
            .map(|item| match item {
                OutputItem::Text { start, end } | OutputItem::Console { start, end } => {
                    text.get(*start..*end).map(Part::Text)
                }
                OutputItem::Image { index } => urls.get(*index).map(|url| Part::Image(url)),
            })
            .collect();
        if let Some(ordered) = ordered {
            return ordered;
        }
    }
    let mut cursor = 0;
    if let Some(ToolDetails::Codemode { images, .. }) = details
        && images.len() == urls.len()
        && images.iter().all(|p| text.is_char_boundary(p.offset))
        && images.windows(2).all(|p| p[0].offset <= p[1].offset)
    {
        for (pos, url) in images.iter().zip(urls) {
            if cursor < pos.offset {
                parts.push(Part::Text(&text[cursor..pos.offset]));
            }
            parts.push(Part::Image(url));
            cursor = pos.offset;
        }
        if cursor < text.len() {
            parts.push(Part::Text(&text[cursor..]));
        }
    } else {
        parts.push(Part::Text(text));
        parts.extend(urls.iter().map(|u| Part::Image(u)));
    }
    parts
}
