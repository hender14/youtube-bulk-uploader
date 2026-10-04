use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use url::Url;

type HmacSha256 = Hmac<Sha256>;
const SESSION_COOKIE: &str = "yt_session";
const OAUTH_COOKIE: &str = "yt_oauth";
const SESSION_SECONDS: u64 = 86_400;

struct Config {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    allowed_emails: Vec<String>,
    secret_resource: Option<String>,
    token_file: Option<PathBuf>,
    cookie_secure: bool,
}

impl Config {
    fn from_env() -> Result<Self> {
        let client_config: Value =
            if let Ok(resource) = std::env::var("OAUTH_CLIENT_CONFIG_RESOURCE") {
                serde_json::from_slice(&read_secret_resource(&resource)?)
                    .context("OAuth client secret must be JSON")?
            } else {
                json!({
                    "client_id": required_env("OAUTH_CLIENT_ID")?,
                    "client_secret": required_env("OAUTH_CLIENT_SECRET")?,
                })
            };
        let client_id = client_config["client_id"]
            .as_str()
            .context("OAuth client ID is missing")?
            .to_owned();
        let client_secret = client_config["client_secret"]
            .as_str()
            .context("OAuth client secret is missing")?
            .to_owned();
        let redirect_uri = required_env("OAUTH_REDIRECT_URI")?;
        let parsed_redirect = Url::parse(&redirect_uri).context("Invalid OAuth redirect URL")?;
        ensure!(
            parsed_redirect.scheme() == "https" || parsed_redirect.host_str() == Some("localhost"),
            "OAuth redirect URL must use HTTPS except on localhost"
        );
        let allowed_emails: Vec<String> = required_env("OAUTH_ALLOWED_EMAILS")?
            .split(',')
            .map(|email| email.trim().to_ascii_lowercase())
            .filter(|email| !email.is_empty())
            .collect();
        ensure!(
            allowed_emails.len() == 1,
            "Configure exactly one Google account email"
        );
        let cookie_secure = match std::env::var("COOKIE_SECURE").as_deref() {
            Ok("true") | Err(_) => true,
            Ok("false") => false,
            _ => anyhow::bail!("COOKIE_SECURE must be true or false"),
        };
        Ok(Self {
            client_id,
            client_secret,
            redirect_uri,
            allowed_emails,
            secret_resource: std::env::var("OAUTH_SECRET_RESOURCE").ok(),
            token_file: std::env::var_os("OAUTH_TOKEN_FILE").map(PathBuf::from),
            cookie_secure,
        })
    }

