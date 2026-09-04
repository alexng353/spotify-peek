//! Spotify Web API access, for the things MPRIS can't answer: the up-next list
//! and whether the current track is in the user's library.
//!
//! This rides on the OAuth app `spotlike` already has registered: its client
//! credentials and refresh token are read from disk but never written back. Our
//! own access token lives in a separate cache so the two tools can't clobber
//! each other.
//!
//! Every call here happens on a worker thread, only while the popup is open.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::Value;

const QUEUE_URL: &str = "https://api.spotify.com/v1/me/player/queue";
const HISTORY_URL: &str = "https://api.spotify.com/v1/me/player/recently-played?limit=1";
const SAVED_URL: &str = "https://api.spotify.com/v1/me/tracks";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// Treat a token expiring within this window as already dead.
const EXPIRY_SLACK: u64 = 60;

#[derive(Clone, Default)]
pub struct Track {
    /// Bare base-62 id. Empty for anything Spotify doesn't give one for (local
    /// files, some podcast items), which [`Track::is`] treats as "never equal".
    pub id: String,
    pub title: String,
    pub artist: String,
}

impl Track {
    /// Identity comparison that refuses to match on an absent id.
    pub fn is(&self, id: &str) -> bool {
        !self.id.is_empty() && self.id == id
    }
}

pub struct Queue {
    /// What the Web API thinks is playing. Worth having because Spotify's MPRIS
    /// interface only publishes the *primary* artist, so "Forever" arrives over
    /// D-Bus as just "Drake".
    pub now: Option<Track>,
    pub next: Vec<Track>,
}

type Res<T> = Result<T, String>;

/// One agent for the whole process, so repeated hovers reuse the pooled TLS
/// connection to `api.spotify.com` instead of paying for a fresh handshake on
/// every lookup.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(ureq::Agent::new_with_defaults)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

pub fn cache_dir() -> PathBuf {
    home().join(".cache/spotify-peek")
}

fn token_cache() -> PathBuf {
    cache_dir().join("token.json")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_json(path: PathBuf) -> Res<Value> {
    let raw = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))
}

/// `spotlike`'s env file is plain `KEY=VALUE` lines.
fn client_credentials() -> Res<(String, String)> {
    let path = home().join(".config/spotlike/env");
    let raw = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut id = None;
    let mut secret = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        match line.split_once('=') {
            Some(("RSPOTIFY_CLIENT_ID", v)) => id = Some(v.to_string()),
            Some(("RSPOTIFY_CLIENT_SECRET", v)) => secret = Some(v.to_string()),
            _ => {}
        }
    }
    match (id, secret) {
        (Some(i), Some(s)) => Ok((i, s)),
        _ => Err("no client credentials in spotlike env".into()),
    }
}

fn our_cached_token() -> Option<String> {
    let v = read_json(token_cache()).ok()?;
    let expires_at = v.get("expires_at")?.as_u64()?;
    if expires_at > now() + EXPIRY_SLACK {
        Some(v.get("access_token")?.as_str()?.to_string())
    } else {
        None
    }
}

fn store_token(access_token: &str, expires_in: u64) {
    let body = serde_json::json!({
        "access_token": access_token,
        "expires_at": now() + expires_in,
    });
    let _ = fs::create_dir_all(cache_dir());
    let _ = fs::write(token_cache(), body.to_string());
}

