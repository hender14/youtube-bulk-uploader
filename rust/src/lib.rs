use anyhow::{Context, Result, ensure};

pub mod cloud_storage;
pub mod web;
pub mod youtube;

pub fn acknowledged_offset(range: Option<&str>, file_size: u64) -> Result<u64> {
    let Some(range) = range else {
        return Ok(0);
    };
    let last_byte = range
        .strip_prefix("bytes=0-")
        .context("Invalid resumable upload Range prefix")?
        .parse::<u64>()
        .context("Invalid resumable upload Range offset")?;
    let offset = last_byte.checked_add(1).context("Upload offset overflow")?;
    ensure!(offset <= file_size, "Upload offset exceeds file size");
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_returns_next_acknowledged_byte() {
        assert_eq!(
            acknowledged_offset(Some("bytes=0-262143"), 524288).unwrap(),
            262144
        );
    }

    #[test]
    fn missing_range_means_zero_bytes() {
        assert_eq!(acknowledged_offset(None, 524288).unwrap(), 0);
    }

    #[test]
    fn invalid_or_out_of_bounds_range_is_rejected() {
        for range in [
            "bytes=unknown",
            "bytes=1-2",
            "bytes=0-524288",
            "bytes=0-18446744073709551615",
        ] {
            assert!(acknowledged_offset(Some(range), 524288).is_err());
        }
    }
}
