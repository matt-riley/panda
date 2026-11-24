use super::*;
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, client::IntoClientRequest},
};

#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Sse,
    Websocket,
    Auto,
}

pub(super) async fn request(
    api: &NativeApi,
    body: &Value,
    cancel: &Cancel,
    emit: &mut Emit<'_>,
) -> Result<Option<(String, Vec<Call>)>> {
    let mut url = api.url()?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid WebSocket endpoint"))?;
    let mut request = url.as_str().into_client_request()?;
    if !api.api.key.is_empty() {
        let (name, value) = if api.family == Family::Azure {
            ("api-key", api.api.key.clone())
        } else {
            ("authorization", format!("Bearer {}", api.api.key))
        };
        request.headers_mut().insert(name, value.parse()?);
    }
    let (mut socket, _) = match connect_async(request).await {
        Ok(connection) => connection,
        Err(tungstenite::Error::Http(r))
            if api.transport == Transport::Auto
                && matches!(r.status().as_u16(), 404 | 405 | 426 | 501) =>
        {
            return Ok(None);
        }
        Err(tungstenite::Error::Http(r)) => bail!("WebSocket handshake failed ({})", r.status()),
        Err(error) => return Err(error).context("could not establish provider WebSocket"),
    };
    let mut create = body.clone();
    let fields = create
        .as_object_mut()
        .context("invalid Responses request")?;
    fields.remove("stream");
    fields.insert("type".into(), json!("response.create"));
    // A failed write may already have reached the provider.
    cancel.mark_response_started();
    socket
        .send(tungstenite::Message::text(create.to_string()))
        .await
        .context("sending response.create")?;
    let mut decoder = wire::Responses::default();
    while !decoder.complete() {
        let message = socket
            .next()
            .await
            .context("incomplete WebSocket response")??;
        match message {
            tungstenite::Message::Text(text) => {
                decoder.event(serde_json::from_str(&text)?, emit)?
            }
            tungstenite::Message::Ping(_) => socket.flush().await?,
            tungstenite::Message::Pong(_) => (),
            _ => bail!("incomplete or invalid WebSocket response"),
        }
    }
    let replay = std::mem::take(&mut decoder.replay);
    let reply = decoder.finish()?;
    api.replay(replay, emit);
    Ok(Some(reply))
}
