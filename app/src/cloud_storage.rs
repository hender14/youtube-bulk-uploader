use anyhow::{Context, Result, ensure};
use reqwest::Method;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

const MAX_STATE_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UploadRecord {
    pub id: String,
    pub owner_email: String,
    pub object_name: String,
    pub session_uri: String,
    pub file_size: u64,
    pub content_type: String,
    pub original_name: String,
    pub title: String,
    pub description: String,
    pub privacy: String,
    pub made_for_kids: bool,
    pub playlist_id: Option<String>,
    #[serde(default)]
    pub youtube_session_uri: Option<String>,
    #[serde(default)]
    pub youtube_uploaded_bytes: u64,
    #[serde(default)]
    pub video_id: Option<String>,
    #[serde(default)]
    pub status: String,
}

pub struct StoredUpload {
    pub record: UploadRecord,
    pub generation: u64,
}

fn http() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()?)
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 43
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'),
        "Invalid upload identifier"
    );
    Ok(())
}

fn validate_session_uri(uri: &str) -> Result<Url> {
    let url = Url::parse(uri).context("Invalid Cloud Storage resumable session")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("storage.googleapis.com")
            && url.username().is_empty()
            && url.password().is_none(),
        "Refusing untrusted Cloud Storage session endpoint"
    );
    Ok(url)
}

fn browser_origin(redirect_uri: &str) -> Result<String> {
    let url = Url::parse(redirect_uri).context("Invalid OAuth redirect URL")?;
    ensure!(
        url.scheme() == "https" || url.host_str() == Some("localhost"),
        "Browser upload origin must use HTTPS except on localhost"
    );
    Ok(url.origin().ascii_serialization())
}

fn object_url(bucket: &str, object_name: &str) -> Result<Url> {
    let mut url = Url::parse("https://storage.googleapis.com/storage/v1/")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid Cloud Storage URL"))?
        .pop_if_empty()
        .extend(["b", bucket, "o", object_name]);
    Ok(url)
}

fn upload_collection_url(bucket: &str) -> Result<Url> {
    let mut url = Url::parse("https://storage.googleapis.com/upload/storage/v1/")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid Cloud Storage upload URL"))?
        .pop_if_empty()
        .extend(["b", bucket, "o"]);
    Ok(url)
}

fn session_record(record: &UploadRecord) -> Result<()> {
    validate_id(&record.id)?;
    validate_session_uri(&record.session_uri)?;
    ensure!(record.object_name == format!("incoming/{}", record.id));
    Ok(())
}

fn save_record(bucket: &str, record: &UploadRecord, generation: Option<u64>) -> Result<u64> {
    session_record(record)?;
    let token = crate::web::metadata_access_token()?;
    let mut url = upload_collection_url(bucket)?;
    url.query_pairs_mut()
        .append_pair("uploadType", "media")
        .append_pair("name", &format!("uploads/{}.json", record.id))
        .append_pair(
            "ifGenerationMatch",
            &generation.map_or_else(|| "0".into(), |value| value.to_string()),
        );
    let response = http()?
        .request(Method::POST, url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(record)?)
        .send()
        .context("Unable to store upload state")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        let detail = body.chars().take(1024).collect::<String>();
        anyhow::bail!("Cloud Storage rejected upload state with HTTP {status}: {detail}");
    }
    let saved: Value = response.json().context("Invalid upload-state response")?;
    saved["generation"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .context("Cloud Storage omitted upload-state generation")
}

fn start_session(bucket: &str, record: &UploadRecord, origin: &str) -> Result<String> {
    validate_id(&record.id)?;
    ensure!(record.file_size > 0, "Video file is empty");
    ensure!(
        record.content_type.starts_with("video/"),
        "Unsupported video content type"
    );
    let token = crate::web::metadata_access_token()?;
    let mut url = upload_collection_url(bucket)?;
    url.query_pairs_mut()
        .append_pair("uploadType", "resumable")
        .append_pair("name", &record.object_name)
        .append_pair("ifGenerationMatch", "0");
    let response = http()?
        .post(url)
        .bearer_auth(token)
        .header("Origin", origin)
        .header("X-Upload-Content-Type", &record.content_type)
        .header("X-Upload-Content-Length", record.file_size)
        .json(&json!({"name":record.object_name,"contentType":record.content_type}))
        .send()
        .context("Unable to start Cloud Storage upload")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        let detail = body.chars().take(1024).collect::<String>();
        anyhow::bail!("Cloud Storage rejected upload setup with HTTP {status}: {detail}");
    }
    let location = response
        .headers()
        .get("Location")
        .context("Cloud Storage omitted the resumable session URI")?
        .to_str()?;
    validate_session_uri(location)?;
    Ok(location.to_owned())
}

