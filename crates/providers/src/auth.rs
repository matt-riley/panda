use crate::wire::string;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, RequestBuilder, Url};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const CODEX: &str = "openai-codex";
const OPENAI_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
fn database(state: &Path) -> Result<Connection> {
    let db = Connection::open(state.join("subscriptions.sqlite3"))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS credentials(id TEXT PRIMARY KEY,value TEXT NOT NULL)",
    )?;
    Ok(db)
}
fn save(db: &Connection, id: &str, value: &Value) -> Result<()> {
    db.execute(
        "INSERT INTO credentials VALUES (?,?) ON CONFLICT(id) DO UPDATE SET value=excluded.value",
        params![id, value.to_string()],
    )?;
    Ok(())
}
pub(crate) async fn credential(state: &Path, id: &str, client: &Client) -> Result<Value> {
    let mut db = database(state)?;
    // Serialize refreshes across Panda sessions so a rotated token cannot be lost.
    db.busy_timeout(Duration::ZERO)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    let tx = loop {
        match db.transaction_with_behavior(TransactionBehavior::Immediate) {
            Ok(tx) => break tx,
            Err(e)
                if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(e) => return Err(e.into()),
        }
    };
    let raw: String = tx
        .query_row("SELECT value FROM credentials WHERE id=?", [id], |r| {
            r.get(0)
        })
        .optional()?
        .with_context(|| format!("Sign in with /login {id}"))?;
    let mut v: Value = serde_json::from_str(&raw)?;
    if v["expires"].as_u64().unwrap_or(0) <= now()? + 30_000 {
        let refresh = string(&v, "refresh")?;
        v = {
            let next = json_response(client.post("https://auth.openai.com/oauth/token").form(&[
                ("client_id", OPENAI_CLIENT),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
            ]))
            .await?;
            codex_token(&next, Some(refresh))?
        };
        save(&tx, id, &v)?;
    }
    tx.commit()?;
    Ok(v)
}
async fn json_response(req: RequestBuilder) -> Result<Value> {
    Ok(req
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}
fn codex_token(v: &Value, old: Option<&str>) -> Result<Value> {
    let access = string(v, "access_token")?;
    let payload = access.split('.').nth(1).context("missing account claims")?;
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))?)?;
    let account = string(&claims["https://api.openai.com/auth"], "chatgpt_account_id")?;
    let seconds = v["expires_in"].as_u64().context("missing expiration")?;
    Ok(
        json!({"type":"oauth","access":access,"refresh":v["refresh_token"].as_str().or(old).context("missing refresh token")?,"expires":now()?.saturating_add(seconds.saturating_mul(1000)),"accountId":account}),
    )
}
pub(crate) async fn device_exchange(
    client: &Client,
    issuer: &str,
    notify: impl Fn(&str, &str),
) -> Result<Value> {
    let start = client
        .post(format!("{issuer}/api/accounts/deviceauth/usercode"))
        .json(&json!({"client_id":OPENAI_CLIENT}));
    let device = json_response(start.header("Accept", "application/json")).await?;
    let code = string(&device, "user_code")?;
    let uri = "https://auth.openai.com/codex/device";
    ensure!(
        Url::parse(uri)?.scheme() == "https"
            && !uri.chars().any(char::is_control)
            && !code.chars().any(char::is_control),
        "invalid verification prompt"
    );
    notify(uri, code);
    let mut interval = device["interval"]
        .as_u64()
        .or_else(|| device["interval"].as_str()?.parse().ok())
        .unwrap_or(5)
        .clamp(1, 900);
    let timeout = Duration::from_secs(device["expires_in"].as_u64().unwrap_or(900).min(900));
    let poll = async {
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            let response = client
                .post(format!("{issuer}/api/accounts/deviceauth/token"))
                .json(
                    &json!({"device_auth_id":string(&device,"device_auth_id")?, "user_code":code}),
                )
                .send()
                .await?;
            let status = response.status();
            if matches!(status.as_u16(), 403 | 404) {
                continue;
            }
            if status.as_u16() == 429 {
                interval = (interval + 5).min(900);
                continue;
            }
            let token: Value = response.json().await?;
            if !status.is_success() {
                match token
                    .pointer("/error/code")
                    .or_else(|| token.get("error"))
                    .and_then(Value::as_str)
                {
                    Some("deviceauth_authorization_pending") => continue,
                    Some("slow_down") => {
                        interval = (interval + 5).min(900);
                        continue;
                    }
                    _ => bail!("Codex device authorization failed ({status})"),
                }
            }
            let response = json_response(client.post(format!("{issuer}/oauth/token")).form(&[
                ("client_id", OPENAI_CLIENT),
                ("grant_type", "authorization_code"),
                ("code", string(&token, "authorization_code")?),
                ("code_verifier", string(&token, "code_verifier")?),
                (
                    "redirect_uri",
                    "https://auth.openai.com/deviceauth/callback",
                ),
            ]))
            .await?;
            return codex_token(&response, None);
        }
    };
    tokio::time::timeout(timeout, poll)
        .await
        .context("device authorization expired")?
}
pub(crate) fn store_login(state: &Path, id: &str, v: &Value) -> Result<()> {
    save(&database(state)?, id, v)
}

