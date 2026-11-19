use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, write::EncoderWriter};
use panda_local::store::Disk;
use std::io::Write;

pub fn export(disk: &mut Disk, mut out: impl Write) -> Result<()> {
    let (before, after) = include_str!("html/viewer.html")
        .split_once("{{SESSION}}")
        .expect("embedded viewer must have a session slot");
    out.write_all(before.as_bytes())?;
    // Stream the same archive as JSONL. Base64 keeps arbitrary transcript
    // bytes out of the HTML parser, including closing script tags.
    let mut encoded = EncoderWriter::new(&mut out, &STANDARD);
    disk.export_pi(&mut encoded)?;
    let out = encoded.finish()?;
    out.write_all(after.as_bytes())?;
    out.flush()?;
    Ok(())
}
