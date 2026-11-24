//! Single-shot, cancellable inference against a host-selected compatible endpoint.
use anyhow::{Result, ensure};
use panda_core::Cancel;
use serde_json::Value;

pub fn request(base: &str, key: &str, task: &str, body: &Value, cancel: &Cancel) -> Result<Value> {
    let path = match task {
        "classify" => "chat/completions",
        "generate_image" => "images/generations",
        _ => anyhow::bail!("unsupported inference task"),
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut req = client
        .post(format!("{}/{path}", base.trim_end_matches('/')))
        .json(body);
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(super::subscription::cancellable(cancel, async {
            let mut response =
                super::checked(req.send().await.map_err(super::transport_error)?).await?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    bytes.len() + chunk.len() <= 12 * 1024 * 1024,
                    "inference response exceeds 12 MiB"
                );
                bytes.extend_from_slice(&chunk);
            }
            Ok(serde_json::from_slice(&bytes)?)
        }))
}
