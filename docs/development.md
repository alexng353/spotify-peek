# Development notes

These notes preserve earlier implementation decisions and measurements. Memory
figures are historical observations from one NVIDIA desktop, not requirements or
current benchmarks.

## Shape

Two layer-shell surfaces, both on the `overlay` layer so they sit above waybar:

- **the strip** — 500x2, pinned to the top edge, permanently mapped, exists only
  to notice the cursor. 2px so the remaining 33px of waybar keeps its own clicks.
- **the card** — unmapped whenever it isn't shown, so it captures no input and
  reserves no space.

All work is gated behind visibility. Closed, there are no timers, no D-Bus
subscriptions, no HTTP and no subprocesses; the process sleeps on the Wayland
socket. Hover for 120ms and it reads MPRIS with up to one `GetAll` per supported
client, starts a 1s tick, and
kicks the Web API lookups. On leave it cancels everything after a 200ms grace.

### What it costs

Idle (launched, never hovered): **RSS 89 MB, PSS 46 MB, 0.00% CPU** — zero CPU
ticks over a 15s window. Open, the 1s tick costs ~0.13%.

RSS overstates it badly. The split:

| | |
| --- | --- |
| private (genuinely ours) | 23 MB — 13 MB dirty heap, 10 MB private clean |
| shared read-only library pages | 66 MB |

and the largest mappings are not ours at all:

```
25.5 MB  libnvidia-gpucomp.so
17.8 MB  libnvidia-glcore.so
 9.7 MB  libgtk-4.so
 3.8 MB  [heap]
 1.8 MB  spotify-peek itself
```

The 43 MB of nvidia GL driver is mapped by GDK during Wayland display init and
**cannot be dropped** — `GDK_DISABLE=gl`, `gl,vulkan,dmabuf` all leave the same
29 mappings in place. Choosing the renderer is what actually moves the needle:

| renderer | RSS | PSS |
| --- | --- | --- |
| `GSK_RENDERER=cairo` (what we set) | 89 MB | **46 MB** |
| `GSK_RENDERER=ngl` | 258 MB | 167 MB |

Getting below ~20 MB would mean dropping GTK for raw Wayland plus hand-rolled
text layout and input handling. Not worth it for a card this size.

**Opening the card costs ~13 MB that is never given back** (89 → 101.5 MB), and
that's expected: GTK allocates the card's render buffers, glyph cache and icon
textures on first draw and keeps them, which is what makes later opens instant.

What matters is that it stops there. `--debug-cycle N` exists to prove it:

```
$ spotify-peek --debug-cycle 60
baseline (never opened): 88.3 MB
after   5 open/close cycles: 101.4 MB
after  59 open/close cycles: 101.6 MB     # flat
```

Before the pooled agent and `malloc_trim`, the same run crept from 102.7 MB to
108.6 MB over the first ~35 cycles — a fresh TLS agent per lookup plus the
allocator keeping every high-water mark. Both fixed; if that creep ever comes
back, this is the tool that shows it.

| Source | Used for |
| --- | --- |
| MPRIS (`zbus`) | title, primary artist, art URL, position, length, status, transport, seek |
| Web API (`ureq`) | up-next queue, full artist list, liked status, like toggle |

Auth rides on the OAuth app **`spotlike`** already has registered: its client
credentials (`~/.config/spotlike/env`) and refresh token
(`~/.local/share/spotlike/token.json`) are read but never written back. Our own
access token is cached separately in `~/.cache/spotify-peek/token.json`, so the
two tools can't clobber each other. Rotating spotlike's credentials breaks
up-next, the artist list and the liked toggle — everything MPRIS can't answer.

## Working on it

```sh
cargo test                       # parsing and MPRIS integration (needs dbus-daemon)
cargo clippy --all-targets       # kept warning-free
cargo build --release && install -Dm755 target/release/spotify-peek ~/.local/bin/
pkill -x spotify-peek && (spotify-peek &)
```

`~/.config/hypr/scripts/spotify-peek/style.css`, when present, overrides the
stylesheet compiled into the binary. Changes to that override need a restart.
Changes to the repository stylesheet need a rebuild.

Two debugging aids:

- `--debug-open` opens the card at startup and logs surface geometry, for
  checking layout without a real hover.
- `SPOTIFY_PEEK_DEBUG=1` traces the hover state machine and copied URLs.
- `--debug-cycle N` opens and closes the card N times, reporting resident memory
  after each, to tell a plateau apart from a leak.

Check the real geometry with `hyprctl layers`; the strip should read
`xywh: 1030 0 500 2`.

## Things that bit, and will again

- **`set_exclusive_zone(-1)`, not `0`.** Zero means "reserve nothing but stay
  clear of everyone else's reserved space", which parks the surface *below*
  waybar's 35px zone. -1 means "reserve nothing and ignore theirs".
- **Layer surfaces need `set_default_size`.** Without it GTK falls back to
  200x200 no matter what the content requests.
- **A fully transparent window never commits a buffer**, so the compositor
  leaves the surface unconfigured and it receives *no pointer events at all*.
  The strip carries `rgba(0,0,0,0.004)` to force the commit.
- **The revealer crossfades rather than slides.** A slide re-measures the surface
  every frame, so the compositor resizes the layer surface throughout the
  animation.
- **`GSK_RENDERER=cairo` is set in-process.** GTK's default GL renderer maps
  ~90 MB of nvidia driver state to draw a 460px card; cairo halves the process's
  memory share and costs nothing at this size. Override by setting the variable.
- **Spotify's MPRIS reports only the primary artist** — "Forever" arrives over
  D-Bus as just "Drake". The full list comes from the `currently_playing` object
  in the queue response, matched by track id so a stale list is never attributed
  to the wrong song, and no extra request is needed.
- **Spotify uses nonstandard MPRIS types:** `mpris:trackid` is a string and
  `mpris:length` is a uint64. Fastpotify uses the standard object path and int64.
  Both representations are accepted. Keep the MPRIS track ID for seeking; use
  `xesam:url` for the Spotify ID needed by likes and links, with a fallback for
  Spotify's legacy `/com/spotify/track/<id>` paths.
- **The queue endpoint needs an active playback session.** Paused with no active
  device, it returns `currently_playing: null` and an empty queue — up-next
  showing `—` is that state, not a bug.

## The sliding window

The cache spans `[prev, current, next … next+3]` — one row more than the three
shown — so a skip in either direction redraws correctly *before* the refetch
lands, and never blanks:

- **forward:** the queue head becomes current; the track we left becomes `prev`.
- **backward:** the track we left is pushed onto the front of the queue; `prev`
  becomes unknown and is refilled on the next lookup round.
- **any other jump:** the window is left alone and the refetch replaces it.

Shifting is driven by the observed track id, never by which button was pressed.
That matters because Spotify's `Previous` restarts the current track when you're
a few seconds in rather than moving back — no id change, so nothing shifts.
`slide_window` is pure and covered by tests for each of those cases.

`prev` is almost always free, since it's whatever we just moved off.
`recently-played` is only called when local history can't supply it — after a
cold open, or right after a backward skip — and its answer is discarded if it
names the track already playing, which happens once a track has played far
enough to enter the history.

## Deliberate choices

- Track changes never blank the UI. Art and queue rows hold their previous value
  until real data arrives.
- Web API lookups are debounced 350ms and read the generation at fire time, so
  holding skip costs one round of requests for wherever you landed.
- Async results are stamped with a generation and dropped if the track moved on.
- Copy produces a bare `open.spotify.com/track/<id>` with no `?si=` parameter.