fn callback_code(target: &str, state: &str) -> Result<String> {
    let url = Url::parse(&format!("http://localhost:1455{target}"))?;
    ensure!(url.path() == "/auth/callback", "unexpected callback path");
    let mut code = None;
    let mut returned_state = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "state" => ensure!(
                returned_state.replace(v.into_owned()).is_none(),
                "duplicate state"
            ),
            "code" => ensure!(code.replace(v.into_owned()).is_none(), "duplicate code"),
            _ => {}
        }
    }
    ensure!(
        returned_state.as_deref() == Some(state),
        "login state mismatch"
    );
    code.filter(|s| !s.is_empty())
        .context("login declined or missing code")
}
pub(crate) async fn browser_login(client: &Client) -> Result<Value> {
    use ring::{
        digest,
        rand::{SecureRandom, SystemRandom},
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let mut nonce = [0u8; 64];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("random generator unavailable"))?;
    let verifier = URL_SAFE_NO_PAD.encode(&nonce[..32]);
    let state = URL_SAFE_NO_PAD.encode(&nonce[32..]);
    let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
    let redirect = "http://localhost:1455/auth/callback";
    let mut url = Url::parse("https://auth.openai.com/oauth/authorize")?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", OPENAI_CLIENT),
        ("redirect_uri", redirect),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("originator", "panda"),
        ("codex_cli_simplified_flow", "true"),
        ("id_token_add_organizations", "true"),
    ]);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 1455))
        .await
        .context("could not bind the login callback; choose device_code instead")?;
    eprintln!("Open {url}");
    let code = tokio::time::timeout(Duration::from_secs(900), async {
        loop {
            let (mut socket, _) = listener.accept().await?;
            let mut line = String::new();
            let read = tokio::time::timeout(
                Duration::from_secs(3),
                BufReader::new((&mut socket).take(8192)).read_line(&mut line),
            )
            .await;
            if !matches!(read, Ok(Ok(_))) || !line.ends_with("\r\n") {
                continue;
            }
            let mut parts = line.split_whitespace();
            let code = if parts.next() == Some("GET") {
                parts
                    .next()
                    .and_then(|target| callback_code(target, &state).ok())
            } else {
                None
            };
            let reply = if code.is_some() {
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nReturn to Panda."
            } else {
                "HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\nInvalid callback."
            };
            let _ = socket.write_all(reply.as_bytes()).await;
            if let Some(c) = code {
                return Ok::<_, anyhow::Error>(c);
            }
        }
    })
    .await
    .context("browser authorization expired")??;
    let response = json_response(client.post("https://auth.openai.com/oauth/token").form(&[
        ("client_id", OPENAI_CLIENT),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect),
        ("code", &code),
        ("code_verifier", &verifier),
    ]))
    .await?;
    codex_token(&response, None)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_callback_is_bound_to_login_state() {
        assert_eq!(
            callback_code("/auth/callback?state=a&code=hello%2Bworld", "a").unwrap(),
            "hello+world"
        );
        for path in [
            "/?state=a&code=x",
            "/auth/callback?state=b&code=x",
            "/auth/callback?code=x",
            "/auth/callback?state=a&code=x&code=y",
        ] {
            assert!(callback_code(path, "a").is_err())
        }
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };
    #[test]
    fn fresh_device_logins_persist_and_reopen_without_refresh() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let jwt = format!(
            "fake.{}.test",
            URL_SAFE_NO_PAD
                .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account"}}"#)
        );
        let routes = vec![
            (
                "/api/accounts/deviceauth/usercode",
                json!({"device_auth_id":"id","user_code":"TEST","interval":0}),
            ),
            (
                "/api/accounts/deviceauth/token",
                json!({"authorization_code":"code","code_verifier":"verifier"}),
            ),
            (
                "/oauth/token",
                json!({"access_token":jwt,"refresh_token":"refresh","expires_in":3600}),
            ),
        ];
        let server = thread::spawn(move || {
            for (path, body) in routes {
                let (mut sock, _) = listener.accept().unwrap();
                let mut request = String::new();
                let mut reader = BufReader::new(&sock);
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                assert!(request.lines().next().unwrap().contains(path));
                let body = body.to_string();
                write!(
                    sock,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let state = tempfile::tempdir().unwrap();
        let client = Client::new();
        let id = CODEX;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let token = device_exchange(&client, &issuer, |url, code| {
                assert!(url.starts_with("https://"));
                assert_eq!(code, "TEST")
            })
            .await
            .unwrap();
            assert_eq!(token["accountId"], "account");
            store_login(state.path(), id, &token).unwrap();
            assert_eq!(credential(state.path(), id, &client).await.unwrap(), token);
        });
        server.join().unwrap();
    }
}