pub fn begin_upload(
    video_bucket: &str,
    state_bucket: &str,
    mut record: UploadRecord,
    redirect_uri: &str,
) -> Result<UploadRecord> {
    validate_id(&record.id)?;
    ensure!(record.object_name == format!("incoming/{}", record.id));
    let origin = browser_origin(redirect_uri)?;
    record.session_uri = start_session(video_bucket, &record, &origin)?;
    save_record(state_bucket, &record, None)?;
    Ok(record)
}

pub fn load_upload_versioned(bucket: &str, id: &str) -> Result<StoredUpload> {
    validate_id(id)?;
    let token = crate::web::metadata_access_token()?;
    let mut url = object_url(bucket, &format!("uploads/{id}.json"))?;
    let response = http()?
        .get(url.clone())
        .bearer_auth(token)
        .send()
        .context("Unable to read upload state")?;
    ensure!(
        response.status().is_success(),
        "Upload state is unavailable"
    );
    let metadata: Value = response.json().context("Invalid upload-state metadata")?;
    let generation = metadata["generation"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok())
        .context("Upload-state generation missing")?;
    url.query_pairs_mut().append_pair("alt", "media");
    let token = crate::web::metadata_access_token()?;
    let response = http()?
        .get(url)
        .bearer_auth(token)
        .send()
        .context("Unable to read upload state")?;
    ensure!(
        response.status().is_success(),
        "Upload state is unavailable"
    );
    let bytes = response.bytes()?;
    ensure!(
        bytes.len() <= MAX_STATE_BYTES,
        "Upload state exceeds size limit"
    );
    let record: UploadRecord = serde_json::from_slice(&bytes)?;
    ensure!(record.id == id, "Upload state identifier mismatch");
    session_record(&record)?;
    Ok(StoredUpload { record, generation })
}

pub fn load_upload(bucket: &str, id: &str) -> Result<UploadRecord> {
    Ok(load_upload_versioned(bucket, id)?.record)
}

pub fn save_upload(bucket: &str, record: &UploadRecord, generation: u64) -> Result<u64> {
    save_record(bucket, record, Some(generation))
}

pub fn read_video_range(
    bucket: &str,
    record: &UploadRecord,
    first_byte: u64,
    byte_count: usize,
) -> Result<Vec<u8>> {
    ensure!(byte_count > 0, "Video range cannot be empty");
    let last_byte = first_byte
        .checked_add(byte_count as u64 - 1)
        .context("Video range overflow")?;
    ensure!(
        last_byte < record.file_size,
        "Video range exceeds object size"
    );
    let token = crate::web::metadata_access_token()?;
    let mut url = object_url(bucket, &record.object_name)?;
    url.query_pairs_mut().append_pair("alt", "media");
    let response = http()?
        .get(url)
        .bearer_auth(token)
        .header("Range", format!("bytes={first_byte}-{last_byte}"))
        .send()
        .context("Unable to read video chunk from Cloud Storage")?;
    ensure!(
        response.status().as_u16() == 206,
        "Cloud Storage did not honor video range"
    );
    let bytes = response.bytes()?;
    ensure!(
        bytes.len() == byte_count,
        "Cloud Storage returned a partial video chunk"
    );
    Ok(bytes.to_vec())
}

