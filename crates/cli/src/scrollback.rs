//! Append-only terminal output. Only validated, bounded PNG data reaches a graphics protocol.
use base64::{Engine, engine::general_purpose::STANDARD};
use panda_core::{Part, ToolDetails};
use std::io::{self, Write};

const MAX_ENCODED: usize = 512 * 1024;
const MAX_IMAGES: usize = 4;

#[derive(Clone, Copy, PartialEq)]
enum Graphics {
    Text,
    Kitty,
    Iterm,
}

pub struct Scrollback {
    graphics: Graphics,
    remaining: usize,
    images_left: usize,
    streamed: String,
}

impl Scrollback {
    pub fn detect() -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        let graphics = if std::env::var_os("TMUX").is_some()
            || std::env::var_os("STY").is_some()
            || term.starts_with("screen")
            || term.starts_with("tmux")
        {
            Graphics::Text
        } else if term == "xterm-kitty" {
            Graphics::Kitty
        } else if std::env::var("TERM_PROGRAM").as_deref() == Ok("iTerm.app") {
            Graphics::Iterm
        } else {
            Graphics::Text
        };
        Self {
            graphics,
            remaining: MAX_ENCODED,
            images_left: MAX_IMAGES,
            streamed: String::new(),
        }
    }

    pub fn reset(&mut self) {
        self.remaining = MAX_ENCODED;
        self.images_left = MAX_IMAGES;
        self.streamed.clear();
    }

    pub fn stream(&mut self, text: &str) {
        if !text.is_empty() {
            print!("{}", crate::safe(text));
            let _ = io::stdout().flush();
            self.streamed.push_str(text);
        }
    }

    pub fn finish(&mut self, text: &str) {
        if let Some(tail) = text.strip_prefix(&self.streamed) {
            if !text.is_empty() {
                println!("{}", crate::safe(tail));
            }
        } else {
            println!();
            if !text.is_empty() {
                println!("{}", crate::safe(text));
            }
        }
        self.streamed.clear();
    }

    pub fn result(
        &mut self,
        text: &str,
        urls: &[String],
        details: Option<&ToolDetails>,
    ) -> io::Result<()> {
        if urls.is_empty() {
            println!(
                "{}",
                crate::safe(&crate::active::result_text(text, details))
            );
            return Ok(());
        }
        if let Some(ToolDetails::Codemode {
            elapsed_ms,
            success,
            ..
        }) = details
        {
            let status = if *success { "completed" } else { "failed" };
            println!("Codemode {status} in {elapsed_ms} ms");
        }
        for part in panda_core::parts(text, urls, details) {
            match part {
                Part::Text(s) => println!("{}", crate::safe(s)),
                Part::Image(url) => self.image(url, &mut io::stdout().lock())?,
            }
        }
        Ok(())
    }

    pub fn images(&mut self, urls: &[String]) -> io::Result<()> {
        for url in urls {
            self.image(url, &mut io::stdout().lock())?;
        }
        Ok(())
    }

    fn image(&mut self, url: &str, out: &mut impl Write) -> io::Result<()> {
        let Some((data, (width, height))) = url
            .strip_prefix("data:image/png;base64,")
            .filter(|s| {
                self.graphics != Graphics::Text && self.images_left > 0 && s.len() <= self.remaining
            })
            .and_then(|s| png_size(s).map(|size| (s, size)))
        else {
            return writeln!(out, "[image · preview unavailable]");
        };
        self.remaining -= data.len();
        self.images_left -= 1;
        let window =
            crossterm::terminal::window_size().unwrap_or(crossterm::terminal::WindowSize {
                columns: 80,
                rows: 24,
                width: 0,
                height: 0,
            });
        let cell = |pixels: u16, cells: u16, fallback: f64| {
            if pixels == 0 || cells == 0 {
                fallback
            } else {
                f64::from(pixels) / f64::from(cells)
            }
        };
        let natural_cols = f64::from(width) / cell(window.width, window.columns, 8.0);
        let natural_rows = f64::from(height) / cell(window.height, window.rows, 16.0);
        let scale = (f64::from(window.columns.clamp(1, 40)) / natural_cols)
            .min(f64::from(window.rows.clamp(1, 12)) / natural_rows)
            .min(1.0);
        let cols = (natural_cols * scale).round().max(1.0) as u16;
        let rows = (natural_rows * scale).round().max(1.0) as u16;
        match self.graphics {
            Graphics::Kitty => {
                let mut chunks = data.as_bytes().chunks(4096).peekable();
                let mut first = true;
                while let Some(chunk) = chunks.next() {
                    write!(out, "\x1b_G")?;
                    if first {
                        write!(out, "a=T,f=100,q=2,c={cols},r={rows},")?;
                        first = false;
                    }
                    write!(out, "m={};", usize::from(chunks.peek().is_some()))?;
                    out.write_all(chunk)?;
                    out.write_all(b"\x1b\\")?;
                }
            }
            Graphics::Iterm => write!(
                out,
                "\x1b]1337;File=inline=1;width={cols};height={rows}:{data}\x07"
            )?,
            Graphics::Text => unreachable!(),
        }
        writeln!(out)
    }
}

fn png_size(data: &str) -> Option<(u32, u32)> {
    let png = STANDARD.decode(data).ok()?;
    if !png.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR")
        || !png.ends_with(b"\0\0\0\0IEND\xae\x42\x60\x82")
    {
        return None;
    }
    let width = u32::from_be_bytes(png.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(png.get(20..24)?.try_into().ok()?);
    ((1..=4096).contains(&width) && (1..=4096).contains(&height)).then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_limits_apply_across_a_turn_and_reset_for_replay() {
        let pixel = include_bytes!("../tests/images/pixel.png");
        let data = STANDARD.encode(pixel);
        let url = format!("data:image/png;base64,{data}");
        let mut scroll = Scrollback::detect();
        scroll.graphics = Graphics::Kitty;
        let mut output = Vec::new();
        for _ in 0..=MAX_IMAGES {
            scroll.image(&url, &mut output).unwrap();
        }
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.matches(&data).count(), MAX_IMAGES);
        assert_eq!(text.matches("preview unavailable").count(), 1);
        scroll.reset();
        scroll.remaining = data.len() - 1;
        let mut output = Vec::new();
        scroll.image(&url, &mut output).unwrap();
        assert!(!output.contains(&0x1b));

        let mut large = pixel.to_vec();
        large[16..20].copy_from_slice(&4097u32.to_be_bytes());
        scroll.reset();
        for invalid in [
            STANDARD.encode(large),
            format!("{data}\x1b"),
            STANDARD.encode(b"text"),
        ] {
            scroll
                .image(&format!("data:image/png;base64,{invalid}"), &mut output)
                .unwrap();
        }
        assert!(!output.contains(&0x1b));
        assert_eq!(scroll.images_left, MAX_IMAGES);
    }
}
