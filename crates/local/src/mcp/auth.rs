//! OAuth authorization-code login for a registered public MCP client.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, Url, header};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{ErrorKind, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct OAuth {
    client_id: String,
    pub(super) scope: Option<String>,
}

#[derive(Clone)]
pub(super) struct Credentials {
    file: PathBuf,
    resource: Url,
    client_id: String,
}
#[derive(Serialize, Deserialize)]
struct Grant {
    resource: String,
    client_id: String,
    access: String,
    expires: Option<u64>,
}
fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
fn endpoint(raw: &str, allow_loopback: bool) -> Result<Url> {
    let url = Url::parse(raw).context("invalid OAuth endpoint")?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && (url.scheme() == "https" || (allow_loopback && url.scheme() == "http" && loopback)),
        "OAuth requires HTTPS or a loopback HTTP endpoint"
    );
    Ok(url)
}
impl Credentials {
    pub fn new(
        state: &Path,
        name: &str,
        spec: &super::ServerSpec,
        project: bool,
    ) -> Result<Option<Self>> {
        let Some(auth) = &spec.oauth else {
            return Ok(None);
        };
        ensure!(
            !project && spec.command.is_none(),
            "OAuth requires a personal HTTP MCP server"
        );
        ensure!(!auth.client_id.trim().is_empty(), "missing OAuth client ID");
        let resource = endpoint(
            spec.url.as_deref().context("missing MCP resource URL")?,
            true,
        )?;
        // Use a fixed, case-insensitive-filesystem-safe key for each server.
        let key: String = digest::digest(&digest::SHA256, name.as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Some(Self {
            file: state.join(format!("mcp-auth-{key}.json")),
            resource,
            client_id: auth.client_id.clone(),
        }))
    }