pub fn delete_video_object(bucket: &str, record: &UploadRecord) -> Result<()> {
    let token = crate::web::metadata_access_token()?;
    let url = object_url(bucket, &record.object_name)?;
    let metadata = http()?
        .get(url.clone())
        .bearer_auth(&token)
        .send()
        .context("Unable to verify staged video before deletion")?;
    if metadata.status().as_u16() == 404 {
        return Ok(());
    }
    ensure!(
        metadata.status().is_success(),
        "Staged video is unavailable"
    );
    let object: Value = metadata.json().context("Invalid staged object metadata")?;
    ensure!(
        object["name"].as_str() == Some(&record.object_name)
            && object["size"]
                .as_str()
                .and_then(|size| size.parse::<u64>().ok())
                == Some(record.file_size),
        "Refusing to delete an unexpected staged object"
    );
    let generation = object["generation"]
        .as_str()
        .context("Staged object generation missing")?;
    let mut delete_url = url;
    delete_url
        .query_pairs_mut()
        .append_pair("ifGenerationMatch", generation);
    let response = http()?
        .delete(delete_url)
        .bearer_auth(token)
        .send()
        .context("Unable to delete completed staged video")?;
    ensure!(
        response.status().is_success(),
        "Cloud Storage refused staged-video deletion"
    );
    Ok(())
}

pub fn acknowledged_bytes(record: &UploadRecord) -> Result<(u64, bool)> {
    session_record(record)?;
    let response = http()?
        .put(&record.session_uri)
        .header("Content-Length", "0")
        .header("Content-Range", format!("bytes */{}", record.file_size))
        .body(Vec::new())
        .send()
        .context("Unable to query Cloud Storage upload progress")?;
    if response.status().as_u16() == 308 {
        let range = response
            .headers()
            .get("Range")
            .map(|value| value.to_str())
            .transpose()?;
        return Ok((crate::acknowledged_offset(range, record.file_size)?, false));
    }
    ensure!(
        response.status().is_success(),
        "Cloud Storage upload session is unavailable"
    );
    let object: Value = response
        .json()
        .context("Invalid Cloud Storage completion response")?;
    ensure!(
        object["name"].as_str() == Some(&record.object_name)
            && object["size"]
                .as_str()
                .and_then(|size| size.parse::<u64>().ok())
                == Some(record.file_size),
        "Cloud Storage object does not match the upload request"
    );
    Ok((record.file_size, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> UploadRecord {
        UploadRecord {
            id: "A".repeat(43),
            owner_email: "owner@example.test".into(),
            object_name: format!("incoming/{}", "A".repeat(43)),
            session_uri:
                "https://storage.googleapis.com/upload/storage/v1/b/example/o?upload_id=opaque"
                    .into(),
            file_size: 1024,
            content_type: "video/mp4".into(),
            original_name: "clip.mp4".into(),
            title: "clip".into(),
            description: String::new(),
            privacy: "private".into(),
            made_for_kids: false,
            playlist_id: None,
            youtube_session_uri: None,
            youtube_uploaded_bytes: 0,
            video_id: None,
            status: "uploading_to_storage".into(),
        }
    }

    #[test]
    fn upload_state_has_random_safe_identity_and_trusted_session_origin() {
        let value = record();
        session_record(&value).unwrap();
        for uri in [
            "http://storage.googleapis.com/upload",
            "https://storage.googleapis.com.evil.test/upload",
            "https://evil.test/upload",
            "https://user@storage.googleapis.com/upload",
        ] {
            assert!(validate_session_uri(uri).is_err());
        }
        assert!(validate_id("../upload".repeat(5).as_str()).is_err());
    }

    #[test]
    fn state_object_urls_escape_path_segments() {
        let url = object_url("video-bucket", "uploads/abc.json").unwrap();
        assert_eq!(
            url.path(),
            "/storage/v1/b/video-bucket/o/uploads%2Fabc.json"
        );
        assert!(url.as_str().contains("uploads%2Fabc.json"));
    }

    #[test]
    fn upload_collection_url_has_no_duplicate_slash() {
        let url = upload_collection_url("video-bucket").unwrap();
        assert_eq!(url.path(), "/upload/storage/v1/b/video-bucket/o");
    }

    #[test]
    fn browser_origin_uses_only_the_oauth_url_origin() {
        assert_eq!(
            browser_origin("https://uploader.example.test/oauth/callback").unwrap(),
            "https://uploader.example.test"
        );
    }
}
