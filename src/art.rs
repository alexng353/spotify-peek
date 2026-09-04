//! On-disk cache for album art.
//!
//! MPRIS hands out a remote `i.scdn.co` URL, so the first sight of a track
//! costs one download; every hover after that is a `stat()`.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

use crate::api::cache_dir;

/// Cap on a downloaded image, in case the URL isn't what we think it is.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

fn art_dir() -> PathBuf {
    cache_dir().join("art")
}

/// The last path segment of a Spotify art URL is already a content hash, so it
/// doubles as the cache key. Anything unexpected in it disqualifies the URL.
fn cache_path(url: &str) -> Option<PathBuf> {
    let name = url.rsplit('/').next()?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(art_dir().join(name))
}

pub fn cached(url: &str) -> Option<PathBuf> {
    cache_path(url).filter(|p| p.is_file())
}

pub fn fetch(url: &str) -> Result<PathBuf, String> {
    let path = cache_path(url).ok_or("unexpected art url")?;
    if path.is_file() {
        return Ok(path);
    }

    let mut bytes = Vec::new();
    crate::api::agent()
        .get(url)
        .call()
        .map_err(|e| format!("art: {e}"))?
        .body_mut()
        .as_reader()
        .take(MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("art: {e}"))?;

    fs::create_dir_all(art_dir()).map_err(|e| format!("art: {e}"))?;
    // Write beside the target and rename, so a hover that races a half-written
    // file can't load a truncated image.
    let tmp = path.with_extension("part");
    fs::write(&tmp, &bytes).map_err(|e| format!("art: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("art: {e}"))?;
    Ok(path)
}
