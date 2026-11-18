//! Native subscription authentication and provider-specific streaming.
use crate::{auth, wire};
use anyhow::{Result, bail, ensure};
use panda_core::*;
use reqwest::Client;
use serde_json::Value;
use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

pub struct Subscription {
    pub provider: String,
    pub model: String,
    pub state: PathBuf,
}
fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent("Panda/0.1")
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .build()?)
}
fn prompt(label: &str) -> Result<String> {
    eprint!("{label} ");
    io::stderr().flush()?;
    let mut s = String::new();
    ensure!(io::stdin().read_line(&mut s)? > 0, "login cancelled");
    Ok(s.trim().to_owned())
}

impl Subscription {
    pub fn login(&self) -> Result<()> {
        self.login_with_cancel(&Cancel::default())
    }
    pub fn login_with_cancel(&self, cancel: &Cancel) -> Result<()> {
        let browser =
            match prompt("Codex login method [browser/device_code] (default browser):")?.as_str() {
                "" | "browser" => true,
                "device_code" => false,
                "cancel" | "/cancel" => bail!("login cancelled"),
                _ => bail!("unknown login method"),
            };
        self.login_with_method(cancel, browser)
    }
    pub fn login_with_method(&self, cancel: &Cancel, browser: bool) -> Result<()> {
        let client = client()?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        ensure!(
            self.provider == auth::CODEX,
            "unknown subscription provider"
        );
        let token = rt.block_on(cancellable(cancel, async {
            if browser {
                auth::browser_login(&client).await
            } else {
                auth::device_exchange(&client, "https://auth.openai.com", |url, code| {
                    eprintln!("Visit {url} and enter {code}")
                })
                .await
            }
        }))?;
        auth::store_login(&self.state, &self.provider, &token)
    }
}
async fn cancellable<T>(
    cancel: &Cancel,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    cancel.check()?;
    tokio::pin!(work);
    let mut tick = tokio::time::interval(Duration::from_millis(40));
    loop {
        tokio::select! {r=&mut work=>return r,_=tick.tick()=>cancel.check()?}
    }
}
impl Provider for Subscription {
    fn reply(
        &mut self,
        system: &str,
        h: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        self.reply_with_effort(system, h, tools, cancel, emit, None)
    }
    fn reply_with_effort(
        &mut self,
        system: &str,
        h: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
        effort: Option<&str>,
    ) -> Result<(String, Vec<Call>)> {
        cancel.check()?;
        let client = client()?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(cancellable(cancel, async {
            ensure!(
                self.provider == auth::CODEX,
                "unknown subscription provider"
            );
            let cred = auth::credential(&self.state, auth::CODEX, &client).await?;
            let mut body = wire::responses_body(&self.model, system, h, tools);
            if let Some(e) = effort {
                body["reasoning"] = serde_json::json!({"effort":e,"summary":"auto"});
            }
            let response = client
                .post("https://chatgpt.com/backend-api/codex/responses")
                .bearer_auth(wire::string(&cred, "access")?)
                .header("chatgpt-account-id", wire::string(&cred, "accountId")?)
                .header("OpenAI-Beta", "responses=experimental")
                .header("originator", "panda")
                .header("Accept", "text/event-stream")
                .json(&body)
                .send()
                .await
                .map_err(crate::transport_error)?;
            let mut response = crate::checked(response).await?;
            let mut stream = wire::Responses::default();
            while let Some(bytes) = response.chunk().await.map_err(crate::transport_error)? {
                stream.push(&bytes, emit)?
            }
            stream.finish()
        }))
    }
}
