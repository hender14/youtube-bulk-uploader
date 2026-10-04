use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use serde::Deserialize;
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
const MAX_VIDEO_BYTES: u64 = 256_000_000_000;

struct Config {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    allowed_emails: Vec<String>,
    secret_resource: Option<String>,
    token_file: Option<PathBuf>,
    video_bucket: Option<String>,
    state_bucket: Option<String>,
    job_resource: Option<String>,
    audit_confirmed: bool,
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
        let audit_confirmed = std::env::var("YOUTUBE_AUDIT_CONFIRMED")
            .map(|value| value == "true")
            .unwrap_or(false);
        Ok(Self {
            client_id,
            client_secret,
            redirect_uri,
            allowed_emails,
            secret_resource: std::env::var("OAUTH_SECRET_RESOURCE").ok(),
            token_file: std::env::var_os("OAUTH_TOKEN_FILE").map(PathBuf::from),
            video_bucket: std::env::var("VIDEO_BUCKET").ok(),
            state_bucket: std::env::var("STATE_BUCKET").ok(),
            job_resource: std::env::var("CLOUD_RUN_JOB_RESOURCE").ok(),
            audit_confirmed,
            cookie_secure,
        })
    }

    fn oauth_storage_configured(&self) -> bool {
        self.secret_resource.is_some() || self.token_file.is_some()
    }

    fn from_worker_env() -> Result<Self> {
        let allowed_emails: Vec<String> = required_env("OAUTH_ALLOWED_EMAILS")?
            .split(',')
            .map(|email| email.trim().to_ascii_lowercase())
            .filter(|email| !email.is_empty())
            .collect();
        ensure!(
            allowed_emails.len() == 1,
            "Configure exactly one Google account email"
        );
        Ok(Self {
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: String::new(),
            allowed_emails,
            secret_resource: std::env::var("OAUTH_SECRET_RESOURCE").ok(),
            token_file: std::env::var_os("OAUTH_TOKEN_FILE").map(PathBuf::from),
            video_bucket: std::env::var("VIDEO_BUCKET").ok(),
            state_bucket: std::env::var("STATE_BUCKET").ok(),
            job_resource: None,
            audit_confirmed: std::env::var("YOUTUBE_AUDIT_CONFIRMED")
                .map(|value| value == "true")
                .unwrap_or(false),
            cookie_secure: true,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUpload {
    file_name: String,
    file_size: u64,
    content_type: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default = "private_privacy")]
    privacy: String,
    made_for_kids: bool,
    #[serde(default)]
    playlist_id: Option<String>,
    #[serde(default)]
    audit_confirmed: bool,
}

fn private_privacy() -> String {
    "private".into()
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    ensure!(!value.trim().is_empty(), "{name} cannot be empty");
    Ok(value)
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub(crate) fn random_token() -> String {
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
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' https://storage.googleapis.com; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
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

pub(crate) fn metadata_access_token() -> Result<String> {
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
:root{font-family:Georgia,"Yu Mincho",serif;color:#1c2e2a;background:#f2f5f0;font-synthesis:none;--forest:#163e36;--lime:#d4e36d;--paper:#fffefa;--line:#cbd5ca;--muted:#62716a}*{box-sizing:border-box}body{margin:0}header{background:var(--forest);color:#f8f5e9;padding:19px max(24px,calc((100vw - 1100px)/2));display:flex;align-items:center;justify-content:space-between}h1{font-size:24px;font-weight:500;margin:0}header button{font:inherit;border:1px solid #a8b9ae;background:transparent;color:inherit;padding:8px 13px;cursor:pointer}main{max-width:1100px;margin:30px auto;padding:0 24px}h2{font-size:22px;font-weight:500;margin:0 0 16px}.upload{border-bottom:1px solid var(--line);padding-bottom:30px}.dropzone{display:grid;min-height:112px;place-items:center;border:1px dashed #6e8578;background:#e8eee5;color:#304a3e;cursor:pointer;text-align:center;padding:16px}.dropzone.drag{background:#dce8ca;border-color:var(--forest)}.dropzone input{position:absolute;width:1px;height:1px;opacity:0}#queue{margin:12px 0}.queue-item,.job-item{display:grid;grid-template-columns:minmax(0,1fr) minmax(170px,300px) auto;gap:12px;align-items:center;border-bottom:1px solid #dce3da;padding:9px 0}.queue-item input,.fields input,.fields select,.fields textarea{font:15px Georgia,"Yu Mincho",serif;color:inherit;border:1px solid #b8c5ba;background:var(--paper);padding:9px;min-width:0}.queue-item button,.jobs button,.primary{font:inherit;border:1px solid var(--forest);background:var(--forest);color:#fff;padding:9px 13px;cursor:pointer}.fields{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:12px;margin:14px 0}.fields label{display:grid;gap:6px;color:var(--muted);font-size:14px}.fields textarea{min-height:78px;resize:vertical}.span2{grid-column:1/-1}.toggle{display:flex!important;grid-template-columns:auto 1fr!important;align-items:center;justify-content:start;gap:9px!important;color:var(--forest)!important}.toggle input{width:17px;height:17px;accent-color:var(--forest)}.audit{display:none;background:#fff2ce;padding:12px;margin:12px 0}.audit.visible{display:block}.primary{background:var(--lime);border-color:var(--forest);color:#17382f;font-weight:600}.primary:disabled{opacity:.5;cursor:wait}.jobs{border-bottom:1px solid var(--line);padding:24px 0}.job-item{grid-template-columns:minmax(0,1fr) minmax(100px,180px) minmax(100px,170px)}.job-item progress{width:100%;accent-color:var(--forest)}.summary{display:flex;gap:40px;align-items:baseline;border-bottom:1px solid var(--line);padding:20px 0}.summary strong{font-size:32px}.summary span,.muted{color:var(--muted)}table{border-collapse:collapse;width:100%;background:var(--paper)}th,td{text-align:left;padding:11px 13px;border-bottom:1px solid #e2e8e3;font:14px Georgia,"Yu Mincho",serif}th{color:var(--muted);font-weight:600}.status{padding:14px 0;color:var(--muted);min-height:24px}.inventory{padding:26px 0}button:focus-visible,input:focus-visible,select:focus-visible,textarea:focus-visible{outline:3px solid #c1d34d;outline-offset:2px}@media(max-width:640px){header{padding:17px 18px}main{margin:20px auto;padding:0 16px}.fields{grid-template-columns:1fr}.span2{grid-column:auto}.queue-item{grid-template-columns:minmax(0,1fr) auto}.queue-item input{grid-column:1/2;grid-row:2}.queue-item button{grid-column:2;grid-row:1/3}.job-item{grid-template-columns:1fr auto}.job-item progress{grid-column:1/-1}.summary{gap:22px}td,th{padding:9px 7px;font-size:12px}}
</style></head><body><header><h1>YouTube Uploader</h1><button id="logout" type="button">ログアウト</button></header><main><section class="upload"><h2>動画をアップロード</h2><form id="upload-form"><label class="dropzone" id="dropzone"><input id="files" type="file" accept="video/*" multiple><span>動画を選択、またはここへドラッグ&ドロップ</span></label><div id="queue"></div><div class="fields"><label>公開設定<select id="privacy"><option value="private">非公開</option><option value="unlisted">限定公開</option><option value="public">公開</option></select></label><label>視聴者<select id="audience"><option value="not-kids">子ども向けではない</option><option value="kids">子ども向け</option></select></label><label class="toggle span2"><input id="playlist-toggle" type="checkbox">既存の再生リストへ追加</label><label id="playlist-field" class="span2">再生リスト<select id="playlist" disabled><option value="">選択してください</option></select></label><label class="span2">説明<textarea id="description" maxlength="5000"></textarea></label></div><div id="audit" class="audit"><label class="toggle"><input id="audit-confirmed" type="checkbox">YouTubeのAPI監査が完了し、この公開設定でアップロード可能なことを確認しました</label></div><button id="upload-button" class="primary" type="submit">選択した動画をアップロード</button></form></section><section class="jobs"><h2>アップロード状況</h2><div id="jobs" class="muted">アップロード履歴はありません</div></section><div id="inventory-status" class="status">チャンネル情報を取得しています</div><section id="content" class="inventory" hidden><div class="summary"><div><strong id="video-count">0</strong><br><span>動画</span></div><div><strong id="playlist-count">0</strong><br><span>再生リスト</span></div><span id="channel"></span></div><h2>動画</h2><table><thead><tr><th>タイトル</th><th>公開設定</th><th>状態</th></tr></thead><tbody id="videos"></tbody></table><h2 style="margin-top:28px">再生リスト</h2><table><thead><tr><th>タイトル</th><th>公開設定</th></tr></thead><tbody id="playlists"></tbody></table></section></main><script src="/app.js" defer></script></body></html>"#;
    send(
        request,
        200,
        body.to_owned(),
        "text/html; charset=utf-8",
        &[],
    )
}

const APP_JS: &str = r#"'use strict';
const $=selector=>document.querySelector(selector);
const pendingKey='yt-uploader.pending.v1';
const statusText={waiting_for_file:'ファイル待ち',uploading_to_storage:'PCからクラウドへ転送中',staged:'YouTube処理待ち',queued:'YouTube処理を予約済み',uploading_to_youtube:'YouTubeへ転送中',youtube_uploaded:'YouTube確認中',staging_deleted:'一時動画を削除済み',completed:'完了',failed:'失敗'};
let queue=[];
function pending(){try{return JSON.parse(localStorage.getItem(pendingKey)||'[]')}catch{return []}}
function savePending(items){localStorage.setItem(pendingKey,JSON.stringify(items))}
function setStatus(node,message){node.textContent=message}
async function api(path,options={}){const response=await fetch(path,{credentials:'same-origin',...options});if(response.status===401){location.reload();throw new Error('再ログインが必要です')}const data=await response.json().catch(()=>({}));if(!response.ok)throw new Error(data.error||'リクエストに失敗しました');return data}
function titleFrom(file){return file.name.replace(/\.[^.]+$/,'').slice(0,100)}
function queueFiles(files){for(const file of files){if(!file.type.startsWith('video/'))continue;if(!queue.some(item=>item.file.name===file.name&&item.file.size===file.size))queue.push({file,title:titleFrom(file)})}renderQueue()}
function renderQueue(){const host=$('#queue');host.replaceChildren();for(const item of queue){const row=document.createElement('div');row.className='queue-item';const name=document.createElement('span');name.textContent=`${item.file.name} · ${(item.file.size/1073741824).toFixed(2)} GB`;const title=document.createElement('input');title.value=item.title;title.maxLength=100;title.setAttribute('aria-label',`${item.file.name}のタイトル`);title.addEventListener('input',()=>item.title=title.value);const remove=document.createElement('button');remove.type='button';remove.textContent='削除';remove.addEventListener('click',()=>{queue=queue.filter(value=>value!==item);renderQueue()});row.append(name,title,remove);host.append(row)}}
function showJobs(){const host=$('#jobs');const list=pending();host.replaceChildren();if(!list.length){host.textContent='アップロード履歴はありません';host.className='muted';return}host.className='';for(const item of list){const row=document.createElement('div');row.className='job-item';const name=document.createElement('span');name.textContent=item.fileName;const state=document.createElement('span');state.textContent=statusText[item.status]||'状態を確認中';const progress=document.createElement('progress');progress.max=100;progress.value=item.progress||0;row.append(name,state,progress);if(item.status==='failed'){const retry=document.createElement('button');retry.type='button';retry.textContent='再試行';retry.addEventListener('click',()=>retryJob(item.id));row.append(retry)}host.append(row)}}
function updateItem(id,changes){const list=pending();const item=list.find(value=>value.id===id);if(item)Object.assign(item,changes);savePending(list);showJobs()}
async function refreshJob(id){try{const result=await api(`/api/uploads/${encodeURIComponent(id)}/status`);updateItem(id,{status:result.status,videoId:result.videoId});if(!['completed','failed'].includes(result.status))setTimeout(()=>refreshJob(id),4000)}catch{setTimeout(()=>refreshJob(id),8000)}}
async function retryJob(id){try{updateItem(id,{status:'queued'});await api(`/api/uploads/${encodeURIComponent(id)}/retry`,{method:'POST'});refreshJob(id)}catch(error){updateItem(id,{status:'failed'});alert(error.message)}}
function uploadChunk(uri,blob,start,end,total,onProgress){return new Promise((resolve,reject)=>{const xhr=new XMLHttpRequest();xhr.open('PUT',uri);xhr.setRequestHeader('Content-Range',`bytes ${start}-${end-1}/${total}`);xhr.upload.onprogress=event=>{if(event.lengthComputable)onProgress(start+event.loaded,total)};xhr.onload=()=>{if(xhr.status===308){const range=xhr.getResponseHeader('Range');const match=range&&range.match(/bytes=0-(\d+)/);resolve({offset:match?Number(match[1])+1:0,complete:false})}else if(xhr.status===200||xhr.status===201)resolve({offset:total,complete:true});else reject(new Error(`クラウド転送に失敗しました (HTTP ${xhr.status})`))};xhr.onerror=()=>reject(new Error('クラウドへの接続が中断しました。再度ファイルを選ぶと続きから再開できます'));xhr.onabort=()=>reject(new Error('転送を中断しました'));xhr.send(blob)})}
async function uploadFile(item,options){const {file,title}=item;let list=pending();let saved=list.find(value=>value.fileName===file.name&&value.fileSize===file.size&&value.title===title&&value.description===options.description&&value.privacy===options.privacy&&value.playlistId===options.playlist_id&&value.madeForKids===options.made_for_kids);if(!saved){const created=await api('/api/uploads',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({file_name:file.name,file_size:file.size,content_type:file.type||'video/mp4',title,description:options.description,privacy:options.privacy,made_for_kids:options.made_for_kids,playlist_id:options.playlist_id,audit_confirmed:options.audit_confirmed})});saved={id:created.id,fileName:file.name,fileSize:file.size,title,description:options.description,privacy:options.privacy,playlistId:options.playlist_id,madeForKids:options.made_for_kids,status:'uploading_to_storage',progress:0};list=pending();list.push(saved);savePending(list);showJobs()}const session=await api(`/api/uploads/${encodeURIComponent(saved.id)}/session`);if(!session.complete){let offset=session.nextByte;while(offset<file.size){const end=Math.min(offset+8*1024*1024,file.size);const chunk=file.slice(offset,end);const result=await uploadChunk(session.sessionUri,chunk,offset,end,file.size,(sent,total)=>updateItem(saved.id,{progress:Math.floor(sent/total*100),status:'uploading_to_storage'}));if(result.complete){offset=file.size;break}if(result.offset<=offset)throw new Error('転送位置を確認できません。再開操作をしてください');offset=result.offset}}updateItem(saved.id,{progress:100,status:'staged'});await api(`/api/uploads/${encodeURIComponent(saved.id)}/finalize`,{method:'POST'});updateItem(saved.id,{status:'queued'});refreshJob(saved.id)}
async function submit(event){event.preventDefault();if(!queue.length){alert('動画ファイルを選択してください');return}const privacy=$('#privacy').value;const playlistId=$('#playlist-toggle').checked?$('#playlist').value:null;if($('#playlist-toggle').checked&&!playlistId){alert('再生リストを選択してください');return}const options={description:$('#description').value,privacy,made_for_kids:$('#audience').value==='kids',playlist_id:playlistId,audit_confirmed:$('#audit-confirmed').checked};const button=$('#upload-button');button.disabled=true;const items=[...queue];for(const item of items){try{await uploadFile(item,options);queue=queue.filter(value=>value!==item);renderQueue()}catch(error){alert(`${item.file.name}: ${error.message}`);break}}button.disabled=false}
$('#files').addEventListener('change',event=>queueFiles(event.target.files));
const drop=$('#dropzone');for(const name of ['dragenter','dragover'])drop.addEventListener(name,event=>{event.preventDefault();drop.classList.add('drag')});for(const name of ['dragleave','drop'])drop.addEventListener(name,event=>{event.preventDefault();drop.classList.remove('drag')});drop.addEventListener('drop',event=>queueFiles(event.dataTransfer.files));
$('#playlist-toggle').addEventListener('change',event=>$('#playlist').disabled=!event.target.checked);
$('#privacy').addEventListener('change',event=>$('#audit').classList.toggle('visible',event.target.value!=='private'));
$('#upload-form').addEventListener('submit',submit);
$('#logout').addEventListener('click',()=>location.assign('/logout'));
showJobs();for(const item of pending())if(!['completed','failed'].includes(item.status))refreshJob(item.id);
api('/api/inventory').then(data=>{const videos=data.videos||[],playlists=data.playlists||[];$('#content').hidden=false;$('#inventory-status').remove();$('#channel').textContent=data.channel?.snippet?.title||'';$('#video-count').textContent=String(videos.length);$('#playlist-count').textContent=String(playlists.length);for(const video of videos){const row=document.createElement('tr');for(const value of [video.snippet?.title,video.status?.privacyStatus,video.processingDetails?.processingStatus]){const cell=document.createElement('td');cell.textContent=value||'不明';row.append(cell)}$('#videos').append(row)}for(const playlist of playlists){const option=document.createElement('option');option.value=playlist.id;option.textContent=playlist.snippet?.title||playlist.id;$('#playlist').append(option);const row=document.createElement('tr');for(const value of [playlist.snippet?.title,playlist.status?.privacyStatus]){const cell=document.createElement('td');cell.textContent=value||'不明';row.append(cell)}$('#playlists').append(row)}}).catch(error=>setStatus($('#inventory-status'),error.message));"#;

fn authenticated_email(request: &Request, config: &Config) -> Option<String> {
    request_cookie(request, SESSION_COOKIE).and_then(|value| verify_session(config, &value))
}

fn parse_new_upload(request: &mut Request) -> Result<NewUpload> {
    ensure!(
        request
            .headers()
            .iter()
            .any(|header| header.field.equiv("Content-Type")
                && header.value.as_str().starts_with("application/json")),
        "Upload metadata must be JSON"
    );
    let mut body = String::new();
    request.as_reader().take(16_385).read_to_string(&mut body)?;
    ensure!(body.len() <= 16_384, "Upload metadata exceeds size limit");
    Ok(serde_json::from_str(&body)?)
}

fn validate_new_upload(upload: &NewUpload, audit_enabled: bool) -> Result<()> {
    ensure!(
        !upload.file_name.trim().is_empty()
            && upload.file_name.chars().count() <= 255
            && !upload.file_name.chars().any(char::is_control),
        "Invalid video filename"
    );
    ensure!(
        upload.file_size > 0 && upload.file_size <= MAX_VIDEO_BYTES,
        "Video file size is outside supported limits"
    );
    ensure!(
        upload.content_type.starts_with("video/"),
        "Select a video file"
    );
    ensure!(
        !upload.title.trim().is_empty() && upload.title.chars().count() <= 100,
        "Title must contain 1 to 100 characters"
    );
    ensure!(upload.description.len() <= 5000, "Description is too long");
    ensure!(
        ["private", "unlisted", "public"].contains(&upload.privacy.as_str()),
        "Invalid video visibility"
    );
    ensure!(
        upload.privacy == "private" || (audit_enabled && upload.audit_confirmed),
        "Non-private upload requires confirmed project audit status"
    );
    if let Some(playlist_id) = &upload.playlist_id {
        ensure!(
            (10..=100).contains(&playlist_id.len())
                && playlist_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'),
            "Invalid playlist identifier"
        );
    }
    Ok(())
}

fn create_upload(mut request: Request, email: &str, config: &Config) -> Result<()> {
    let upload = match parse_new_upload(&mut request) {
        Ok(upload) => upload,
        Err(_) => {
            return send(
                request,
                400,
                json!({"error":"Invalid upload metadata"}).to_string(),
                "application/json",
                &[],
            );
        }
    };
    if validate_new_upload(&upload, config.audit_confirmed).is_err() {
        return send(
            request,
            400,
            json!({"error":"Upload metadata is not allowed"}).to_string(),
            "application/json",
            &[],
        );
    }
    let (Some(video_bucket), Some(state_bucket)) = (&config.video_bucket, &config.state_bucket)
    else {
        return send(
            request,
            503,
            json!({"error":"Cloud upload is not configured"}).to_string(),
            "application/json",
            &[],
        );
    };
    let id = random_token();
    let record = crate::cloud_storage::UploadRecord {
        id: id.clone(),
        owner_email: email.to_owned(),
        object_name: format!("incoming/{id}"),
        session_uri: String::new(),
        file_size: upload.file_size,
        content_type: upload.content_type,
        original_name: upload.file_name,
        title: upload.title,
        description: upload.description,
        privacy: upload.privacy,
        made_for_kids: upload.made_for_kids,
        playlist_id: upload.playlist_id,
        youtube_session_uri: None,
        youtube_uploaded_bytes: 0,
        video_id: None,
        status: "uploading_to_storage".into(),
    };
    let record = match crate::cloud_storage::begin_upload(video_bucket, state_bucket, record) {
        Ok(record) => record,
        Err(_) => {
            return send(
                request,
                502,
                json!({"error":"Could not create a secure upload session"}).to_string(),
                "application/json",
                &[],
            );
        }
    };
    send(
        request,
        201,
        json!({"id":record.id,"fileName":record.original_name,"fileSize":record.file_size,"status":"waiting_for_file"}).to_string(),
        "application/json",
        &[],
    )
}

fn upload_session(request: Request, id: &str, email: &str, config: &Config) -> Result<()> {
    let Some(state_bucket) = &config.state_bucket else {
        return send(
            request,
            503,
            json!({"error":"Cloud upload is not configured"}).to_string(),
            "application/json",
            &[],
        );
    };
    let stored = match crate::cloud_storage::load_upload_versioned(state_bucket, id) {
        Ok(stored) if stored.record.owner_email == email => stored,
        _ => {
            return send(
                request,
                404,
                json!({"error":"Upload session not found"}).to_string(),
                "application/json",
                &[],
            );
        }
    };
    let mut record = stored.record;
    let mut generation = stored.generation;
    let (offset, complete) = match crate::cloud_storage::acknowledged_bytes(&record) {
        Ok(progress) => progress,
        Err(_) => {
            return send(
                request,
                502,
                json!({"error":"Could not check upload progress"}).to_string(),
                "application/json",
                &[],
            );
        }
    };
    if complete && record.status == "uploading_to_storage" {
        record.status = "staged".into();
        generation = crate::cloud_storage::save_upload(state_bucket, &record, generation)?;
    }
    send(
        request,
        200,
        json!({"id":record.id,"sessionUri":record.session_uri,"nextByte":offset,"complete":complete,"fileSize":record.file_size,"status":record.status,"generation":generation}).to_string(),
        "application/json",
        &[],
    )
}

fn run_worker_job(job_resource: &str, id: &str) -> Result<String> {
    let pieces: Vec<&str> = job_resource.split('/').collect();
    ensure!(
        pieces.len() == 6
            && pieces[0] == "projects"
            && !pieces[1].is_empty()
            && pieces[2] == "locations"
            && !pieces[3].is_empty()
            && pieces[4] == "jobs"
            && !pieces[5].is_empty(),
        "Invalid Cloud Run Job resource"
    );
    let token = metadata_access_token()?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .post(format!("https://run.googleapis.com/v2/{job_resource}:run"))
        .bearer_auth(token)
        .json(&json!({"overrides":{"containerOverrides":[{"name":"worker","env":[{"name":"UPLOAD_ID","value":id}]}]}}))
        .send()
        .context("Unable to start the Cloud Run upload Job")?;
    ensure!(
        response.status().is_success(),
        "Cloud Run rejected the upload Job"
    );
    let operation: Value = response
        .json()
        .context("Invalid Cloud Run operation response")?;
    operation["name"]
        .as_str()
        .map(str::to_owned)
        .context("Cloud Run did not return a Job operation name")
}

fn finalize_upload(request: Request, id: &str, email: &str, config: &Config) -> Result<()> {
    let (Some(state_bucket), Some(job_resource)) = (&config.state_bucket, &config.job_resource)
    else {
        return send(
            request,
            503,
            json!({"error":"Cloud upload jobs are not configured"}).to_string(),
            "application/json",
            &[],
        );
    };
    let stored = match crate::cloud_storage::load_upload_versioned(state_bucket, id) {
        Ok(stored) if stored.record.owner_email == email => stored,
        _ => {
            return send(
                request,
                404,
                json!({"error":"Upload session not found"}).to_string(),
                "application/json",
                &[],
            );
        }
    };
    let mut record = stored.record;
    let mut generation = stored.generation;
    if [
        "queued",
        "uploading_to_youtube",
        "youtube_uploaded",
        "completed",
        "staging_deleted",
    ]
    .contains(&record.status.as_str())
    {
        return send(
            request,
            202,
            json!({"id":record.id,"status":record.status,"videoId":record.video_id}).to_string(),
            "application/json",
            &[],
        );
    }
    if record.status != "staged" {
        return send(
            request,
            409,
            json!({"error":"Video transfer has not completed"}).to_string(),
            "application/json",
            &[],
        );
    }
    match crate::cloud_storage::acknowledged_bytes(&record) {
        Ok((offset, true)) if offset == record.file_size => {}
        _ => {
            return send(
                request,
                409,
                json!({"error":"Cloud Storage has not confirmed the complete video"}).to_string(),
                "application/json",
                &[],
            );
        }
    }
    record.status = "queued".into();
    generation = match crate::cloud_storage::save_upload(state_bucket, &record, generation) {
        Ok(generation) => generation,
        Err(_) => {
            return send(
                request,
                409,
                json!({"error":"Upload is already being queued"}).to_string(),
                "application/json",
                &[],
            );
        }
    };
    let operation = match run_worker_job(job_resource, id) {
        Ok(operation) => operation,
        Err(_) => {
            record.status = "staged".into();
            let _ = crate::cloud_storage::save_upload(state_bucket, &record, generation);
            return send(
                request,
                502,
                json!({"error":"Could not queue YouTube upload"}).to_string(),
                "application/json",
                &[],
            );
        }
    };
    send(
        request,
        202,
        json!({"id":id,"status":"queued","operation":operation}).to_string(),
        "application/json",
        &[],
    )
}

fn upload_status(request: Request, id: &str, email: &str, config: &Config) -> Result<()> {
    let Some(state_bucket) = &config.state_bucket else {
        return send(
            request,
            503,
            json!({"error":"Cloud upload is not configured"}).to_string(),
            "application/json",
            &[],
        );
    };
    let stored = match crate::cloud_storage::load_upload_versioned(state_bucket, id) {
        Ok(stored) if stored.record.owner_email == email => stored.record,
        _ => {
            return send(
                request,
                404,
                json!({"error":"Upload session not found"}).to_string(),
                "application/json",
                &[],
            );
        }
    };
    send(
        request,
        200,
        json!({"id":stored.id,"status":stored.status,"videoId":stored.video_id,"fileName":stored.original_name,"fileSize":stored.file_size,"youtubeUploadedBytes":stored.youtube_uploaded_bytes}).to_string(),
        "application/json",
        &[],
    )
}

fn retry_upload(request: Request, id: &str, email: &str, config: &Config) -> Result<()> {
    let Some(state_bucket) = &config.state_bucket else {
        return send(
            request,
            503,
            json!({"error":"Cloud upload is not configured"}).to_string(),
            "application/json",
            &[],
        );
    };
    let stored = match crate::cloud_storage::load_upload_versioned(state_bucket, id) {
        Ok(stored) if stored.record.owner_email == email => stored,
        _ => {
            return send(
                request,
                404,
                json!({"error":"Upload session not found"}).to_string(),
                "application/json",
                &[],
            );
        }
    };
    let mut record = stored.record;
    if record.status != "failed" {
        return send(
            request,
            409,
            json!({"error":"Only failed uploads can be retried"}).to_string(),
            "application/json",
            &[],
        );
    }
    record.status = "staged".into();
    if crate::cloud_storage::save_upload(state_bucket, &record, stored.generation).is_err() {
        return send(
            request,
            409,
            json!({"error":"Upload state changed; refresh before retrying"}).to_string(),
            "application/json",
            &[],
        );
    }
    finalize_upload(request, id, email, config)
}

fn serve_request(request: Request, config: &Config) -> Result<()> {
    let url = request_url(&request)?;
    if *request.method() == Method::Get
        && let Some(id) = url
            .path()
            .strip_prefix("/api/uploads/")
            .and_then(|tail| tail.strip_suffix("/session"))
    {
        let Some(email) = authenticated_email(&request, config) else {
            return send(
                request,
                401,
                json!({"error":"Authentication required"}).to_string(),
                "application/json",
                &[],
            );
        };
        return upload_session(request, id, &email, config);
    }
    if *request.method() == Method::Get
        && let Some(id) = url
            .path()
            .strip_prefix("/api/uploads/")
            .and_then(|tail| tail.strip_suffix("/status"))
    {
        let Some(email) = authenticated_email(&request, config) else {
            return send(
                request,
                401,
                json!({"error":"Authentication required"}).to_string(),
                "application/json",
                &[],
            );
        };
        return upload_status(request, id, &email, config);
    }
    if *request.method() == Method::Post
        && let Some(id) = url
            .path()
            .strip_prefix("/api/uploads/")
            .and_then(|tail| tail.strip_suffix("/finalize"))
    {
        let Some(email) = authenticated_email(&request, config) else {
            return send(
                request,
                401,
                json!({"error":"Authentication required"}).to_string(),
                "application/json",
                &[],
            );
        };
        return finalize_upload(request, id, &email, config);
    }
    if *request.method() == Method::Post
        && let Some(id) = url
            .path()
            .strip_prefix("/api/uploads/")
            .and_then(|tail| tail.strip_suffix("/retry"))
    {
        let Some(email) = authenticated_email(&request, config) else {
            return send(
                request,
                401,
                json!({"error":"Authentication required"}).to_string(),
                "application/json",
                &[],
            );
        };
        return retry_upload(request, id, &email, config);
    }
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
        (&Method::Post, "/api/uploads") => {
            let Some(email) = authenticated_email(&request, config) else {
                return send(
                    request,
                    401,
                    json!({"error":"Authentication required"}).to_string(),
                    "application/json",
                    &[],
                );
            };
            create_upload(request, &email, config)
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
            video_bucket: config.video_bucket.clone(),
            state_bucket: config.state_bucket.clone(),
            job_resource: config.job_resource.clone(),
            audit_confirmed: config.audit_confirmed,
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

pub fn run_worker() -> Result<()> {
    let config = Config::from_worker_env()?;
    let id = required_env("UPLOAD_ID")?;
    let state_bucket = config
        .state_bucket
        .as_deref()
        .context("STATE_BUCKET is required for the worker")?;
    let video_bucket = config
        .video_bucket
        .as_deref()
        .context("VIDEO_BUCKET is required for the worker")?;
    let stored = crate::cloud_storage::load_upload_versioned(state_bucket, &id)?;
    let mut record = stored.record;
    let mut generation = stored.generation;
    ensure!(
        config.allowed_emails.contains(&record.owner_email),
        "Upload owner is not allowlisted"
    );
    if record.status == "completed" {
        return Ok(());
    }
    record.status = "uploading_to_youtube".into();
    generation = crate::cloud_storage::save_upload(state_bucket, &record, generation)?;
    let result = (|| -> Result<()> {
        let credentials = load_credentials(&config)?;
        let mut youtube = crate::youtube::YouTube::from_authorized_user(&credentials)?;
        let (video, updated_generation) = if let Some(video_id) = record.video_id.as_deref() {
            (youtube.video(video_id)?, generation)
        } else {
            youtube.upload_from_storage(
                video_bucket,
                state_bucket,
                &mut record,
                generation,
                config.audit_confirmed,
            )?
        };
        generation = updated_generation;
        ensure!(
            video["status"]["privacyStatus"] == record.privacy,
            "YouTube video privacy does not match requested settings"
        );
        record.video_id = video["id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| record.video_id.clone());
        ensure!(record.video_id.is_some(), "YouTube video ID is missing");
        if record.status != "staging_deleted" {
            crate::cloud_storage::delete_video_object(video_bucket, &record)?;
            record.status = "staging_deleted".into();
            generation = crate::cloud_storage::save_upload(state_bucket, &record, generation)?;
        }
        if let Some(playlist_id) = &record.playlist_id {
            youtube.add_to_playlist(playlist_id, record.video_id.as_deref().unwrap())?;
        }
        record.status = "completed".into();
        crate::cloud_storage::save_upload(state_bucket, &record, generation)?;
        println!(
            "{}",
            json!({"id":record.id,"videoId":record.video_id,"status":"completed"})
        );
        Ok(())
    })();
    if let Err(error) = result {
        if let Ok(stored) = crate::cloud_storage::load_upload_versioned(state_bucket, &id) {
            record = stored.record;
            generation = stored.generation;
        }
        record.status = "failed".into();
        let _ = crate::cloud_storage::save_upload(state_bucket, &record, generation);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            client_id: "client-id".into(),
            client_secret: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
            redirect_uri: "https://example.test/oauth/callback".into(),
            allowed_emails: vec!["owner@example.test".into()],
            secret_resource: Some("projects/test/secrets/oauth".into()),
            token_file: None,
            video_bucket: Some("test-videos".into()),
            state_bucket: Some("test-state".into()),
            job_resource: Some("projects/test/locations/asia-northeast1/jobs/uploader".into()),
            audit_confirmed: false,
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
    fn upload_validation_requires_audit_for_non_private_visibility() {
        let upload = NewUpload {
            file_name: "clip.mp4".into(),
            file_size: 1024,
            content_type: "video/mp4".into(),
            title: "clip".into(),
            description: String::new(),
            privacy: "private".into(),
            made_for_kids: false,
            playlist_id: None,
            audit_confirmed: false,
        };
        assert!(validate_new_upload(&upload, false).is_ok());
        let mut unlisted = NewUpload {
            privacy: "unlisted".into(),
            audit_confirmed: true,
            ..upload
        };
        assert!(validate_new_upload(&unlisted, false).is_err());
        assert!(validate_new_upload(&unlisted, true).is_ok());
        unlisted.file_size = MAX_VIDEO_BYTES + 1;
        assert!(validate_new_upload(&unlisted, true).is_err());
    }

    #[test]
    fn cloud_run_job_resource_must_be_a_full_resource_name() {
        assert!(run_worker_job("not-a-resource", "upload-id").is_err());
    }

    #[test]
    fn authenticated_http_routes_serve_upload_dashboard_and_javascript() {
        let config = test_config();
        let session = session_value(&config, "owner@example.test", now().unwrap() + 60).unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let request = server.recv().unwrap();
                serve_request(request, &config).unwrap();
            }
        });
        let client = Client::new();
        let dashboard = client
            .get(&base)
            .header("Cookie", format!("{SESSION_COOKIE}={session}"))
            .send()
            .unwrap();
        assert!(dashboard.status().is_success());
        assert!(
            dashboard.headers()["Content-Security-Policy"]
                .to_str()
                .unwrap()
                .contains("https://storage.googleapis.com")
        );
        let html = dashboard.text().unwrap();
        assert!(html.contains("id=\"upload-form\""));
        assert!(html.contains("id=\"playlist-toggle\""));
        assert!(html.contains("id=\"files\" type=\"file\" accept=\"video/*\" multiple"));
        let javascript = client.get(format!("{base}/app.js")).send().unwrap();
        assert!(javascript.text().unwrap().contains("Content-Range"));
        handle.join().unwrap();
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