    pub fn token(&self) -> Result<Option<header::HeaderValue>> {
        let Some(raw) = crate::resources::optional_text(&self.file)? else {
            return Ok(None);
        };
        let grant: Grant =
            serde_json::from_str(&raw).map_err(|_| anyhow::anyhow!("invalid saved MCP login"))?;
        let now = now()?;
        if grant.resource != self.resource.as_str()
            || grant.client_id != self.client_id
            || grant.expires.is_some_and(|t| t <= now)
        {
            return Ok(None);
        }
        let mut value = header::HeaderValue::from_str(&format!("Bearer {}", grant.access))
            .context("invalid saved MCP token")?;
        value.set_sensitive(true);
        Ok(Some(value))
    }
    pub fn logout(&self) -> Result<()> {
        match std::fs::remove_file(&self.file) {
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            result => Ok(result?),
        }
    }
    async fn discover(&self, client: &Client) -> Result<(Url, Url)> {
        let challenge = client
            .post(self.resource.clone())
            .header("Accept", "application/json, text/event-stream")
            .json(&super::initialize_request(1))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("MCP authorization discovery failed"))?;
        ensure!(
            challenge.status() == reqwest::StatusCode::UNAUTHORIZED,
            "expected MCP authorization challenge"
        );
        let raw = challenge
            .headers()
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(metadata_uri)
            .context("missing Bearer resource_metadata challenge")?;
        let allow_loopback = self.resource.scheme() == "http";
        let metadata = fetch_json(client.get(endpoint(raw, allow_loopback)?)).await?;
        ensure!(
            metadata["resource"].as_str() == Some(self.resource.as_str()),
            "OAuth resource metadata mismatch"
        );
        let issuer_raw = metadata["authorization_servers"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .context("missing OAuth issuer")?;
        let issuer = endpoint(issuer_raw, allow_loopback)?;
        ensure!(issuer.query().is_none(), "invalid OAuth issuer");
        let mut discovery = issuer.clone();
        discovery.set_path(&format!(
            "/.well-known/oauth-authorization-server{}",
            issuer.path().trim_end_matches('/')
        ));
        let metadata = fetch_json(client.get(discovery)).await?;
        ensure!(
            metadata["issuer"] == issuer_raw,
            "OAuth issuer metadata mismatch"
        );
        let url = |key: &str| {
            endpoint(
                metadata[key].as_str().context("missing OAuth endpoint")?,
                allow_loopback,
            )
        };
        let authorize = url("authorization_endpoint")?;
        let token = url("token_endpoint")?;
        Ok((authorize, token))
    }
    pub async fn login(&self, scope: Option<&str>, show: &mut dyn FnMut(&str)) -> Result<()> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()?;
        let (mut authorize, token) = self.discover(&client).await?;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let redirect = format!("http://{}/callback", listener.local_addr()?);
        let mut random = [0u8; 64];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| anyhow::anyhow!("secure random generator unavailable"))?;
        let state = URL_SAFE_NO_PAD.encode(&random[..32]);
        let verifier = URL_SAFE_NO_PAD.encode(&random[32..]);
        let challenge =
            URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
        authorize.query_pairs_mut().extend_pairs([
            ("client_id", self.client_id.as_str()),
            ("response_type", "code"),
            ("resource", self.resource.as_str()),
            ("redirect_uri", &redirect),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ]);
        if let Some(scope) = scope {
            authorize.query_pairs_mut().append_pair("scope", scope);
        }
        show(&format!("Open {authorize}"));
        let code = tokio::time::timeout(Duration::from_secs(300), callback(listener, &state))
            .await
            .context("MCP login expired")??;
        let grant = fetch_json(client.post(token).form(&[
            ("client_id", self.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("resource", self.resource.as_str()),
        ]))
        .await?;
        ensure!(
            grant["token_type"]
                .as_str()
                .is_some_and(|t| t.eq_ignore_ascii_case("bearer")),
            "expected OAuth bearer token"
        );
        let access = grant["access_token"]
            .as_str()
            .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_graphic()))
            .context("invalid OAuth access token")?;
        let now = now()?;
        let saved = Grant {
            resource: self.resource.to_string(),
            client_id: self.client_id.clone(),
            access: access.into(),
            expires: grant["expires_in"].as_u64().map(|t| now.saturating_add(t)),
        };
        let mut file = tempfile::NamedTempFile::new_in(
            self.file.parent().context("missing state directory")?,
        )?;
        serde_json::to_writer(&mut file, &saved)?;
        file.flush()?;
        file.persist(&self.file)?;
        Ok(())
    }
}
fn metadata_uri(header: &str) -> Option<&str> {
    let (scheme, params) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    params.split(',').find_map(|p| {
        let (name, value) = p.trim().split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("resource_metadata")
            .then(|| value.trim().trim_matches('"'))
    })
}
async fn fetch_json(request: reqwest::RequestBuilder) -> Result<Value> {
    let mut res = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("OAuth HTTP request failed"))?;
    ensure!(
        res.status().is_success(),
        "OAuth HTTP status {}",
        res.status().as_u16()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("OAuth response interrupted"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= 65536,
            "OAuth response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid OAuth JSON response"))
}
async fn callback(listener: tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(3),
            BufReader::new((&mut socket).take(8192)).read_line(&mut line),
        )
        .await;
        let parsed = line
            .strip_prefix("GET ")
            .and_then(|s| s.split_once(' '))
            .and_then(|(target, _)| target.strip_prefix("/callback?"))
            .and_then(|q| Url::parse(&format!("http://127.0.0.1/callback?{q}")).ok());
        let mut code = None;
        let mut denied = false;
        if matches!(read, Ok(Ok(_)))
            && line.ends_with("\r\n")
            && let Some(url) = parsed
        {
            let values = |key: &str| {
                url.query_pairs()
                    .filter(|(k, _)| k == key)
                    .map(|(_, v)| v.into_owned())
                    .collect::<Vec<_>>()
            };
            if values("state") == [state] {
                let codes = values("code");
                if codes.len() == 1 && !codes[0].is_empty() {
                    code = codes.into_iter().next();
                }
                denied = !values("error").is_empty();
            }
        }
        let status = if code.is_some() {
            "200 OK"
        } else {
            "400 Bad Request"
        };
        let _ = socket
            .write_all(
                format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\nReturn to Panda.")
                    .as_bytes(),
            )
            .await;
        ensure!(!denied, "MCP login declined");
        if let Some(code) = code {
            return Ok(code);
        }
    }
}
