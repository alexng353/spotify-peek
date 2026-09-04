//! Minimal MPRIS client for the Spotify desktop client.
//!
//! Deliberately uncached: a refresh is one `GetAll` round trip, and it only
//! happens while the popup is open. No `PropertiesChanged` subscription, no
//! background polling, no `playerctl` subprocess — a closed popup costs nothing.

use std::collections::HashMap;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedValue};

const DEST: &str = "org.mpris.MediaPlayer2.spotify";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const PROPS: &str = "org.freedesktop.DBus.Properties";

/// Everything the card needs, from a single `GetAll`.
#[derive(Clone, Default, PartialEq)]
pub struct Snapshot {
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub track_id: String,
    pub art_url: String,
    pub position_us: i64,
    pub length_us: i64,
}

pub struct Player {
    conn: Connection,
}

impl Player {
    pub fn new() -> zbus::Result<Self> {
        Ok(Self {
            conn: Connection::session()?,
        })
    }

    fn proxy(&self, interface: &'static str) -> zbus::Result<Proxy<'_>> {
        Proxy::new(&self.conn, DEST, PATH, interface)
    }

    /// `None` means Spotify isn't on the session bus.
    pub fn snapshot(&self) -> Option<Snapshot> {
        let props: HashMap<String, OwnedValue> =
            self.proxy(PROPS).ok()?.call("GetAll", &(PLAYER,)).ok()?;

        let meta: HashMap<String, OwnedValue> = get(&props, "Metadata")?;
        let title: String = get(&meta, "xesam:title").unwrap_or_default();
        let track_id: String = get(&meta, "xesam:trackid")
            .or_else(|| get(&meta, "mpris:trackid"))
            .unwrap_or_default();

        // Spotify reports `mpris:length` as a uint64, but the spec says int64 —
        // accept either.
        let length_us = get::<u64>(&meta, "mpris:length")
            .map(|n| n as i64)
            .or_else(|| get::<i64>(&meta, "mpris:length"))
            .unwrap_or(0);

        let artists: Vec<String> = get(&meta, "xesam:artist").unwrap_or_default();
        let status: String = get(&props, "PlaybackStatus").unwrap_or_default();

        Some(Snapshot {
            playing: status == "Playing",
            title,
            artist: artists.join(", "),
            track_id,
            art_url: get(&meta, "mpris:artUrl").unwrap_or_default(),
            position_us: get(&props, "Position").unwrap_or(0),
            length_us,
        })
    }

    fn send(&self, method: &str) {
        if let Ok(p) = self.proxy(PLAYER) {
            let _ = p.call::<_, _, ()>(method, &());
        }
    }

    pub fn play_pause(&self) {
        self.send("PlayPause");
    }

    pub fn next(&self) {
        self.send("Next");
    }

    pub fn previous(&self) {
        self.send("Previous");
    }

    /// Spotify hands out `mpris:trackid` as a string rather than the object path
    /// the spec calls for, so re-type it here before handing it back.
    pub fn seek_to(&self, track_id: &str, position_us: i64) {
        let Ok(path) = ObjectPath::try_from(track_id) else {
            return;
        };
        if let Ok(p) = self.proxy(PLAYER) {
            let _ = p.call::<_, _, ()>("SetPosition", &(path, position_us));
        }
    }
}

fn get<T>(map: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    T::try_from(map.get(key)?.try_clone().ok()?).ok()
}
