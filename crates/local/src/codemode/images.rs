use super::{MAX_CALLS, MAX_IMAGE_URL_BYTES};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use panda_core::ImagePosition;
use std::{fs::File, io::Write};

#[derive(Default)]
pub(super) struct Selected {
    urls: Vec<String>,
    bytes: usize,
}

fn contents(url: &str) -> Result<(&str, Vec<u8>)> {
    let (mime, data) = url
        .strip_prefix("data:")
        .and_then(|v| v.split_once(";base64,"))
        .context("expected a local image data URL")?;
    let bytes = STANDARD.decode(data).context("invalid image base64")?;
    ensure!(
        bytes.len() <= crate::files::MAX_IMAGE_BYTES,
        "image exceeds 8 MiB"
    );
    ensure!(
        panda_coding::image_mime_type(&bytes) == Some(mime),
        "unsupported image or MIME mismatch"
    );
    Ok((mime, bytes))
}

impl Selected {
    pub fn push(&mut self, url: String) -> Result<()> {
        ensure!(
            self.bytes + url.len() <= MAX_IMAGE_URL_BYTES && self.urls.len() < MAX_CALLS,
            "Codemode image budget exceeded"
        );
        contents(&url)?;
        self.bytes += url.len();
        self.urls.push(url);
        Ok(())
    }

    pub fn finish(
        self,
        root: &File,
        automatic: Vec<String>,
        offsets: &[usize],
        preview: &super::Preview,
    ) -> Result<(Vec<String>, Vec<ImagePosition>)> {
        ensure!(
            offsets.len() == self.urls.len(),
            "invalid image output positions"
        );
        if self.urls.is_empty() {
            return Ok((automatic, vec![]));
        }
        let mut positions = Vec::new();
        for (url, &offset) in self.urls.iter().zip(offsets) {
            let (mime, bytes) = contents(url)?;
            let ext = mime.strip_prefix("image/").unwrap();
            let (label, _) =
                crate::output_log::create_artifact(root, "codemode", ext, |f| f.write_all(&bytes))?;
            positions.push(ImagePosition {
                offset: preview.offset(offset),
                label,
            });
        }
        Ok((self.urls, positions))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregate_emitted_image_budget_is_independent_of_read_count() {
        let mut bytes = vec![0; 5 * 1024 * 1024];
        bytes[..6].copy_from_slice(b"GIF89a");
        let url = format!("data:image/gif;base64,{}", STANDARD.encode(&bytes));
        let mut selected = Selected::default();
        selected.push(url.clone()).unwrap();
        assert!(
            selected
                .push(url)
                .unwrap_err()
                .to_string()
                .contains("budget")
        );
        assert_eq!(selected.urls.len(), 1);
    }
}
