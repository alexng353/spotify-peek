//! spotify-peek — a Spotify card that drops down when the cursor touches the
//! top edge of the screen.
//!
//! Two layer-shell surfaces, both on the `overlay` layer so they sit above
//! waybar:
//!
//!   * a 500x2 transparent strip pinned to the top edge, permanently mapped,
//!     which exists only to notice the cursor;
//!   * the card itself, unmapped whenever it isn't shown, so it captures no
//!     input and reserves no space.
//!
//! Everything expensive is gated behind visibility. While the card is hidden
//! there are no timers, no D-Bus subscriptions, no HTTP and no subprocesses —
//! the process just sleeps on the Wayland socket.

mod api;
mod art;
mod mpris;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk::prelude::*;
use gtk::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CssProvider,
    EventControllerMotion, GestureClick, Image, Label, Orientation, ProgressBar, Revealer,
    RevealerTransitionType, glib,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

const APP_ID: &str = "dev.alex.SpotifyPeek";

/// Width of the invisible hover strip along the top edge.
const TRIGGER_W: i32 = 500;
/// Height of that strip. Two pixels is enough to catch a cursor clamped against
/// the screen edge, and shallow enough to leave waybar's own clicks alone.
const TRIGGER_H: i32 = 2;
const CARD_W: i32 = 460;

/// How long the cursor must sit on the strip before the card opens, so merely
/// crossing the top edge doesn't summon it.
const DWELL: Duration = Duration::from_millis(120);
/// Grace period after the cursor leaves, which also covers the handover as it
/// moves from the strip into the card.
const GRACE: Duration = Duration::from_millis(200);
const FADE_MS: u32 = 150;
const TICK: Duration = Duration::from_secs(1);

/// Up-next rows on the card.
const QUEUE_ROWS: usize = 3;
/// Rows actually fetched. One spare beyond what's shown, so that after a skip
/// the window still has a full set of rows to draw before the refetch lands —
/// the cached window spans [prev, current, next … next+3].
const QUEUE_FETCH: usize = QUEUE_ROWS + 1;
/// Don't re-ask Spotify for the queue more often than this.
const QUEUE_TTL: u64 = 10;
/// Web API lookups wait this long after a track change, so holding down skip
/// fires one round of requests instead of one per track.
const LOOKUP_DEBOUNCE: Duration = Duration::from_millis(350);
/// How long "Link copied" replaces the artist line.
const COPY_FEEDBACK: Duration = Duration::from_millis(1200);

const ICON_PLAY: &str = "media-playback-start-symbolic";
const ICON_PAUSE: &str = "media-playback-pause-symbolic";
const ICON_NO_ART: &str = "audio-x-generic-symbolic";
const ICON_LIKED: &str = "starred-symbolic";
const ICON_UNLIKED: &str = "non-starred-symbolic";

/// The media bindings from `~/.config/hypr/hyprland.lua`, shown as a legend so
/// the card doubles as a reminder of them.
const KEY_HINTS: [(&[&str], &str); 2] = [
    (&["Media keys"], "previous · play/pause · next"),
    (&["SUPER", "SHIFT", "C"], "copy song link"),
];

/// Results from worker threads, tagged with the generation they were requested
/// for, so results for a track we've already moved past get dropped.
enum Msg {
    Art {
        generation: u64,
        path: PathBuf,
    },
    Queue {
        generation: u64,
        queue: api::Queue,
    },
    QueueFailed {
        generation: u64,
    },
    Liked {
        generation: u64,
        liked: bool,
    },
    Previous {
        generation: u64,
        track: Option<api::Track>,
    },
}

struct Ui {
    popup: ApplicationWindow,
    revealer: Revealer,
    playing: GtkBox,
    idle: Label,
    art: Image,
    title: Label,
    artist: Label,
    elapsed: Label,
    total: Label,
    bar: ProgressBar,
    play_icon: Image,
    controls: Vec<Button>,
    like: Button,
    like_icon: Image,
    /// Art plus title/artist: the click-to-copy target.
    identity: GtkBox,
    queue_rows: Vec<Label>,
}

