//! API-key streaming endpoints. Codex subscriptions keep their own OAuth transport.
mod history;
mod pi;
mod stream;
use crate::{Api, checked, subscription::cancellable, transport_error, wire};
use anyhow::{Result, bail, ensure};
use panda_core::{Call, Cancel, Emit, Event, Message, Provider, ProviderReplay};
use reqwest::Url;
use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Anthropic,
    Responses,
    Gemini,
    Azure,
    Vertex,
    Pi,
}
impl Family {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "anthropic-messages" => Ok(Self::Anthropic),
            "openai-responses" => Ok(Self::Responses),
            "google-generative-ai" => Ok(Self::Gemini),
            "azure-openai-responses" => Ok(Self::Azure),
            "google-vertex" => Ok(Self::Vertex),
            "pi-messages" => Ok(Self::Pi),
            _ => bail!("unknown native API family {name}"),
        }
    }
    fn wire(self) -> Self {
        match self {
            Self::Azure => Self::Responses,
            Self::Vertex => Self::Gemini,
            other => other,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic-messages",
            Self::Responses => "openai-responses",
            Self::Gemini => "google-generative-ai",
            Self::Azure => "azure-openai-responses",
            Self::Vertex => "google-vertex",
            Self::Pi => "pi-messages",
        }
    }
}

pub fn builtin(id: &str) -> Option<(Family, &'static str)> {
    match id {
        "anthropic" => Some((Family::Anthropic, "https://api.anthropic.com/v1")),
        "openai-api" => Some((Family::Responses, "https://api.openai.com/v1")),
        "google" => Some((
            Family::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
        )),
        _ => None,
    }
}

pub struct NativeApi {
    pub family: Family,
    pub provider: String,
    pub api: Api,
    pub max_tokens: u64,
    pub images: bool,
}
impl NativeApi {
    fn url(&self) -> Result<Url> {
        let mut url = Url::parse(&self.api.endpoint)?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("invalid provider URL"))?;
            segments.pop_if_empty();
            match self.family {
                Family::Anthropic | Family::Pi => {
                    segments.push("messages");
                }
                Family::Responses | Family::Azure => {
                    segments.push("responses");
                }
                Family::Gemini | Family::Vertex => {
                    let model = self
                        .api
                        .model
                        .strip_prefix("models/")
                        .unwrap_or(&self.api.model);
                    segments
                        .push("models")
                        .push(&format!("{model}:streamGenerateContent"));
                }
            }
        }
        if self.family.wire() == Family::Gemini {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
        Ok(url)
    }
    fn replay(&self, blocks: Vec<Value>, emit: &mut Emit) {
        if !blocks.is_empty() {
            emit(Event::Replay(ProviderReplay {
                provider: self.provider.clone(),
                model: self.api.model.clone(),
                family: self.family.wire().name().into(),
                blocks,
            }));
        }
    }
}
impl Provider for NativeApi {
    fn reply(
        &mut self,
        s: &str,
        h: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
    ) -> Result<(String, Vec<Call>)> {
        self.reply_with_effort(s, h, tools, cancel, emit, None)
    }
    fn reply_with_effort(
        &mut self,
        s: &str,
        h: &[Message],
        tools: &[Value],
        cancel: &Cancel,
        emit: &mut Emit,
        effort: Option<&str>,
    ) -> Result<(String, Vec<Call>)> {
        cancel.check()?;
        let body = if self.family == Family::Pi {
            pi::body(self, s, h, tools, effort)?
        } else {
            history::body(self, s, h, tools, effort)?
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut request = client
            .post(self.url()?)
            .header("accept", "text/event-stream");
        if self.family == Family::Anthropic {
            request = request.header("anthropic-version", "2023-06-01");
        }
        if !self.api.key.is_empty() {
            request = match self.family {
                Family::Anthropic => request.header("x-api-key", &self.api.key),
                Family::Azure => request.header("api-key", &self.api.key),
                Family::Responses | Family::Vertex | Family::Pi => {
                    request.bearer_auth(&self.api.key)
                }
                Family::Gemini => request.header("x-goog-api-key", &self.api.key),
            };
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(cancellable(cancel, async {
            let mut response =
                checked(request.json(&body).send().await.map_err(transport_error)?).await?;
            if self.family.wire() == Family::Responses {
                let mut decoder = wire::Responses::default();
                while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                    decoder.push(&chunk, emit)?;
                }
                let replay = std::mem::take(&mut decoder.replay);
                let result = decoder.finish()?;
                self.replay(replay, emit);
                return Ok(result);
            }
            let mut lines = wire::Lines::default();
            let mut decoder = stream::Decoder::new(self.family.wire(), h.len(), self.max_tokens);
            while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                lines.feed(&chunk, |v| decoder.push(v, emit))?;
            }
            ensure!(lines.partial.is_empty(), "incomplete provider event");
            let (text, calls, replay) = decoder.finish(emit)?;
            self.replay(replay, emit);
            Ok((text, calls))
        }))
    }
}