    fn oauth_storage_configured(&self) -> bool {
        self.secret_resource.is_some() || self.token_file.is_some()
    }
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    ensure!(!value.trim().is_empty(), "{name} cannot be empty");
    Ok(value)
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn oauth_url(config: &Config, state: &str, verifier: &str, nonce: &str) -> Result<String> {
    let mut url = Url::parse("https://accounts.google.com/o/oauth2/v2/auth")?;
    url.query_pairs_mut()
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &config.redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("response_mode", "form_post")
        .append_pair("scope", "openid email https://www.googleapis.com/auth/youtube.upload https://www.googleapis.com/auth/youtube.force-ssl")
        .append_pair("access_type", "offline")
        .append_pair("prompt", "select_account")
        .append_pair("state", state)
        .append_pair("nonce", nonce)
        .append_pair("code_challenge", &pkce_challenge(verifier))
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
}

fn header(name: &str, value: &str) -> Result<Header> {
    Header::from_bytes(name, value).map_err(|_| anyhow!("Invalid HTTP response header"))
}

fn send(
    request: Request,
    status: u16,
    body: String,
    content_type: &str,
    extra_headers: &[(&str, String)],
) -> Result<()> {
    let mut response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", content_type)?)
        .with_header(header("Cache-Control", "no-store")?);
    for (name, value) in extra_headers {
        response = response.with_header(header(name, value)?);
    }
    response = response
        .with_header(header("X-Content-Type-Options", "nosniff")?)
        .with_header(header("Referrer-Policy", "no-referrer")?)
        .with_header(header(
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        )?);
    request
        .respond(response)
        .context("Unable to send HTTP response")
}

fn cookie(name: &str, value: &str, age: u64, secure: bool) -> String {
    format!(
        "{name}={value}; Path=/; Max-Age={age}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

fn oauth_cookie(value: &str, age: u64) -> String {
    format!("{OAUTH_COOKIE}={value}; Path=/; Max-Age={age}; HttpOnly; SameSite=None; Secure")
}

fn request_cookie(request: &Request, name: &str) -> Option<String> {
    let cookies = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Cookie"))?
        .value
        .as_str();
    cookies.split(';').find_map(|item| {
        let (key, value) = item.trim().split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

fn session_value(config: &Config, email: &str, expires: u64) -> Result<String> {
    let payload = format!("{email}|{expires}|{}", random_token());
    let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let mut mac = HmacSha256::new_from_slice(config.client_secret.as_bytes())?;
    mac.update(format!("yt-uploader-session-v1:{encoded}").as_bytes());
    Ok(format!(
        "{encoded}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    ))
}

fn verify_session(config: &Config, value: &str) -> Option<String> {
    let (payload, signature) = value.split_once('.')?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let mut mac = HmacSha256::new_from_slice(config.client_secret.as_bytes()).ok()?;
    mac.update(format!("yt-uploader-session-v1:{payload}").as_bytes());
    mac.verify_slice(&signature).ok()?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let mut fields = std::str::from_utf8(&decoded).ok()?.split('|');
    let email = fields.next()?.to_ascii_lowercase();
    let expires = fields.next()?.parse::<u64>().ok()?;
    let nonce = fields.next()?;
    if fields.next().is_some() || nonce.is_empty() || expires <= now().ok()? {
        return None;
    }
    config.allowed_emails.contains(&email).then_some(email)
}

fn redirect(request: Request, location: &str, set_cookies: &[String]) -> Result<()> {
    let headers: Vec<(&str, String)> = std::iter::once(("Location", location.to_owned()))
        .chain(
            set_cookies
                .iter()
                .map(|value| ("Set-Cookie", value.clone())),
        )
        .collect();
    send(
        request,
        302,
        String::new(),
        "text/plain; charset=utf-8",
        &headers,
    )
}

fn start_oauth(request: Request, config: &Config) -> Result<()> {
    let state = random_token();
    let verifier = random_token();
    let nonce = random_token();
    let target = oauth_url(config, &state, &verifier, &nonce)?;
    let state_cookie = oauth_cookie(&format!("{state}.{verifier}.{nonce}"), 600);
    redirect(request, &target, &[state_cookie])
}

fn request_url(request: &Request) -> Result<Url> {
    Url::parse(&format!("http://localhost{}", request.url())).context("Invalid request URL")
}

fn authorization_code(config: &Config, code: &str, verifier: &str) -> Result<Value> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()?;
    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code),
            ("client_id", config.client_id.as_str()),
            ("client_secret", config.client_secret.as_str()),
            ("redirect_uri", config.redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ])
        .send()
        .context("OAuth token exchange failed")?;
    ensure!(
        response.status().is_success(),
        "OAuth token exchange rejected"
    );
    response.json().context("Invalid OAuth token response")
}

fn verified_email(id_token: &str, client_id: &str, nonce: &str) -> Result<String> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    let response = client
        .get("https://oauth2.googleapis.com/tokeninfo")
        .query(&[("id_token", id_token)])
        .send()
        .context("Google identity verification failed")?;
    ensure!(
        response.status().is_success(),
        "Google identity token rejected"
    );
    let claims: Value = response
        .json()
        .context("Invalid Google identity response")?;
    ensure!(
        claims["aud"].as_str() == Some(client_id),
        "OAuth audience mismatch"
    );
    ensure!(
        claims["nonce"].as_str() == Some(nonce),
        "OAuth nonce mismatch"
    );
    let email_verified = claims["email_verified"] == true || claims["email_verified"] == "true";
    ensure!(email_verified, "Google account email is not verified");
    let email = claims["email"]
        .as_str()
        .context("Google identity response has no email")?
        .to_ascii_lowercase();
    Ok(email)
}

fn metadata_access_token() -> Result<String> {
    let response = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?
        .get("http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token")
        .header("Metadata-Flavor", "Google")
        .send()
        .context("Google Cloud runtime identity is unavailable")?;
    ensure!(
        response.status().is_success(),
        "Google Cloud runtime identity rejected"
    );
    let token: Value = response
        .json()
        .context("Invalid runtime identity response")?;
    token["access_token"]
        .as_str()
        .map(str::to_owned)
        .context("Runtime identity token missing")
}

fn read_secret_resource(resource: &str) -> Result<Vec<u8>> {
    let access_token = metadata_access_token()?;
    let response = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?
        .get(format!(
            "https://secretmanager.googleapis.com/v1/{resource}/versions/latest:access"
        ))
        .bearer_auth(access_token)
        .send()
        .context("Unable to read Secret Manager configuration")?;
    ensure!(
        response.status().is_success(),
        "Secret Manager configuration is unavailable"
    );
    let secret: Value = response.json().context("Invalid Secret Manager response")?;
    STANDARD
        .decode(
            secret["payload"]["data"]
                .as_str()
                .context("Secret Manager payload missing")?,
        )
        .context("Secret Manager payload is not valid base64")
}

fn store_credentials(config: &Config, credentials: &[u8]) -> Result<()> {
    if let Some(path) = &config.token_file {
        let parent = path.parent().context("OAuth token file has no parent")?;
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut temporary, credentials)?;
        temporary.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        temporary.persist(path).map_err(|error| error.error)?;
        return Ok(());
    }
    let resource = config
        .secret_resource
        .as_deref()
        .context("Set OAUTH_SECRET_RESOURCE or OAUTH_TOKEN_FILE")?;
    let access_token = metadata_access_token()?;
    let response = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?
        .post(format!(
            "https://secretmanager.googleapis.com/v1/{resource}:addVersion"
        ))
        .bearer_auth(access_token)
        .json(&json!({"payload":{"data":STANDARD.encode(credentials)}}))
        .send()
        .context("Unable to persist OAuth credentials")?;
    ensure!(
        response.status().is_success(),
        "Secret Manager rejected OAuth credentials"
    );
    Ok(())
}

fn load_credentials(config: &Config) -> Result<Vec<u8>> {
    if let Some(path) = &config.token_file {
        return std::fs::read(path).context("Unable to read stored OAuth credentials");
    }
    let resource = config
        .secret_resource
        .as_deref()
        .context("Set OAUTH_SECRET_RESOURCE or OAUTH_TOKEN_FILE")?;
    read_secret_resource(resource).context("Unable to read stored OAuth credentials")
}

fn complete_oauth(mut request: Request, config: &Config) -> Result<()> {
    ensure!(
        config.oauth_storage_configured(),
        "OAuth credential storage is not configured"
    );
    ensure!(
        request
            .headers()
            .iter()
            .any(|header| header.field.equiv("Content-Type")
                && header
                    .value
                    .as_str()
                    .starts_with("application/x-www-form-urlencoded")),
        "OAuth callback must be form encoded"
    );
    let mut body = String::new();
    request.as_reader().take(16_385).read_to_string(&mut body)?;
    ensure!(body.len() <= 16_384, "OAuth callback is too large");
    let query: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
    ensure!(
        !query.contains_key("error"),
        "Google authorization was not completed"
    );
    let state = query.get("state").context("OAuth state missing")?;
    let code = query
        .get("code")
        .context("OAuth authorization code missing")?;
    let state_cookie =
        request_cookie(&request, OAUTH_COOKIE).context("OAuth browser state missing")?;
    let (saved_state, remaining) = state_cookie
        .split_once('.')
        .context("Invalid OAuth browser state")?;
    let (verifier, nonce) = remaining
        .split_once('.')
        .context("Invalid OAuth browser state")?;
    ensure!(saved_state == state, "OAuth state mismatch");
    let tokens = authorization_code(config, code, verifier)?;
    let id_token = tokens["id_token"]
        .as_str()
        .context("Google identity token missing")?;
    let email = verified_email(id_token, &config.client_id, nonce)?;
    ensure!(
        config.allowed_emails.contains(&email),
        "Google account is not allowlisted"
    );
    let refresh_token = if let Some(token) = tokens["refresh_token"].as_str() {
        token.to_owned()
    } else {
        let existing: Value = serde_json::from_slice(&load_credentials(config)?)?;
        ensure!(
            existing["email"].as_str() == Some(&email),
            "Google did not issue a refresh token for this account"
        );
        existing["refresh_token"]
            .as_str()
            .context("Stored Google refresh token missing")?
            .to_owned()
    };
    let stored = serde_json::to_vec(&json!({
        "client_id":config.client_id,
        "client_secret":config.client_secret,
        "refresh_token":refresh_token,
        "email":email,
    }))?;
    store_credentials(config, &stored)?;
    let session = session_value(config, &email, now()? + SESSION_SECONDS)?;
    let clear_oauth = oauth_cookie("deleted", 0);
    let session_cookie = cookie(
        SESSION_COOKIE,
        &session,
        SESSION_SECONDS,
        config.cookie_secure,
    );
    redirect(request, "/", &[clear_oauth, session_cookie])
}

fn dashboard(request: Request) -> Result<()> {
    let body = r#"<!doctype html>
<html lang="ja"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>YouTube Uploader</title>
<style>
:root{font-family:ui-sans-serif,system-ui,sans-serif;color:#18211f;background:#f2f5f2;font-synthesis:none}*{box-sizing:border-box}body{margin:0}header{background:#173b35;color:#f6f4eb;padding:20px max(24px,calc((100vw - 1120px)/2));display:flex;align-items:center;justify-content:space-between}h1{font-family:Georgia,serif;font-size:25px;font-weight:500;margin:0}button{font:inherit;border:1px solid currentColor;background:transparent;color:inherit;padding:8px 12px;cursor:pointer}main{max-width:1120px;margin:32px auto;padding:0 24px}.summary{display:flex;gap:48px;align-items:baseline;border-bottom:1px solid #cbd4ce;padding:0 0 22px;margin-bottom:28px}.summary strong{font:34px Georgia,serif}.summary span{color:#586660}h2{font:22px Georgia,serif;font-weight:500;margin:26px 0 12px}table{border-collapse:collapse;width:100%;background:#fff}th,td{text-align:left;padding:11px 13px;border-bottom:1px solid #e2e8e3;font-size:14px}th{color:#52615a;font-weight:600}.status{padding:24px 0;color:#586660}@media(max-width:600px){header{padding:18px 20px}main{margin:22px auto;padding:0 16px}.summary{gap:22px}td,th{padding:9px 7px;font-size:12px}}
</style></head><body><header><h1>YouTube Uploader</h1><button id="logout" type="button">ログアウト</button></header><main><div id="status" class="status">チャンネル情報を取得しています</div><section id="content" hidden><div class="summary"><div><strong id="video-count">0</strong><br><span>動画</span></div><div><strong id="playlist-count">0</strong><br><span>再生リスト</span></div><span id="channel"></span></div><h2>動画</h2><table><thead><tr><th>タイトル</th><th>公開設定</th><th>状態</th></tr></thead><tbody id="videos"></tbody></table><h2>再生リスト</h2><table><thead><tr><th>タイトル</th><th>公開設定</th></tr></thead><tbody id="playlists"></tbody></table></section></main><script src="/app.js" defer></script></body></html>"#;
    send(
        request,
        200,
        body.to_owned(),
        "text/html; charset=utf-8",
        &[],
    )
}

const APP_JS: &str = r#"'use strict';
const statusNode=document.querySelector('#status');
const text=(node,value)=>{node.textContent=value||'不明'};
fetch('/api/inventory',{credentials:'same-origin'}).then(async response=>{if(response.status===401){location.reload();return null}if(!response.ok)throw new Error('チャンネル情報を取得できませんでした');return response.json()}).then(data=>{if(!data)return;document.querySelector('#content').hidden=false;statusNode.remove();text(document.querySelector('#channel'),data.channel?.snippet?.title);const videos=data.videos||[];const playlists=data.playlists||[];text(document.querySelector('#video-count'),String(videos.length));text(document.querySelector('#playlist-count'),String(playlists.length));for(const video of videos){const row=document.createElement('tr');for(const value of [video.snippet?.title,video.status?.privacyStatus,video.processingDetails?.processingStatus]){const cell=document.createElement('td');text(cell,value);row.append(cell)}document.querySelector('#videos').append(row)}for(const playlist of playlists){const row=document.createElement('tr');for(const value of [playlist.snippet?.title,playlist.status?.privacyStatus]){const cell=document.createElement('td');text(cell,value);row.append(cell)}document.querySelector('#playlists').append(row)}}).catch(error=>{statusNode.textContent=error.message});
document.querySelector('#logout').addEventListener('click',()=>{location.assign('/logout')});"#;

fn authenticated_email(request: &Request, config: &Config) -> Option<String> {
    request_cookie(request, SESSION_COOKIE).and_then(|value| verify_session(config, &value))
}

fn serve_request(request: Request, config: &Config) -> Result<()> {
    let url = request_url(&request)?;
    match (request.method(), url.path()) {
        (&Method::Get, "/healthz") => send(request, 200, "ok".into(), "text/plain", &[]),
        (&Method::Get, "/") => {
            if authenticated_email(&request, config).is_some() {
                dashboard(request)
            } else {
                start_oauth(request, config)
            }
        }
        (&Method::Get, "/oauth/start") => start_oauth(request, config),
        (&Method::Post, "/oauth/callback") => complete_oauth(request, config),
        (&Method::Get, "/app.js") => send(
            request,
            200,
            APP_JS.into(),
            "application/javascript; charset=utf-8",
            &[],
        ),
        (&Method::Get, "/api/inventory") => {
            let Some(email) = authenticated_email(&request, config) else {
                return send(
                    request,
                    401,
                    "Authentication required".into(),
                    "text/plain",
                    &[],
                );
            };
            let credentials = load_credentials(config)?;
            let stored: Value = serde_json::from_slice(&credentials)?;
            ensure!(
                stored["email"].as_str() == Some(&email),
                "Session identity does not match OAuth credentials"
            );
            let youtube = crate::youtube::YouTube::from_authorized_user(&credentials)?;
            let result = youtube.inventory()?;
            send(
                request,
                200,
                serde_json::to_string(&result)?,
                "application/json",
                &[("Cache-Control", "no-store".into())],
            )
        }
        (&Method::Get, "/logout") => {
            let clear = cookie(SESSION_COOKIE, "deleted", 0, config.cookie_secure);
            send(request, 200, "<!doctype html><html lang=\"ja\"><meta charset=\"utf-8\"><title>ログアウト</title><p>ログアウトしました。</p><a href=\"/oauth/start\">Googleでログイン</a></html>".into(), "text/html; charset=utf-8", &[("Set-Cookie", clear)])
        }
        _ => send(request, 404, "Not found".into(), "text/plain", &[]),
    }
}

pub fn serve() -> Result<()> {
    let config = Config::from_env()?;
    ensure!(
        config.oauth_storage_configured(),
        "Set OAUTH_SECRET_RESOURCE or OAUTH_TOKEN_FILE"
    );
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse::<u16>()
        .context("PORT must be a valid TCP port")?;
    let server = Server::http(("0.0.0.0", port)).map_err(|error| anyhow!(error.to_string()))?;
    for request in server.incoming_requests() {
        let config = Config {
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            redirect_uri: config.redirect_uri.clone(),
            allowed_emails: config.allowed_emails.clone(),
            secret_resource: config.secret_resource.clone(),
            token_file: config.token_file.clone(),
            cookie_secure: config.cookie_secure,
        };
        std::thread::spawn(move || {
            if let Err(error) = serve_request(request, &config) {
                let _ = error;
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            client_id: "client-id".into(),
            client_secret: "test-signing-secret".into(),
            redirect_uri: "https://example.test/oauth/callback".into(),
            allowed_emails: vec!["owner@example.test".into()],
            secret_resource: Some("projects/test/secrets/oauth".into()),
            token_file: None,
            cookie_secure: true,
        }
    }

    #[test]
    fn signed_session_accepts_only_allowlisted_unexpired_identity() {
        let config = test_config();
        let session = session_value(&config, "owner@example.test", now().unwrap() + 60).unwrap();
        assert_eq!(
            verify_session(&config, &session).as_deref(),
            Some("owner@example.test")
        );
        assert!(verify_session(&config, &(session + "x")).is_none());
        let expired = session_value(&config, "owner@example.test", 1).unwrap();
        assert!(verify_session(&config, &expired).is_none());
        let other = session_value(&config, "other@example.test", now().unwrap() + 60).unwrap();
        assert!(verify_session(&config, &other).is_none());
    }

    #[test]
    fn oauth_authorization_uses_state_and_pkce_without_implicit_grant() {
        let config = test_config();
        let target = oauth_url(&config, "state-value", "verifier-value", "nonce-value").unwrap();
        let url = Url::parse(&target).unwrap();
        let query: std::collections::HashMap<String, String> = url
            .query_pairs()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        assert_eq!(query["state"], "state-value");
        assert_eq!(query["nonce"], "nonce-value");
        assert_eq!(query["code_challenge"], pkce_challenge("verifier-value"));
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["response_mode"], "form_post");
        assert_eq!(query["access_type"], "offline");
        assert_eq!(query["prompt"], "select_account");
    }

    #[test]
    fn cookies_are_http_only_lax_and_secure_in_cloud() {
        let value = cookie(SESSION_COOKIE, "opaque", 60, true);
        assert!(value.contains("HttpOnly"));
        assert!(value.contains("SameSite=Lax"));
        assert!(value.contains("Secure"));
        assert!(!value.contains("Domain="));
        let oauth = oauth_cookie("opaque", 600);
        assert!(oauth.contains("SameSite=None"));
        assert!(oauth.contains("Secure"));
    }
}