#[derive(Default)]
struct State {
    in_trigger: bool,
    in_popup: bool,
    open: bool,
    dwell: Option<glib::SourceId>,
    grace: Option<glib::SourceId>,
    unmap: Option<glib::SourceId>,
    tick: Option<glib::SourceId>,
    /// Track currently drawn on the card, for change detection.
    rendered: String,
    /// Bumped on every track change; stamps async requests.
    generation: u64,
    queue_fetched_at: u64,
    queue_inflight: Option<u64>,
    /// Shared debounce for the Web API lookups (queue and liked status).
    lookup_debounce: Option<glib::SourceId>,
    /// The window ahead of the current track, up to [`QUEUE_FETCH`] long.
    queue: Vec<api::Track>,
    /// The track behind the current one, so a backward skip shifts rather than
    /// blanks. Usually free — it's whatever we just moved off — and only
    /// fetched when we have no local history for it.
    prev: Option<api::Track>,
    prev_inflight: Option<u64>,
    /// `None` until the library check for the current track comes back.
    liked: Option<bool>,
    liked_inflight: Option<u64>,
    /// Full artist list from the Web API, keyed by track id, since MPRIS only
    /// reports the primary artist.
    artists: Option<(String, String)>,
    /// Held while the copy confirmation is showing, so the tick doesn't
    /// overwrite it.
    copy_feedback: Option<glib::SourceId>,
    art_inflight: Option<u64>,
    /// Last snapshot, kept so the seek gesture knows what it's seeking in.
    snap: Option<mpris::Snapshot>,
}

impl State {
    /// Turn a snapshot into a Track, preferring the Web API's full artist list
    /// over MPRIS's primary-artist-only value when we have it for that track.
    fn track_of(&self, snap: &mpris::Snapshot) -> api::Track {
        let id = spotify_id(snap).unwrap_or_default();
        let artist = self
            .artists
            .as_ref()
            .filter(|(for_id, _)| *for_id == id)
            .map(|(_, artists)| artists.clone())
            .unwrap_or_else(|| snap.artist.clone());
        api::Track {
            id,
            title: snap.title.clone(),
            artist,
        }
    }

    /// Slide the cached window when the track changes, so a skip in either
    /// direction draws correctly before the refetch lands.
    ///
    /// Driven by the observed track id, never by which button was pressed —
    /// which matters because Spotify's `Previous` restarts the current track
    /// when you're a few seconds in rather than moving back. That produces no
    /// id change, so nothing shifts.
    fn slide_window(&mut self, snap: &mpris::Snapshot, outgoing: Option<api::Track>) {
        let Some(id) = spotify_id(snap) else {
            return;
        };
        if self.queue.first().is_some_and(|t| t.is(&id)) {
            self.queue.remove(0);
            self.prev = outgoing;
        } else if self.prev.as_ref().is_some_and(|t| t.is(&id)) {
            if let Some(outgoing) = outgoing {
                self.queue.insert(0, outgoing);
                self.queue.truncate(QUEUE_FETCH);
            }
            // Whatever sits behind the new position is unknown until the next
            // lookup round refills it.
            self.prev = None;
        }
        // Any other jump leaves the window alone: the refetch replaces it,
        // which beats blanking the card in the meantime.
    }
}

struct App {
    ui: Ui,
    state: RefCell<State>,
    player: Option<mpris::Player>,
    tx: async_channel::Sender<Msg>,
}

fn main() -> glib::ExitCode {
    // `--debug-open` opens the card at startup and logs surface geometry, for
    // checking layout without a real hover.
    let debug_open = std::env::args().any(|a| a == "--debug-open");

    // GTK defaults to a GL renderer, which on this nvidia box maps ~90 MB of
    // driver state to draw a 460px card. Cairo's software renderer more than
    // halves the process's memory share (PSS 96 MB -> 42 MB) and costs nothing
    // to composite at this size.
    // SAFETY: still single-threaded here — before GTK init and before any
    // worker thread is spawned.
    if std::env::var_os("GSK_RENDERER").is_none() {
        unsafe { std::env::set_var("GSK_RENDERER", "cairo") };
    }

    // `--debug-cycle N` opens and closes the card N times, reporting resident
    // memory after each, to tell a plateau apart from a leak.
    let cycles = std::env::args()
        .skip_while(|a| a != "--debug-cycle")
        .nth(1)
        .and_then(|n| n.parse::<u32>().ok());

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(move |gtk_app| build(gtk_app, debug_open, cycles));
    // Argv is handled above; don't hand it to GTK, which would reject the flag.
    app.run_with_args::<&str>(&[])
}

/// Hand heap that glibc is holding but no longer using back to the kernel.
///
/// Drawing the card churns through a lot of short-lived allocations; without
/// this the allocator keeps the high-water mark for the rest of the session.
fn release_free_heap() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    // SAFETY: no arguments to get wrong, and it only ever releases memory the
    // allocator already considers free.
    unsafe { malloc_trim(0) };
}

/// This process's resident set, in MB.
fn resident_mb() -> f64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0.0);
    pages * 4096.0 / (1024.0 * 1024.0)
}