/// Spend `spotlike`'s refresh token for a fresh access token of our own.
fn refresh() -> Res<String> {
    let (id, secret) = client_credentials()?;
    let stored = read_json(home().join(".local/share/spotlike/token.json"))?;
    let refresh_token = stored
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or("no refresh token in spotlike token.json")?;

    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    let form = format!("grant_type=refresh_token&refresh_token={refresh_token}");
    let body = agent()
        .post(TOKEN_URL)
        .header("Authorization", format!("Basic {basic}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(form)
        .map_err(|e| format!("token refresh: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("token refresh: {e}"))?;

    let v: Value = serde_json::from_str(&body).map_err(|e| format!("token refresh: {e}"))?;
    let access = v
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("token refresh: no access_token in response")?;
    let expires_in = v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
    store_token(access, expires_in);
    Ok(access.to_string())
}

fn access_token() -> Res<String> {
    if let Some(t) = our_cached_token() {
        return Ok(t);
    }
    // spotlike's own access token may still be live — borrow it rather than
    // spending a refresh.
    if let Ok(v) = read_json(home().join(".local/share/spotlike/token.json")) {
        let obtained = v.get("obtained_at").and_then(Value::as_u64);
        let ttl = v.get("expires_in").and_then(Value::as_u64);
        let token = v.get("access_token").and_then(Value::as_str);
        if let (Some(obtained), Some(ttl), Some(token)) = (obtained, ttl, token)
            && obtained + ttl > now() + EXPIRY_SLACK
        {
            store_token(token, obtained + ttl - now());
            return Ok(token.to_string());
        }
    }
    refresh()
}

/// Run an authenticated request. A 401 means the token was revoked or rotated
/// out from under us, so force one refresh and retry before giving up.
fn authed<T>(what: &str, call: impl Fn(&str) -> Result<T, ureq::Error>) -> Res<T> {
    match call(&access_token()?) {
        Ok(value) => Ok(value),
        Err(ureq::Error::StatusCode(401)) => call(&refresh()?).map_err(|e| format!("{what}: {e}")),
        Err(e) => Err(format!("{what}: {e}")),
    }
}

fn artists_of(item: &Value) -> String {
    item.get("artists")
        .and_then(Value::as_array)
        .map(|artists| {
            artists
                .iter()
                .filter_map(|a| a.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn parse_track(item: &Value) -> Track {
    Track {
        id: item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        artist: artists_of(item),
    }
}

fn parse_queue(body: &str, limit: usize) -> Res<Queue> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("queue: {e}"))?;
    let items = v
        .get("queue")
        .and_then(Value::as_array)
        .ok_or("queue: no queue in response")?;

    Ok(Queue {
        now: v
            .get("currently_playing")
            .filter(|c| !c.is_null())
            .map(parse_track),
        next: items.iter().take(limit).map(parse_track).collect(),
    })
}

fn parse_recently_played(body: &str) -> Res<Option<Track>> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("history: {e}"))?;
    Ok(v.get("items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("track"))
        .map(parse_track))
}

/// The track played before the current one, for the `prev` end of the window.
///
/// Only needed when local history can't supply it — normally the track we just
/// left *is* the previous one, for free.
pub fn previously_played() -> Res<Option<Track>> {
    let body = authed("history", |token| {
        agent()
            .get(HISTORY_URL)
            .header("Authorization", format!("Bearer {token}"))
            .call()?
            .body_mut()
            .read_to_string()
    })?;
    parse_recently_played(&body)
}

/// What Spotify is playing and the next `limit` tracks. The queue comes back
/// empty when no device holds an active playback session.
pub fn queue(limit: usize) -> Res<Queue> {
    let body = authed("queue", |token| {
        agent()
            .get(QUEUE_URL)
            .header("Authorization", format!("Bearer {token}"))
            .call()?
            .body_mut()
            .read_to_string()
    })?;
    parse_queue(&body, limit)
}

/// Whether `track_id` (a bare Spotify base-62 id) is in the user's library.
pub fn is_liked(track_id: &str) -> Res<bool> {
    let url = format!("{SAVED_URL}/contains?ids={track_id}");
    let body = authed("liked", |token| {
        agent()
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .call()?
            .body_mut()
            .read_to_string()
    })?;
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get(0).and_then(Value::as_bool))
        .ok_or_else(|| format!("liked: unexpected response {body}"))
}

pub fn set_liked(track_id: &str, liked: bool) -> Res<()> {
    let url = format!("{SAVED_URL}?ids={track_id}");
    authed("set liked", |token| {
        let auth = format!("Bearer {token}");
        if liked {
            agent().put(&url).header("Authorization", auth).send_empty()
        } else {
            agent().delete(&url).header("Authorization", auth).call()
        }
        .map(|_| ())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape copied from a live `GET /v1/me/player/queue` response.
    const SAMPLE: &str = r#"{
        "currently_playing": {
            "id": "4wpXFtC0lIeXdMPXQvKmuU",
            "name": "Forever",
            "artists": [
                { "name": "Drake" }, { "name": "Kanye West" },
                { "name": "Lil Wayne" }, { "name": "Eminem" }
            ]
        },
        "queue": [
            { "id": "q1", "name": "there he go", "artists": [{ "name": "sosocamo" }] },
            { "id": "q2", "name": "DIE TRYING", "artists": [{ "name": "PARTYNEXTDOOR" }, { "name": "Yebba" }] },
            { "id": "q3", "name": "LORD LIFT ME UP", "artists": [{ "name": "Ye" }] },
            { "id": "q4", "name": "SKY CITY", "artists": [{ "name": "Drake" }] }
        ]
    }"#;

    /// Shape copied from a live `GET /v1/me/player/recently-played` response.
    const HISTORY: &str = r#"{
        "items": [
            { "track": { "id": "h1", "name": "SUZY", "artists": [{ "name": "DONDA" }, { "name": "Ye" }] } }
        ]
    }"#;

    #[test]
    fn reads_titles_and_joins_artists() {
        let queue = parse_queue(SAMPLE, 3).unwrap();
        assert_eq!(queue.next.len(), 3, "limit is respected");
        assert_eq!(queue.next[0].title, "there he go");
        assert_eq!(queue.next[0].artist, "sosocamo");
        assert_eq!(queue.next[1].artist, "PARTYNEXTDOOR, Yebba");
    }

    /// The reason this endpoint is worth parsing at all: MPRIS would only give
    /// us "Drake" for this track.
    #[test]
    fn now_playing_keeps_every_artist() {
        let now = parse_queue(SAMPLE, 3).unwrap().now.unwrap();
        assert_eq!(now.id, "4wpXFtC0lIeXdMPXQvKmuU");
        assert_eq!(now.artist, "Drake, Kanye West, Lil Wayne, Eminem");
    }

    #[test]
    fn empty_queue_is_not_an_error() {
        let queue = parse_queue(r#"{"queue": [], "currently_playing": null}"#, 3).unwrap();
        assert!(queue.next.is_empty());
        assert!(queue.now.is_none());
    }

    #[test]
    fn missing_queue_is_an_error() {
        assert!(parse_queue(r#"{"error": {"status": 403}}"#, 3).is_err());
    }

    #[test]
    fn reads_the_previous_track_from_history() {
        let prev = parse_recently_played(HISTORY).unwrap().unwrap();
        assert_eq!(prev.id, "h1");
        assert_eq!(prev.title, "SUZY");
        assert_eq!(prev.artist, "DONDA, Ye");
    }

    #[test]
    fn empty_history_is_not_an_error() {
        assert!(parse_recently_played(r#"{"items": []}"#).unwrap().is_none());
    }

    /// Local files and some podcast items arrive without an id; those must
    /// never be treated as matching another track.
    #[test]
    fn tracks_without_an_id_never_match() {
        let anonymous = Track::default();
        assert!(!anonymous.is(""));
        assert!(!anonymous.is("q1"));
        assert!(parse_queue(SAMPLE, 4).unwrap().next[0].is("q1"));
    }
}
