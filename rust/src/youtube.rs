use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Deserialize)]
struct Credentials {
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

pub struct YouTube {
    http: Client,
    token: String,
    base_url: String,
    token_path: Option<PathBuf>,
    refresh_credentials: Option<Credentials>,
    production_upload: bool,
    refreshed: Instant,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UploadOptions {
    pub title: String,
    pub description: String,
    pub privacy: String,
    pub made_for_kids: bool,
}

#[derive(Serialize, Deserialize)]
struct UploadRecord {
    options: UploadOptions,
    session_uri: Option<String>,
    uploaded_bytes: u64,
    video_id: Option<String>,
    file_size: u64,
}

fn save_record(path: &Path, record: &UploadRecord) -> Result<()> {
    let parent = path.parent().context("State directory missing")?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, record)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn fingerprint(path: &Path) -> Result<(String, u64)> {
    let mut source = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    let mut size = 0;
    loop {
        let length = source.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
        size += length as u64;
    }
    Ok((format!("{:x}", hash.finalize()), size))
}

fn execute(request: RequestBuilder) -> Result<Value> {
    let response = request.send().map_err(|error| error.without_url())?;
    let status = response.status();
    if status == StatusCode::NO_CONTENT {
        return Ok(Value::Null);
    }
    let body: Value = response
        .json()
        .map_err(|error| error.without_url())
        .context("Invalid Google JSON response")?;
    if !status.is_success() {
        let reason = body["error"]["errors"][0]["reason"]
            .as_str()
            .unwrap_or("request_failed");
        bail!(
            "Google API failed (HTTP {}, reason {})",
            status.as_u16(),
            reason
        );
    }
    Ok(body)
}

impl YouTube {
    pub fn from_token_file(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).context("Unable to read OAuth file")?;
        Self::from_authorized_user(&data).map(|mut youtube| {
            youtube.token_path = Some(path.into());
            youtube
        })
    }

    pub fn from_authorized_user(data: &[u8]) -> Result<Self> {
        let credentials: Credentials = serde_json::from_slice(data)
            .context("OAuth data needs client_id, client_secret and refresh_token")?;
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()?;
        let token = Self::refresh_token(&http, &credentials)?;
        Ok(Self {
            http,
            token,
            base_url: "https://www.googleapis.com/youtube/v3".into(),
            token_path: None,
            refresh_credentials: Some(credentials),
            production_upload: true,
            refreshed: Instant::now(),
        })
    }

    fn refresh_token(http: &Client, credentials: &Credentials) -> Result<String> {
        let response = execute(http.post("https://oauth2.googleapis.com/token").form(&[
            ("grant_type", "refresh_token"),
            ("client_id", credentials.client_id.as_str()),
            ("client_secret", credentials.client_secret.as_str()),
            ("refresh_token", credentials.refresh_token.as_str()),
        ]))?;
        response["access_token"]
            .as_str()
            .context("Missing OAuth access token")
            .map(str::to_owned)
    }