fn load_css() {
    let provider = CssProvider::new();
    let on_disk = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".config/hypr/scripts/spotify-peek/style.css");
    // Prefer the stylesheet next to the source so it can be tweaked without a
    // rebuild; fall back to the copy compiled in.
    if on_disk.is_file() {
        provider.load_from_path(&on_disk);
    } else {
        provider.load_from_string(include_str!("../style.css"));
    }
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Shared setup for both surfaces: overlay layer, pinned to the top edge,
/// reserving nothing and never taking the keyboard.
fn init_surface(window: &ApplicationWindow, namespace: &str, margin_top: i32, size: (i32, i32)) {
    window.init_layer_shell();
    window.set_namespace(Some(namespace));
    window.set_layer(Layer::Overlay);
    // Anchoring to exactly one edge leaves the compositor to centre it there.
    window.set_anchor(Edge::Top, true);
    window.set_margin(Edge::Top, margin_top);
    // -1, not 0: zero means "reserve nothing but stay clear of everyone else's
    // reserved space", which parks us below waybar's 35px zone. -1 means
    // "reserve nothing and ignore theirs", which is how we overlap it.
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);
    // Layer surfaces take the window's default size; without one GTK falls back
    // to 200x200 regardless of what the content asks for.
    window.set_default_size(size.0, size.1);
}

fn build(gtk_app: &Application, debug_open: bool, cycles: Option<u32>) {
    // Launching a second time doesn't start a second process: GTK hands the
    // activation to the running instance, which would otherwise build a second
    // set of surfaces on top of the first.
    if !gtk_app.windows().is_empty() {
        debug("already running; ignoring activation");
        return;
    }

    let (tx, rx) = async_channel::unbounded::<Msg>();

    let trigger = ApplicationWindow::builder().application(gtk_app).build();
    init_surface(&trigger, "spotify-peek-trigger", 0, (TRIGGER_W, TRIGGER_H));
    trigger.add_css_class("peek-trigger");
    let strip = GtkBox::new(Orientation::Horizontal, 0);
    strip.set_size_request(TRIGGER_W, TRIGGER_H);
    trigger.set_child(Some(&strip));

    let popup = ApplicationWindow::builder().application(gtk_app).build();
    init_surface(&popup, "spotify-peek", TRIGGER_H, (CARD_W, -1));
    popup.add_css_class("peek-popup");

    let ui = build_card(&popup);
    let app = Rc::new(App {
        ui,
        state: RefCell::new(State::default()),
        player: mpris::Player::new().ok(),
        tx,
    });

    wire_controls(&app);

    let surfaces: [(&gtk::Widget, bool); 2] =
        [(trigger.upcast_ref(), true), (popup.upcast_ref(), false)];
    for (widget, is_trigger) in surfaces {
        let motion = EventControllerMotion::new();
        // Capture, not the default bubble: nothing inside these surfaces cares
        // about the pointer, and capture sees the crossing event first.
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        let entered = app.clone();
        motion.connect_enter(move |_, _, _| {
            debug(if is_trigger {
                "enter strip"
            } else {
                "enter card"
            });
            entered.set_hovered(is_trigger, true);
        });
        let left = app.clone();
        motion.connect_leave(move |_| {
            debug(if is_trigger {
                "leave strip"
            } else {
                "leave card"
            });
            left.set_hovered(is_trigger, false);
        });
        widget.add_controller(motion);
    }

    let pump = app.clone();
    glib::spawn_future_local(async move {
        while let Ok(msg) = rx.recv().await {
            pump.handle(msg);
        }
    });

    trigger.present();

    if let Some(cycles) = cycles {
        let probe = app.clone();
        let mut remaining = cycles * 2;
        let mut open = false;
        eprintln!("baseline (never opened): {:.1} MB", resident_mb());
        glib::timeout_add_local(Duration::from_millis(700), move || {
            open = !open;
            probe.set_hovered(true, open);
            if !open {
                eprintln!(
                    "after {:>3} open/close cycles: {:.1} MB",
                    (cycles * 2 - remaining).div_ceil(2),
                    resident_mb()
                );
            }
            remaining -= 1;
            if remaining == 0 {
                probe.ui.popup.application().unwrap().quit();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    if debug_open {
        let probe = app.clone();
        let trigger = trigger.clone();
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            eprintln!(
                "trigger allocation: {}x{}",
                trigger.width(),
                trigger.height()
            );
            probe.set_hovered(true, true);
            glib::timeout_add_local_once(Duration::from_millis(600), move || {
                eprintln!(
                    "card allocation: {}x{}",
                    probe.ui.popup.width(),
                    probe.ui.popup.height()
                );
            });
        });
    }
}

fn build_card(popup: &ApplicationWindow) -> Ui {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.add_css_class("card");
    root.set_size_request(CARD_W, -1);

    // --- now playing -------------------------------------------------------
    let art = Image::from_icon_name(ICON_NO_ART);
    art.set_pixel_size(64);
    art.add_css_class("art");

    let title = Label::builder()
        .halign(Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    title.add_css_class("title");
    let artist = Label::builder()
        .halign(Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    artist.add_css_class("artist");

    let text = GtkBox::new(Orientation::Vertical, 2);
    text.set_valign(Align::Center);
    text.set_hexpand(true);
    text.append(&title);
    text.append(&artist);

    // Liked toggle. Adwaita ships no heart pair, so a star stands in.
    let like_icon = Image::from_icon_name(ICON_UNLIKED);
    let like = Button::builder().child(&like_icon).build();
    like.add_css_class("control");
    like.add_css_class("like");
    like.set_valign(Align::Start);
    // Until the library check returns, the star reads as "checking" rather than
    // "not saved".
    like.add_css_class("unknown");

    // Art and text together are the click-to-copy target; the like button is a
    // sibling so its clicks can't be mistaken for a copy.
    let identity = GtkBox::new(Orientation::Horizontal, 12);
    identity.set_hexpand(true);
    identity.add_css_class("identity");
    identity.set_tooltip_text(Some("Click to copy link"));
    identity.append(&art);
    identity.append(&text);

    let header = GtkBox::new(Orientation::Horizontal, 12);
    header.append(&identity);
    header.append(&like);

    // --- seek bar ----------------------------------------------------------
    let elapsed = Label::new(Some("0:00"));
    elapsed.add_css_class("time");
    let total = Label::new(Some("0:00"));
    total.add_css_class("time");
    let bar = ProgressBar::new();
    bar.set_hexpand(true);
    bar.set_valign(Align::Center);
    bar.add_css_class("seek");

    let seek_row = GtkBox::new(Orientation::Horizontal, 8);
    seek_row.append(&elapsed);
    seek_row.append(&bar);
    seek_row.append(&total);

    // --- controls ----------------------------------------------------------
    let play_icon = Image::from_icon_name(ICON_PLAY);
    let mut controls = Vec::new();
    let control_row = GtkBox::new(Orientation::Horizontal, 8);
    control_row.set_halign(Align::Center);
    let icons = [
        Some("media-skip-backward-symbolic"),
        None, // play/pause, whose icon swaps with playback state
        Some("media-skip-forward-symbolic"),
    ];
    for icon in icons {
        let button = match icon {
            Some(name) => {
                let image = Image::from_icon_name(name);
                Button::builder().child(&image).build()
            }
            None => Button::builder().child(&play_icon).build(),
        };
        button.add_css_class("control");
        if icon.is_none() {
            button.add_css_class("primary");
        }
        control_row.append(&button);
        controls.push(button);
    }

    // --- up next -----------------------------------------------------------
    // Fill rather than Start, so the label's border-top spans the whole card
    // and can double as the divider.
    let heading = Label::builder()
        .label("UP NEXT")
        .halign(Align::Fill)
        .xalign(0.0)
        .build();
    heading.add_css_class("heading");

    let queue_box = GtkBox::new(Orientation::Vertical, 1);
    let queue_rows: Vec<Label> = (0..QUEUE_ROWS)
        .map(|_| {
            let row = Label::builder()
                .halign(Align::Start)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .label("—")
                .build();
            row.add_css_class("queue-row");
            queue_box.append(&row);
            row
        })
        .collect();

    // --- keybind legend ----------------------------------------------------
    let legend = GtkBox::new(Orientation::Vertical, 3);
    legend.add_css_class("legend");
    for (keys, what) in KEY_HINTS {
        let row = GtkBox::new(Orientation::Horizontal, 4);
        for key in keys {
            let cap = Label::new(Some(key));
            cap.add_css_class("key");
            row.append(&cap);
        }
        let what = Label::new(Some(what));
        what.add_css_class("key-what");
        row.append(&what);
        legend.append(&row);
    }

    let playing = GtkBox::new(Orientation::Vertical, 10);
    playing.append(&header);
    playing.append(&seek_row);
    playing.append(&control_row);
    playing.append(&heading);
    playing.append(&queue_box);
    playing.append(&legend);

    let idle = Label::new(Some("Nothing playing"));
    idle.add_css_class("idle");
    idle.set_visible(false);

    root.append(&playing);
    root.append(&idle);

    // Crossfade rather than slide: a slide re-measures the surface every frame,
    // which makes the compositor resize the layer surface throughout the
    // animation. A crossfade keeps the geometry fixed.
    let revealer = Revealer::builder()
        .transition_type(RevealerTransitionType::Crossfade)
        .transition_duration(FADE_MS)
        .child(&root)
        .build();
    popup.set_child(Some(&revealer));

    Ui {
        popup: popup.clone(),
        revealer,
        playing,
        idle,
        art,
        title,
        artist,
        elapsed,
        total,
        bar,
        play_icon,
        controls,
        like,
        like_icon,
        identity,
        queue_rows,
    }
}

fn wire_controls(app: &Rc<App>) {
    let actions: [fn(&mpris::Player); 3] = [
        mpris::Player::previous,
        mpris::Player::play_pause,
        mpris::Player::next,
    ];
    for (button, action) in app.ui.controls.iter().zip(actions) {
        let clicked = app.clone();
        button.connect_clicked(move |_| {
            if let Some(player) = &clicked.player {
                action(player);
            }
            clicked.refresh_soon();
        });
    }

    let liker = app.clone();
    app.ui.like.connect_clicked(move |_| {
        // Nothing to toggle until the library check has come back.
        let (Some(current), Some(id)) = (
            liker.state.borrow().liked,
            liker.state.borrow().snap.as_ref().and_then(spotify_id),
        ) else {
            return;
        };
        let generation = liker.state.borrow().generation;
        let target = !current;
        // Show the new state at once; the worker reverts it if Spotify refuses.
        liker.apply_liked(Some(target));

        let tx = liker.tx.clone();
        thread::spawn(move || {
            let liked = match api::set_liked(&id, target) {
                Ok(()) => target,
                Err(why) => {
                    eprintln!("spotify-peek: {why}");
                    current
                }
            };
            let _ = tx.send_blocking(Msg::Liked { generation, liked });
        });
    });

    let copier = app.clone();
    let copy = GestureClick::new();
    copy.connect_pressed(move |_, _, _, _| copier.copy_link());
    app.ui.identity.add_controller(copy);

    let seek = app.clone();
    let bar = app.ui.bar.clone();
    let gesture = GestureClick::new();
    gesture.connect_pressed(move |_, _, x, _| {
        let width = bar.width();
        let snap = seek.state.borrow().snap.clone();
        let (Some(player), Some(snap)) = (&seek.player, snap) else {
            return;
        };
        if width <= 0 || snap.length_us <= 0 || snap.track_id.is_empty() {
            return;
        }
        let fraction = (x / f64::from(width)).clamp(0.0, 1.0);
        player.seek_to(&snap.track_id, (snap.length_us as f64 * fraction) as i64);
        seek.refresh_soon();
    });
    app.ui.bar.add_controller(gesture);
}

impl App {
    fn set_hovered(self: &Rc<Self>, is_trigger: bool, inside: bool) {
        {
            let mut state = self.state.borrow_mut();
            if is_trigger {
                state.in_trigger = inside;
            } else {
                state.in_popup = inside;
            }
        }
        self.reevaluate();
    }

    /// The single place that decides whether the card should be opening or
    /// closing.
    fn reevaluate(self: &Rc<Self>) {
        let mut state = self.state.borrow_mut();
        let hovered = state.in_trigger || state.in_popup;

        if hovered {
            cancel(&mut state.grace);
            if !state.open && state.dwell.is_none() {
                let app = self.clone();
                state.dwell = Some(glib::timeout_add_local_once(DWELL, move || {
                    app.state.borrow_mut().dwell = None;
                    app.open();
                }));
            }
        } else {
            cancel(&mut state.dwell);
            if state.open && state.grace.is_none() {
                let app = self.clone();
                state.grace = Some(glib::timeout_add_local_once(GRACE, move || {
                    app.state.borrow_mut().grace = None;
                    app.close();
                }));
            }
        }
    }

    fn open(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            if state.open {
                return;
            }
            state.open = true;
            cancel(&mut state.unmap);
        }

        self.refresh(true);
        self.ui.popup.present();

        // Let the surface map before the fade starts, or GTK skips it.
        let reveal = self.clone();
        glib::idle_add_local_once(move || {
            if reveal.state.borrow().open {
                reveal.ui.revealer.set_reveal_child(true);
            }
        });

        let tick = self.clone();
        self.state.borrow_mut().tick = Some(glib::timeout_add_local(TICK, move || {
            tick.refresh(false);
            glib::ControlFlow::Continue
        }));
    }

    fn close(self: &Rc<Self>) {
        let mut state = self.state.borrow_mut();
        state.open = false;
        cancel(&mut state.tick);
        self.ui.revealer.set_reveal_child(false);

        let app = self.clone();
        state.unmap = Some(glib::timeout_add_local_once(
            Duration::from_millis(u64::from(FADE_MS) + 30),
            move || {
                let still_closed = {
                    let mut state = app.state.borrow_mut();
                    state.unmap = None;
                    !state.open
                };
                if still_closed {
                    app.ui.popup.set_visible(false);
                    release_free_heap();
                }
            },
        ));
    }

    /// Spotify applies transport changes asynchronously, so give it a beat
    /// before re-reading — otherwise the card redraws the pre-click state.
    fn refresh_soon(self: &Rc<Self>) {
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(120), move || {
            app.refresh(false);
        });
    }

    /// Read the player and redraw. `opening` forces a queue refresh even when
    /// the track hasn't changed, so a re-hover picks up reordering.
    fn refresh(self: &Rc<Self>, opening: bool) {
        let snap = self
            .player
            .as_ref()
            .and_then(mpris::Player::snapshot)
            .filter(|s| !s.title.is_empty());

        let Some(snap) = snap else {
            self.ui.playing.set_visible(false);
            self.ui.idle.set_visible(true);
            self.ui.idle.set_label(if self.player.is_some() {
                "Nothing playing"
            } else {
                "No session bus"
            });
            let mut state = self.state.borrow_mut();
            state.snap = None;
            state.rendered.clear();
            return;
        };

        self.ui.idle.set_visible(false);
        self.ui.playing.set_visible(true);

        self.ui.title.set_label(&snap.title);
        self.render_artist(&snap);
        self.ui
            .play_icon
            .set_icon_name(Some(if snap.playing { ICON_PAUSE } else { ICON_PLAY }));

        let fraction = if snap.length_us > 0 {
            (snap.position_us as f64 / snap.length_us as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.ui.bar.set_fraction(fraction);
        self.ui.elapsed.set_label(&clock(snap.position_us));
        self.ui.total.set_label(&clock(snap.length_us));

        let (changed, generation, stale) = {
            let mut state = self.state.borrow_mut();
            let changed = state.rendered != snap.track_id;
            let outgoing = state.snap.replace(snap.clone());
            if changed {
                state.generation += 1;
                state.rendered = snap.track_id.clone();
                let outgoing = outgoing.as_ref().map(|s| state.track_of(s));
                state.slide_window(&snap, outgoing);
            }
            let stale = now().saturating_sub(state.queue_fetched_at) >= QUEUE_TTL;
            (changed, state.generation, stale)
        };

        if changed {
            self.set_art(&snap.art_url, generation);
            self.apply_liked(None);
            self.render_queue();
        }
        let liked_unknown = self.state.borrow().liked.is_none();
        if changed || (opening && (stale || liked_unknown)) {
            self.schedule_lookups();
        }
    }

    /// Both Web API lookups share one debounced timer, and read the generation
    /// at fire time so a burst of skips resolves to a single round of requests
    /// for whatever track we actually landed on.
    fn schedule_lookups(self: &Rc<Self>) {
        let mut state = self.state.borrow_mut();
        cancel(&mut state.lookup_debounce);
        let app = self.clone();
        state.lookup_debounce = Some(glib::timeout_add_local_once(LOOKUP_DEBOUNCE, move || {
            let generation = {
                let mut state = app.state.borrow_mut();
                state.lookup_debounce = None;
                state.generation
            };
            app.fetch_queue(generation);
            app.fetch_liked(generation);
            app.fetch_previous(generation);
        }));
    }

    /// Prefer the Web API's full artist list over MPRIS's primary-artist-only
    /// value. Keyed by track id, so a stale list is simply ignored rather than
    /// briefly attributed to the wrong song.
    fn render_artist(&self, snap: &mpris::Snapshot) {
        let full = {
            let state = self.state.borrow();
            if state.copy_feedback.is_some() {
                // A copy confirmation is showing in this label; leave it be.
                return;
            }
            let id = spotify_id(snap);
            state
                .artists
                .as_ref()
                .filter(|(for_id, _)| Some(for_id) == id.as_ref())
                .map(|(_, artists)| artists.clone())
        };
        self.ui
            .artist
            .set_label(full.as_deref().unwrap_or(&snap.artist));
    }

    /// Copy a clean track URL — no `?si=` share parameter, which is what makes
    /// Spotify's own "copy link" output a tracking URL.
    fn copy_link(self: &Rc<Self>) {
        let Some(id) = self.state.borrow().snap.as_ref().and_then(spotify_id) else {
            return;
        };
        let url = format!("https://open.spotify.com/track/{id}");
        if let Some(display) = gtk::gdk::Display::default() {
            display.clipboard().set_text(&url);
        }
        debug(&format!("copied {url}"));

        cancel(&mut self.state.borrow_mut().copy_feedback);
        self.ui.artist.set_label("Link copied");
        let app = self.clone();
        self.state.borrow_mut().copy_feedback =
            Some(glib::timeout_add_local_once(COPY_FEEDBACK, move || {
                app.state.borrow_mut().copy_feedback = None;
                if let Some(snap) = app.state.borrow().snap.clone() {
                    app.render_artist(&snap);
                }
            }));
    }

    /// Paint the liked state. `None` means "not known yet".
    fn apply_liked(&self, liked: Option<bool>) {
        self.state.borrow_mut().liked = liked;
        self.ui
            .like_icon
            .set_icon_name(Some(if liked == Some(true) {
                ICON_LIKED
            } else {
                ICON_UNLIKED
            }));
        if liked.is_some() {
            self.ui.like.remove_css_class("unknown");
        } else {
            self.ui.like.add_css_class("unknown");
        }
        if liked == Some(true) {
            self.ui.like.add_css_class("liked");
        } else {
            self.ui.like.remove_css_class("liked");
        }
    }

    /// Draw the cached queue. Rows past the end of what we know are left alone
    /// while a fetch is in flight, so nothing blanks out mid-update.
    fn render_queue(&self) {
        let state = self.state.borrow();
        let pending = state.queue_inflight.is_some() || state.lookup_debounce.is_some();
        for (index, label) in self.ui.queue_rows.iter().enumerate() {
            match state.queue.get(index) {
                Some(track) => label.set_label(&format!("{}  ·  {}", track.title, track.artist)),
                None if pending => {}
                None => label.set_label("—"),
            }
        }
    }

    fn set_art(self: &Rc<Self>, url: &str, generation: u64) {
        if let Some(path) = art::cached(url) {
            self.ui.art.set_from_file(Some(path));
            return;
        }
        self.ui.art.set_icon_name(Some(ICON_NO_ART));
        if url.is_empty() || self.state.borrow().art_inflight == Some(generation) {
            return;
        }
        self.state.borrow_mut().art_inflight = Some(generation);

        let tx = self.tx.clone();
        let url = url.to_string();
        thread::spawn(move || {
            match art::fetch(&url) {
                Ok(path) => {
                    let _ = tx.send_blocking(Msg::Art { generation, path });
                }
                Err(why) => eprintln!("spotify-peek: {why}"),
            };
        });
    }

    fn fetch_queue(self: &Rc<Self>, generation: u64) {
        if self.state.borrow().queue_inflight == Some(generation) {
            return;
        }
        self.state.borrow_mut().queue_inflight = Some(generation);

        let tx = self.tx.clone();
        thread::spawn(move || {
            let msg = match api::queue(QUEUE_FETCH) {
                Ok(queue) => Msg::Queue { generation, queue },
                Err(why) => {
                    eprintln!("spotify-peek: {why}");
                    Msg::QueueFailed { generation }
                }
            };
            let _ = tx.send_blocking(msg);
        });
    }

    /// Only asks Spotify when local history can't supply the previous track —
    /// after a cold open, or immediately after a backward skip.
    fn fetch_previous(self: &Rc<Self>, generation: u64) {
        {
            let state = self.state.borrow();
            if state.prev.is_some() || state.prev_inflight == Some(generation) {
                return;
            }
        }
        self.state.borrow_mut().prev_inflight = Some(generation);

        let tx = self.tx.clone();
        thread::spawn(move || match api::previously_played() {
            Ok(track) => {
                let _ = tx.send_blocking(Msg::Previous { generation, track });
            }
            Err(why) => eprintln!("spotify-peek: {why}"),
        });
    }

    fn fetch_liked(self: &Rc<Self>, generation: u64) {
        let Some(id) = self.state.borrow().snap.as_ref().and_then(spotify_id) else {
            return;
        };
        if self.state.borrow().liked_inflight == Some(generation) {
            return;
        }
        self.state.borrow_mut().liked_inflight = Some(generation);

        let tx = self.tx.clone();
        thread::spawn(move || match api::is_liked(&id) {
            Ok(liked) => {
                let _ = tx.send_blocking(Msg::Liked { generation, liked });
            }
            Err(why) => eprintln!("spotify-peek: {why}"),
        });
    }

    fn handle(self: &Rc<Self>, msg: Msg) {
        let current = self.state.borrow().generation;
        match msg {
            Msg::Art { generation, path } => {
                if generation == current {
                    self.ui.art.set_from_file(Some(path));
                }
                self.state.borrow_mut().art_inflight = None;
            }
            Msg::Queue { generation, queue } => {
                let snap = {
                    let mut state = self.state.borrow_mut();
                    state.queue_inflight = None;
                    if generation != current {
                        return;
                    }
                    state.queue = queue.next;
                    state.queue_fetched_at = now();
                    if let Some(now_playing) = queue.now {
                        state.artists = Some((now_playing.id, now_playing.artist));
                    }
                    state.snap.clone()
                };
                self.render_queue();
                if let Some(snap) = snap {
                    self.render_artist(&snap);
                }
            }
            Msg::QueueFailed { generation } => {
                self.state.borrow_mut().queue_inflight = None;
                if generation == current {
                    // Leave whatever's on screen; just stop claiming a fetch is
                    // pending so unknown rows settle.
                    self.render_queue();
                }
            }
            Msg::Liked { generation, liked } => {
                self.state.borrow_mut().liked_inflight = None;
                if generation == current {
                    self.apply_liked(Some(liked));
                }
            }
            Msg::Previous { generation, track } => {
                let mut state = self.state.borrow_mut();
                state.prev_inflight = None;
                if generation != current {
                    return;
                }
                // History includes the current track once it's played far
                // enough, which would make a backward skip look like a jump.
                let playing = state.snap.as_ref().and_then(spotify_id).unwrap_or_default();
                if !track.as_ref().is_some_and(|t| t.is(&playing)) {
                    state.prev = track;
                }
            }
        }
    }
}

/// Traces the hover state machine when `SPOTIFY_PEEK_DEBUG` is set.
fn debug(message: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SPOTIFY_PEEK_DEBUG").is_some()) {
        eprintln!("spotify-peek: {message}");
    }
}

fn cancel(slot: &mut Option<glib::SourceId>) {
    if let Some(id) = slot.take() {
        id.remove();
    }
}

/// Spotify's MPRIS track id looks like `/com/spotify/track/<base62>`; the Web
/// API wants the last segment on its own.
fn spotify_id(snap: &mpris::Snapshot) -> Option<String> {
    let id = snap.track_id.rsplit('/').next()?;
    let plausible = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric());
    plausible.then(|| id.to_string())
}

fn clock(microseconds: i64) -> String {
    let seconds = (microseconds / 1_000_000).max(0);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> api::Track {
        api::Track {
            id: id.into(),
            title: id.into(),
            artist: "someone".into(),
        }
    }

    /// A snapshot as MPRIS delivers it, with the object-path style track id.
    fn playing(id: &str) -> mpris::Snapshot {
        mpris::Snapshot {
            track_id: format!("/com/spotify/track/{id}"),
            title: id.into(),
            ..Default::default()
        }
    }

    fn window(prev: Option<&str>, queue: &[&str]) -> State {
        State {
            prev: prev.map(track),
            queue: queue.iter().map(|id| track(id)).collect(),
            ..Default::default()
        }
    }

    fn ids(tracks: &[api::Track]) -> Vec<&str> {
        tracks.iter().map(|t| t.id.as_str()).collect()
    }

    #[test]
    fn skipping_forward_promotes_the_queue_head() {
        let mut state = window(None, &["b", "c", "d", "e"]);
        state.slide_window(&playing("b"), Some(track("a")));

        assert_eq!(ids(&state.queue), ["c", "d", "e"], "head is consumed");
        assert_eq!(
            state.prev.as_ref().map(|t| t.id.as_str()),
            Some("a"),
            "the track we left becomes prev, for free"
        );
    }

    #[test]
    fn skipping_back_pushes_the_current_track_onto_the_queue() {
        let mut state = window(Some("a"), &["c", "d"]);
        state.slide_window(&playing("a"), Some(track("b")));

        assert_eq!(ids(&state.queue), ["b", "c", "d"]);
        assert!(state.prev.is_none(), "what's behind 'a' is now unknown");
    }

    #[test]
    fn the_window_stays_bounded_when_skipping_back() {
        let mut state = window(Some("a"), &["c", "d", "e", "f"]);
        state.slide_window(&playing("a"), Some(track("b")));
        assert_eq!(state.queue.len(), QUEUE_FETCH);
    }

    /// Jumping somewhere unrelated must leave the window alone rather than
    /// blanking it; the refetch replaces it a moment later.
    #[test]
    fn an_unrelated_jump_leaves_the_window_alone() {
        let mut state = window(Some("a"), &["c", "d"]);
        state.slide_window(&playing("zzz"), Some(track("b")));

        assert_eq!(ids(&state.queue), ["c", "d"]);
        assert_eq!(state.prev.as_ref().map(|t| t.id.as_str()), Some("a"));
    }

    /// Spotify's `Previous` restarts the current track when you're a few
    /// seconds in. That's no id change, so nothing may shift.
    #[test]
    fn restarting_the_same_track_does_not_shift() {
        let mut state = window(Some("a"), &["c", "d"]);
        state.slide_window(&playing("b"), Some(track("b")));

        assert_eq!(ids(&state.queue), ["c", "d"]);
        assert_eq!(state.prev.as_ref().map(|t| t.id.as_str()), Some("a"));
    }

    #[test]
    fn full_artist_list_wins_over_mpris_when_it_matches() {
        let mut state = window(None, &[]);
        state.artists = Some(("b".into(), "Drake, Kanye West".into()));

        let mut snap = playing("b");
        snap.artist = "Drake".into();
        assert_eq!(state.track_of(&snap).artist, "Drake, Kanye West");

        // ...but a list belonging to a different track is ignored.
        let mut other = playing("zzz");
        other.artist = "Someone Else".into();
        assert_eq!(state.track_of(&other).artist, "Someone Else");
    }

    #[test]
    fn formats_times_as_minutes_and_seconds() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(65_000_000), "1:05");
        assert_eq!(clock(-5), "0:00");
    }

    #[test]
    fn extracts_the_base62_id_from_the_mpris_path() {
        assert_eq!(
            spotify_id(&playing("7oDbkGSI3IRz")).as_deref(),
            Some("7oDbkGSI3IRz")
        );
        assert!(spotify_id(&mpris::Snapshot::default()).is_none());
    }
}