    fn upload_request(&mut self, method: Method, uri: &str) -> Result<RequestBuilder> {
        if self.refreshed.elapsed() >= Duration::from_secs(3000)
            && let Some(credentials) = self.refresh_credentials.as_ref()
        {
            let token = Self::refresh_token(&self.http, credentials)?;
            self.token = token;
            self.refreshed = Instant::now();
        }
        let url = reqwest::Url::parse(uri)?;
        let host = url.host_str().context("Upload session missing host")?;
        let google = url.scheme() == "https"
            && (host == "googleapis.com" || host.ends_with(".googleapis.com"));
        let local_test = !self.production_upload && uri.starts_with(&format!("{}/", self.base_url));
        ensure!(google || local_test, "Refusing non-Google upload endpoint");
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "Invalid upload endpoint authority"
        );
        Ok(self
            .http
            .request(method, uri)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(180)))
    }

    fn upload_result(
        response: reqwest::blocking::Response,
        size: u64,
    ) -> Result<(u64, Option<String>)> {
        let status = response.status();
        if status.as_u16() == 308 {
            let range = response
                .headers()
                .get("Range")
                .map(|value| value.to_str())
                .transpose()?;
            return Ok((crate::acknowledged_offset(range, size)?, None));
        }
        ensure!(
            status.is_success(),
            "Upload paused (HTTP {}); rerun to query the same session",
            status.as_u16()
        );
        let body: Value = response.json().map_err(|error| error.without_url())?;
        let video_id = body["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("Upload completion missing video ID")?;
        Ok((size, Some(video_id.into())))
    }

    pub fn upload(
        &mut self,
        path: &Path,
        options: UploadOptions,
        directory: &Path,
        audited: bool,
        chunk_size: usize,
    ) -> Result<Value> {
        ensure!(
            !options.title.trim().is_empty() && options.title.chars().count() <= 100,
            "Title must contain 1 to 100 characters"
        );
        ensure!(
            options.description.len() <= 5000,
            "Description exceeds 5000 bytes"
        );
        ensure!(
            ["private", "unlisted", "public"].contains(&options.privacy.as_str()),
            "Invalid privacy setting"
        );
        ensure!(
            options.privacy == "private" || audited,
            "Non-private uploads require administrator audit confirmation"
        );
        ensure!(
            chunk_size > 0 && chunk_size.is_multiple_of(256 * 1024),
            "Chunk size must be a positive multiple of 256 KiB"
        );
        let (digest, size) = fingerprint(path)?;
        ensure!(size > 0, "Empty video file");
        let state_path = directory.join(format!("{digest}.json"));
        std::fs::create_dir_all(directory)?;
        let lock_path = directory.join(format!("{digest}.lock"));
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)?;
        lock.try_lock()
            .context("Another process is uploading this file")?;
        let mut record = if state_path.is_file() {
            serde_json::from_slice::<UploadRecord>(&std::fs::read(&state_path)?)?
        } else {
            UploadRecord {
                options: options.clone(),
                session_uri: None,
                uploaded_bytes: 0,
                video_id: None,
                file_size: size,
            }
        };
        ensure!(
            record.options == options && record.file_size == size,
            "Saved upload metadata differs; reuse the original options"
        );
        if let Some(video_id) = &record.video_id {
            return Ok(json!({"id":video_id,"already_uploaded":true}));
        }
        if let Some(uri) = &record.session_uri {
            let response = self
                .upload_request(Method::PUT, uri)?
                .header("Content-Length", "0")
                .header("Content-Range", format!("bytes */{size}"))
                .body(Vec::new())
                .send()
                .map_err(|error| error.without_url())?;
            let (offset, completed) = Self::upload_result(response, size)?;
            record.uploaded_bytes = offset;
            record.video_id = completed;
        } else {
            let inventory = self.inventory()?;
            ensure!(
                !inventory["videos"]
                    .as_array()
                    .context("Video inventory missing")?
                    .iter()
                    .any(|video| video["snippet"]["title"] == options.title),
                "A video with this title exists; review it before uploading"
            );
            let uri = if self.production_upload {
                "https://www.googleapis.com/upload/youtube/v3/videos".into()
            } else {
                format!("{}/upload", self.base_url)
            };
            let response = self.upload_request(Method::POST, &uri)?.query(&[("uploadType", "resumable"), ("part", "snippet,status")])
                .header("X-Upload-Content-Length", size.to_string()).header("X-Upload-Content-Type", "application/octet-stream")
                .json(&json!({"snippet":{"title":options.title,"description":options.description,"categoryId":"22"},"status":{"privacyStatus":options.privacy,"selfDeclaredMadeForKids":options.made_for_kids}}))
                .send().map_err(|error| error.without_url())?;
            ensure!(
                response.status().is_success(),
                "Upload session creation failed (HTTP {})",
                response.status().as_u16()
            );
            record.session_uri = Some(
                response
                    .headers()
                    .get("Location")
                    .context("Upload session URI missing")?
                    .to_str()?
                    .into(),
            );
            save_record(&state_path, &record)?;
        }
        let mut source = std::fs::File::open(path)?;
        while record.video_id.is_none() && record.uploaded_bytes < size {
            source.seek(SeekFrom::Start(record.uploaded_bytes))?;
            let length = (size - record.uploaded_bytes).min(chunk_size as u64) as usize;
            let mut chunk = vec![0; length];
            source.read_exact(&mut chunk)?;
            let uri = record
                .session_uri
                .as_deref()
                .context("Upload session missing")?;
            let response = self
                .upload_request(Method::PUT, uri)?
                .header("Content-Type", "application/octet-stream")
                .header(
                    "Content-Range",
                    format!(
                        "bytes {}-{}/{}",
                        record.uploaded_bytes,
                        record.uploaded_bytes + length as u64 - 1,
                        size
                    ),
                )
                .body(chunk)
                .send()
                .map_err(|error| error.without_url())?;
            let (offset, completed) = Self::upload_result(response, size)?;
            ensure!(
                offset > record.uploaded_bytes,
                "No upload progress acknowledged; rerun to query session"
            );
            record.uploaded_bytes = offset;
            record.video_id = completed;
            save_record(&state_path, &record)?;
            eprintln!("Upload progress: {offset}/{size} bytes");
        }
        let video_id = record
            .video_id
            .clone()
            .context("Upload incomplete; rerun to query session")?;
        record.session_uri = None;
        save_record(&state_path, &record)?;
        let verified = self.video(&video_id)?;
        ensure!(
            verified["status"]["privacyStatus"] == options.privacy,
            "Upload completed but privacy differs; refresh inventory"
        );
        Ok(json!({"id":video_id,"already_uploaded":false,"video":verified}))
    }

    pub fn upload_from_storage(
        &mut self,
        video_bucket: &str,
        state_bucket: &str,
        record: &mut crate::cloud_storage::UploadRecord,
        mut generation: u64,
        audited: bool,
    ) -> Result<(Value, u64)> {
        ensure!(record.file_size > 0, "Video file is empty");
        ensure!(
            record.title.chars().count() <= 100 && !record.title.trim().is_empty(),
            "Title must contain 1 to 100 characters"
        );
        ensure!(
            record.description.len() <= 5000,
            "Description exceeds 5000 bytes"
        );
        ensure!(
            ["private", "unlisted", "public"].contains(&record.privacy.as_str()),
            "Invalid video privacy"
        );
        ensure!(
            record.privacy == "private" || audited,
            "Non-private uploads require administrator audit confirmation"
        );
        if record.video_id.is_none() {
            if record.youtube_session_uri.is_none() {
                let inventory = self.inventory()?;
                ensure!(
                    !inventory["videos"]
                        .as_array()
                        .context("Video inventory missing")?
                        .iter()
                        .any(|video| video["snippet"]["title"] == record.title),
                    "A video with this title exists; review it before uploading"
                );
                let uri = "https://www.googleapis.com/upload/youtube/v3/videos";
                let response = self
                    .upload_request(Method::POST, uri)?
                    .query(&[("uploadType", "resumable"), ("part", "snippet,status")])
                    .header("X-Upload-Content-Length", record.file_size.to_string())
                    .header("X-Upload-Content-Type", "application/octet-stream")
                    .json(&json!({"snippet":{"title":record.title,"description":record.description,"categoryId":"22"},"status":{"privacyStatus":record.privacy,"selfDeclaredMadeForKids":record.made_for_kids}}))
                    .send()
                    .map_err(|error| error.without_url())?;
                ensure!(
                    response.status().is_success(),
                    "Upload session creation failed (HTTP {})",
                    response.status().as_u16()
                );
                let session_uri = response
                    .headers()
                    .get("Location")
                    .context("Upload session URI missing")?
                    .to_str()?
                    .to_owned();
                record.youtube_session_uri = Some(session_uri);
                record.status = "uploading_to_youtube".into();
                generation = crate::cloud_storage::save_upload(state_bucket, record, generation)?;
            }
            let session_uri = record
                .youtube_session_uri
                .as_deref()
                .context("YouTube resumable session missing")?;
            let response = self
                .upload_request(Method::PUT, session_uri)?
                .header("Content-Length", "0")
                .header("Content-Range", format!("bytes */{}", record.file_size))
                .body(Vec::new())
                .send()
                .map_err(|error| error.without_url())?;
            let (offset, completed) = Self::upload_result(response, record.file_size)?;
            record.youtube_uploaded_bytes = offset;
            record.video_id = completed;
            generation = crate::cloud_storage::save_upload(state_bucket, record, generation)?;
        }
        while record.video_id.is_none() && record.youtube_uploaded_bytes < record.file_size {
            let first_byte = record.youtube_uploaded_bytes;
            let length = (record.file_size - first_byte).min(8 * 1024 * 1024) as usize;
            let chunk =
                crate::cloud_storage::read_video_range(video_bucket, record, first_byte, length)?;
            let session_uri = record
                .youtube_session_uri
                .as_deref()
                .context("YouTube resumable session missing")?;
            let response = self
                .upload_request(Method::PUT, session_uri)?
                .header("Content-Type", "application/octet-stream")
                .header(
                    "Content-Range",
                    format!(
                        "bytes {}-{}/{}",
                        first_byte,
                        first_byte + length as u64 - 1,
                        record.file_size
                    ),
                )
                .body(chunk)
                .send()
                .map_err(|error| error.without_url())?;
            let (offset, completed) = Self::upload_result(response, record.file_size)?;
            ensure!(
                offset > first_byte,
                "No YouTube upload progress acknowledged"
            );
            record.youtube_uploaded_bytes = offset;
            record.video_id = completed;
            generation = crate::cloud_storage::save_upload(state_bucket, record, generation)?;
        }
        let video_id = record
            .video_id
            .as_deref()
            .context("YouTube upload incomplete; retry the cloud job")?;
        record.youtube_session_uri = None;
        record.status = "youtube_uploaded".into();
        generation = crate::cloud_storage::save_upload(state_bucket, record, generation)?;
        let verified = self.video(video_id)?;
        ensure!(
            verified["status"]["privacyStatus"] == record.privacy,
            "YouTube upload completed but privacy could not be verified"
        );
        Ok((verified, generation))
    }

    fn request(&self, method: Method, resource: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}/{}", self.base_url, resource))
            .bearer_auth(&self.token)
    }

    fn get(&self, resource: &str, query: &[(&str, &str)]) -> Result<Value> {
        execute(self.request(Method::GET, resource).query(query))
    }

    fn pages(&self, resource: &str, query: &[(&str, &str)]) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut page = String::new();
        loop {
            let mut parameters = query.to_vec();
            parameters.push(("maxResults", "50"));
            if !page.is_empty() {
                parameters.push(("pageToken", &page));
            }
            let response = self.get(resource, &parameters)?;
            items.extend(response["items"].as_array().cloned().unwrap_or_default());
            match response["nextPageToken"].as_str() {
                Some(token) => page = token.to_owned(),
                None => return Ok(items),
            }
        }
    }

    pub fn playlists(&self) -> Result<Vec<Value>> {
        self.pages("playlists", &[("part", "snippet,status"), ("mine", "true")])
    }

    pub fn inventory(&self) -> Result<Value> {
        let response = self.get(
            "channels",
            &[("part", "snippet,contentDetails"), ("mine", "true")],
        )?;
        let channel = response["items"][0].clone();
        let uploads = channel["contentDetails"]["relatedPlaylists"]["uploads"]
            .as_str()
            .context("No YouTube channel available")?;
        let items = self.pages(
            "playlistItems",
            &[("part", "contentDetails"), ("playlistId", uploads)],
        )?;
        let ids: Vec<&str> = items
            .iter()
            .filter_map(|item| item["contentDetails"]["videoId"].as_str())
            .collect();
        let mut videos = Vec::new();
        for batch in ids.chunks(50) {
            let response = self.get(
                "videos",
                &[
                    ("part", "snippet,status,processingDetails"),
                    ("id", &batch.join(",")),
                ],
            )?;
            videos.extend(response["items"].as_array().cloned().unwrap_or_default());
        }
        Ok(json!({"channel": channel, "videos": videos, "playlists": self.playlists()?}))
    }

    pub fn create_playlist(&self, title: &str) -> Result<Value> {
        ensure!(
            !title.trim().is_empty() && title.chars().count() <= 150,
            "Playlist title must contain 1 to 150 characters"
        );
        ensure!(
            !self
                .playlists()?
                .iter()
                .any(|item| item["snippet"]["title"] == title),
            "Playlist title already exists; reuse an existing ID"
        );
        execute(
            self.request(Method::POST, "playlists")
                .query(&[("part", "snippet,status")])
                .json(&json!({"snippet":{"title":title},"status":{"privacyStatus":"private"}})),
        )
    }

    pub fn add_to_playlist(&self, playlist_id: &str, video_id: &str) -> Result<bool> {
        let items = self.pages(
            "playlistItems",
            &[("part", "contentDetails"), ("playlistId", playlist_id)],
        )?;
        if items
            .iter()
            .any(|item| item["contentDetails"]["videoId"] == video_id)
        {
            return Ok(false);
        }
        execute(self.request(Method::POST, "playlistItems").query(&[("part", "snippet")])
            .json(&json!({"snippet":{"playlistId":playlist_id,"resourceId":{"kind":"youtube#video","videoId":video_id}}})))?;
        Ok(true)
    }

    pub fn set_playlist_privacy(&self, playlist_id: &str, privacy: &str) -> Result<Value> {
        ensure!(
            ["private", "unlisted", "public"].contains(&privacy),
            "Invalid privacy setting"
        );
        let response = self.get(
            "playlists",
            &[("part", "snippet,status"), ("id", playlist_id)],
        )?;
        let current = response["items"][0]["snippet"]
            .as_object()
            .context("Playlist not found")?;
        let fields = ["title", "description", "defaultLanguage"];
        let snippet: serde_json::Map<String, Value> = current
            .iter()
            .filter(|(key, _)| fields.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        execute(
            self.request(Method::PUT, "playlists")
                .query(&[("part", "snippet,status")])
                .json(
                    &json!({"id":playlist_id,"snippet":snippet,"status":{"privacyStatus":privacy}}),
                ),
        )?;
        let verified = self.get(
            "playlists",
            &[("part", "snippet,status"), ("id", playlist_id)],
        )?;
        ensure!(
            verified["items"][0]["status"]["privacyStatus"] == privacy,
            "Playlist privacy change not yet confirmed; refresh inventory before retrying"
        );
        Ok(verified)
    }

    pub fn video(&self, video_id: &str) -> Result<Value> {
        let response = execute(self.request(Method::GET, "videos").query(&[
            ("part", "snippet,status,processingDetails"),
            ("id", video_id),
        ]))?;
        response["items"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .context("Video not found or inaccessible")
    }

    pub fn set_privacy(&self, video_id: &str, privacy: &str) -> Result<Value> {
        ensure!(
            ["private", "unlisted", "public"].contains(&privacy),
            "Invalid privacy setting"
        );
        let current = self.video(video_id)?;
        let status = current["status"]
            .as_object()
            .context("Video status missing")?;
        let writable = [
            "privacyStatus",
            "license",
            "embeddable",
            "publicStatsViewable",
            "publishAt",
            "selfDeclaredMadeForKids",
            "containsSyntheticMedia",
        ];
        let mut editable: serde_json::Map<String, Value> = status
            .iter()
            .filter(|(key, _)| writable.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if privacy != "private" {
            editable.remove("publishAt");
        }
        editable.insert("privacyStatus".into(), json!(privacy));
        execute(
            self.request(Method::PUT, "videos")
                .query(&[("part", "status")])
                .json(&json!({"id":video_id,"status":editable})),
        )?;
        let verified = self.video(video_id)?;
        ensure!(
            verified["status"]["privacyStatus"] == privacy,
            "Requested privacy not yet confirmed; actual status is {}",
            verified["status"]["privacyStatus"]
        );
        Ok(verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_http::{Header, Response, Server};

    #[test]
    fn interrupted_upload_resumes_from_server_and_completed_upload_is_not_repeated() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("test.mp4");
        std::fs::write(&file, vec![7; 524288]).unwrap();
        let state_dir = directory.path().join("state");
        let options = UploadOptions {
            title: "synthetic test".into(),
            description: String::new(),
            privacy: "private".into(),
            made_for_kids: false,
        };
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let session_uri = format!("{base}/session");
        let mut api = YouTube {
            http: Client::new(),
            token: "fake-token".into(),
            base_url: base,
            token_path: None,
            refresh_credentials: None,
            production_upload: false,
            refreshed: Instant::now(),
        };
        let handle = std::thread::spawn(move || {
            let receive = || {
                server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("Expected mock request")
            };
            receive()
                .respond(Response::from_string(
                    r#"{"items":[{"contentDetails":{"relatedPlaylists":{"uploads":"uploads"}}}]}"#,
                ))
                .unwrap();
            receive()
                .respond(Response::from_string(r#"{"items":[]}"#))
                .unwrap();
            receive()
                .respond(Response::from_string(r#"{"items":[]}"#))
                .unwrap();
            let mut request = receive();
            assert_eq!(request.method().as_str(), "POST");
            let metadata: Value = serde_json::from_reader(request.as_reader()).unwrap();
            assert_eq!(metadata["status"]["privacyStatus"], "private");
            request
                .respond(
                    Response::from_string("{}")
                        .with_header(Header::from_bytes("Location", session_uri).unwrap()),
                )
                .unwrap();
            let request = receive();
            assert!(
                request
                    .headers()
                    .iter()
                    .any(|header| header.field.equiv("Content-Range")
                        && header.value.as_str() == "bytes 0-262143/524288")
            );
            request
                .respond(
                    Response::empty(308)
                        .with_header(Header::from_bytes("Range", "bytes=0-262143").unwrap()),
                )
                .unwrap();
            receive().respond(Response::empty(503)).unwrap();
            let request = receive();
            assert!(
                request
                    .headers()
                    .iter()
                    .any(|header| header.field.equiv("Content-Range")
                        && header.value.as_str() == "bytes */524288")
            );
            request
                .respond(
                    Response::empty(308)
                        .with_header(Header::from_bytes("Range", "bytes=0-262143").unwrap()),
                )
                .unwrap();
            let mut request = receive();
            assert!(
                request
                    .headers()
                    .iter()
                    .any(|header| header.field.equiv("Content-Range")
                        && header.value.as_str() == "bytes 262144-524287/524288")
            );
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            assert_eq!(body, vec![7; 262144]);
            request
                .respond(Response::from_string(r#"{"id":"finished"}"#).with_status_code(201))
                .unwrap();
            receive()
                .respond(Response::from_string(
                    r#"{"items":[{"id":"finished","status":{"privacyStatus":"private"}}]}"#,
                ))
                .unwrap();
        });
        assert!(
            api.upload(&file, options.clone(), &state_dir, false, 262144)
                .is_err()
        );
        let (digest, _) = fingerprint(&file).unwrap();
        let state_path = state_dir.join(format!("{digest}.json"));
        let mut saved: UploadRecord =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(saved.uploaded_bytes, 262144);
        saved.uploaded_bytes = 0;
        save_record(&state_path, &saved).unwrap();
        assert_eq!(
            api.upload(&file, options.clone(), &state_dir, false, 262144)
                .unwrap()["id"],
            "finished"
        );
        handle.join().unwrap();
        assert_eq!(
            api.upload(&file, options, &state_dir, false, 262144)
                .unwrap()["already_uploaded"],
            true
        );
        let saved: UploadRecord =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(saved.uploaded_bytes, 524288);
        assert!(saved.session_uri.is_none());
        assert!(file.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn upload_rejects_unconfirmed_audit_before_network_calls() {
        let mut api = YouTube {
            http: Client::new(),
            token: "fake-token".into(),
            base_url: "http://127.0.0.1:1".into(),
            token_path: None,
            refresh_credentials: None,
            production_upload: false,
            refreshed: Instant::now(),
        };
        let options = UploadOptions {
            title: "test".into(),
            description: String::new(),
            privacy: "unlisted".into(),
            made_for_kids: false,
        };
        let error = api
            .upload(
                Path::new("missing.mp4"),
                options,
                Path::new("unused"),
                false,
                262144,
            )
            .unwrap_err();
        assert!(error.to_string().contains("audit confirmation"));
    }

    #[test]
    fn upload_session_cannot_leak_token_to_another_host() {
        let mut api = YouTube {
            http: Client::new(),
            token: "fake-token".into(),
            base_url: "https://www.googleapis.com/youtube/v3".into(),
            token_path: None,
            refresh_credentials: None,
            production_upload: false,
            refreshed: Instant::now(),
        };
        assert!(
            api.upload_request(Method::PUT, "https://attacker.example/session")
                .is_err()
        );
        assert!(
            api.upload_request(Method::PUT, "http://www.googleapis.com/session")
                .is_err()
        );
        assert!(
            api.upload_request(Method::PUT, "https://www.googleapis.com/session")
                .is_ok()
        );
    }

    #[test]
    fn playlists_include_all_pages() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let api = YouTube {
            http: Client::new(),
            token: "fake-token".into(),
            base_url: format!("http://{}", server.server_addr()),
            token_path: None,
            refresh_credentials: None,
            production_upload: false,
            refreshed: Instant::now(),
        };
        let handle = std::thread::spawn(move || {
            server
                .recv()
                .unwrap()
                .respond(Response::from_string(
                    r#"{"items":[{"id":"first"}],"nextPageToken":"next"}"#,
                ))
                .unwrap();
            let request = server.recv().unwrap();
            assert!(request.url().contains("pageToken=next"));
            request
                .respond(Response::from_string(r#"{"items":[{"id":"second"}]}"#))
                .unwrap();
        });
        assert_eq!(api.playlists().unwrap().len(), 2);
        handle.join().unwrap();
    }

    #[test]
    fn duplicate_membership_does_not_insert() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let api = YouTube {
            http: Client::new(),
            token: "fake-token".into(),
            base_url: format!("http://{}", server.server_addr()),
            token_path: None,
            refresh_credentials: None,
            production_upload: false,
            refreshed: Instant::now(),
        };
        let handle = std::thread::spawn(move || {
            server
                .recv()
                .unwrap()
                .respond(Response::from_string(
                    r#"{"items":[{"contentDetails":{"videoId":"clip"}}]}"#,
                ))
                .unwrap();
        });
        assert!(!api.add_to_playlist("playlist", "clip").unwrap());
        handle.join().unwrap();
    }

    #[test]
    fn privacy_update_keeps_writable_fields_and_verifies_result() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let api = YouTube {
            http: Client::new(),
            token: "fake-token".into(),
            base_url: format!("http://{}", server.server_addr()),
            token_path: None,
            refresh_credentials: None,
            production_upload: false,
            refreshed: Instant::now(),
        };

        let handle = std::thread::spawn(move || {
            server.recv().unwrap().respond(Response::from_string(r#"{"items":[{"status":{"privacyStatus":"private","license":"youtube","selfDeclaredMadeForKids":false,"uploadStatus":"processed"}}]}"#)).unwrap();
            let mut request = server.recv().unwrap();
            assert_eq!(request.method().as_str(), "PUT");
            let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
            assert_eq!(body["status"]["privacyStatus"], "unlisted");
            assert_eq!(body["status"]["license"], "youtube");
            assert_eq!(body["status"]["selfDeclaredMadeForKids"], false);
            assert!(body["status"].get("uploadStatus").is_none());
            request.respond(Response::from_string("{}")).unwrap();
            server
                .recv()
                .unwrap()
                .respond(Response::from_string(
                    r#"{"items":[{"status":{"privacyStatus":"unlisted"}}]}"#,
                ))
                .unwrap();
        });
        assert_eq!(
            api.set_privacy("clip", "unlisted").unwrap()["status"]["privacyStatus"],
            "unlisted"
        );
        handle.join().unwrap();
    }
}
