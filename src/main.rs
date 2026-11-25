#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod art;
mod backend;
mod eq;
mod settings;

use std::{
    cell::RefCell, collections::HashMap, ffi::c_void, path::PathBuf, rc::Rc,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use backend::{Cmd, Event};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use settings::{Settings, Shared};
use slint::{CloseRequestResponse, ComponentHandle, Model, ModelRc, VecModel};
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};
use tokio::sync::mpsc::UnboundedSender;

slint::include_modules!();

/// Twice the size it is shown at, for scaled displays.
const THUMB: u32 = 132;
// shortcut: forgotten all at once when full, use an LRU if scrolling back refetches too much.
const MAX_THUMBS: usize = 600;

struct Ui {
    app: App,
    tray: Tray,
    rows: Rc<VecModel<TrackRow>>,
    media: Option<MediaControls>,
    duration_ms: u32,
    /// Position at `since`; it keeps moving only while `playing`.
    position_ms: u32,
    since: Instant,
    playing: bool,
    title_bar: [u8; 3],
    cover: Option<art::Pixels>,
    /// Physical pixels the Now Playing cover is shown at.
    cover_size: u32,
    track_uri: String,
    tx: UnboundedSender<Cmd>,
    /// Small covers by image id; `None` while one is being fetched.
    thumbs: HashMap<String, Option<slint::Image>>,
    /// Start of each lyrics line. Belongs to `lyrics_uri`.
    lyrics: Vec<u32>,
    lyrics_uri: String,
    shared: Arc<Shared>,
    settings_path: PathBuf,
    save: slint::Timer,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn with_ui(f: impl FnOnce(&mut Ui)) {
    UI.with_borrow_mut(|ui| ui.as_mut().map(f));
}

const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_CAPTION_COLOR: u32 = 35;

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(hwnd: *mut c_void, attribute: u32, value: *const u32, size: u32) -> i32;
}

fn hwnd(app: &App) -> Option<*mut c_void> {
    match app.window().window_handle().window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win) => Some(win.hwnd.get() as *mut c_void),
        _ => None,
    }
}

fn clock(ms: u32) -> String {
    let s = ms / 1000;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

impl Ui {
    fn now_ms(&self) -> u32 {
        let run = if self.playing { self.since.elapsed().as_millis() as u32 } else { 0 };
        self.position_ms.saturating_add(run).min(self.duration_ms)
    }

    fn show_position(&self) {
        let now = self.now_ms();
        self.app.set_elapsed(clock(now).into());
        self.follow_lyrics(now);
        self.app.set_progress(if self.duration_ms == 0 { 0. } else { now as f32 / self.duration_ms as f32 });
    }

    fn set_playback(&mut self, ms: u32, playing: bool) {
        (self.position_ms, self.since, self.playing) = (ms, Instant::now(), playing);
        self.app.set_playing(playing);
        self.tray.set_playing(playing);
        self.show_position();
        let progress = Some(MediaPosition(Duration::from_millis(ms.into())));
        self.media_update(|m| {
            m.set_playback(if playing { MediaPlayback::Playing { progress } } else { MediaPlayback::Paused { progress } })
        });
    }

    /// Fetches the current song's lyrics once they can be seen.
    fn want_lyrics(&mut self) {
        if self.app.get_lyrics_visible() && !self.track_uri.is_empty() && self.lyrics_uri != self.track_uri {
            self.lyrics_uri = self.track_uri.clone();
            self.set_lyrics(false, vec![], "Loading lyrics…");
            let _ = self.tx.send(Cmd::Lyrics(self.track_uri.clone()));
        }
    }

    fn set_lyrics(&mut self, synced: bool, lines: Vec<(u32, String)>, note: &str) {
        let (starts, words): (Vec<_>, Vec<slint::SharedString>) = lines.into_iter().map(|(t, w)| (t, w.into())).unzip();
        self.lyrics = starts;
        self.app.set_lyrics_active(-1);
        self.app.set_lyrics_synced(synced);
        self.app.set_lyrics_note(note.into());
        self.app.set_lyrics(ModelRc::new(VecModel::from(words)));
        self.follow_lyrics(self.now_ms());
    }

    fn follow_lyrics(&self, now: u32) {
        self.app.set_lyrics_active(line_at(&self.lyrics, now));
    }

    fn want_cover(&mut self, id: String) {
        if self.thumbs.len() >= MAX_THUMBS {
            self.thumbs.clear();
        }
        match self.thumbs.get(&id) {
            Some(Some(image)) => self.fill_cover(&id, image.clone()),
            Some(None) => {}
            None => {
                let _ = self.tx.send(Cmd::Thumb(id.clone()));
                self.thumbs.insert(id, None);
            }
        }
    }

    fn fill_cover(&self, id: &str, image: slint::Image) {
        for (i, mut row) in self.rows.iter().enumerate() {
            if row.cover_id == id && !row.has_cover {
                (row.cover, row.has_cover) = (image.clone(), true);
                self.rows.set_row_data(i, row);
            }
        }
    }

    fn change_settings(&mut self, change: impl FnOnce(&mut Settings)) {
        let settings = {
            let mut settings = self.shared.lock();
            let old = settings.clone();
            change(&mut settings);
            if (old.bitrate, old.normalise) != (settings.bitrate, settings.normalise) {
                self.app.set_needs_restart(true);
            }
            settings.clone()
        };
        self.shared.eq_version.fetch_add(1, Ordering::Release);
        self.show_settings(&settings);
        // A dragged slider changes many times a second; write once it rests.
        self.save.start(slint::TimerMode::SingleShot, Duration::from_millis(600), || with_ui(|ui| ui.save_settings()));
    }

    fn save_settings(&mut self) {
        self.save.stop();
        let settings = self.shared.lock().clone();
        if let Err(e) = settings.save(&self.settings_path) {
            self.app.set_status(format!("Settings could not be saved: {e}").into());
        }
    }

    fn show_settings(&self, s: &Settings) {
        let app = &self.app;
        app.set_eq_on(s.eq_on);
        app.set_bitrate(s.bitrate.into());
        app.set_normalise(s.normalise);
        app.set_tray(s.tray);
        app.global::<Theme>().set_tint(s.tint);
        // Replacing the model would drop the slider that is being dragged.
        let bands = app.get_eq();
        for (i, gain) in s.eq.iter().enumerate() {
            if bands.row_data(i) != Some(*gain) {
                bands.set_row_data(i, *gain);
            }
        }
    }

    fn set_look(&mut self, look: art::Look) {
        let app = &self.app;
        let image = slint::Image::from_rgba8(look.backdrop);
        // Two layers cross-fade; the new picture always goes to the hidden one.
        let scene = app.global::<Scene>();
        let show_b = !scene.get_show_b();
        if show_b { scene.set_b(image) } else { scene.set_a(image) }
        scene.set_show_b(show_b);
        let [r, g, b] = look.accent;
        app.global::<Theme>().set_accent(slint::Color::from_rgb_u8(r, g, b));
        self.title_bar = look.edge;
        self.tint_title_bar();
    }

    fn show_cover(&self) {
        if let Some(pixels) = &self.cover {
            let shrinks = (1..pixels.width()).contains(&self.cover_size);
            let pixels = if shrinks { art::shrink(pixels, self.cover_size) } else { pixels.clone() };
            self.app.set_cover(slint::Image::from_rgba8(pixels));
        }
    }

    fn tint_title_bar(&self) {
        if let Some(hwnd) = hwnd(&self.app) {
            let [r, g, b] = self.title_bar;
            let set = |attribute, value: u32| unsafe {
                DwmSetWindowAttribute(hwnd, attribute, &raw const value, size_of::<u32>() as u32)
            };
            // Purely cosmetic, so older Windows versions may refuse it.
            set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
            set(DWMWA_CAPTION_COLOR, u32::from_le_bytes([r, g, b, 0]));
        }
    }

    fn media_update(&mut self, f: impl FnOnce(&mut MediaControls) -> Result<(), souvlaki::Error>) {
        if let Some(Err(e)) = self.media.as_mut().map(f) {
            log::warn!("media controls: {e:?}");
        }
    }

    fn handle(&mut self, event: Event) {
        let app = &self.app;
        match event {
            Event::NeedLogin(message) => {
                app.set_ready(false);
                app.set_need_login(true);
                app.set_busy(false);
                app.set_message(if message.is_empty() { "Spotify Premium is required.".into() } else { message.into() });
            }
            Event::Connecting(message) => {
                app.set_ready(false);
                app.set_need_login(false);
                app.set_busy(true);
                app.set_message(message.into());
            }
            Event::Offline(message) => {
                app.set_ready(false);
                app.set_need_login(false);
                app.set_busy(false);
                app.set_message(message.into());
            }
            Event::Ready => app.set_ready(true),
            Event::Status(text) => app.set_status(text.into()),
            Event::Playlists(list) => {
                let list: Vec<_> = list.into_iter().map(|(name, uri)| Playlist { name: name.into(), uri: uri.into() }).collect();
                app.set_playlists(ModelRc::new(VecModel::from(list)));
            }
            Event::List { title, context } => {
                self.rows.set_vec(vec![]);
                app.set_list_title(title.into());
                app.set_list_context(context.into());
                app.set_status("".into());
                app.set_loading(true);
                app.set_has_more(true);
                app.invoke_reset_scroll();
            }
            Event::Rows(rows) => {
                app.set_loading(false);
                self.rows.extend(rows.into_iter().map(|r| TrackRow {
                    uri: r.uri.into(),
                    title: r.title.into(),
                    artist: r.artist.into(),
                    artist_uri: r.artist_uri.into(),
                    album: r.album.into(),
                    album_uri: r.album_uri.into(),
                    duration: clock(r.duration_ms).into(),
                    cover_id: r.cover.into(),
                    ..Default::default()
                }));
                // A list too short to scroll would never ask for its next chunk.
                if self.rows.row_count() < 60 {
                    app.invoke_more();
                }
            }
            Event::ListDone => {
                app.set_loading(false);
                app.set_has_more(false);
                if self.rows.row_count() == 0 {
                    app.set_status("Nothing here.".into());
                }
            }
            Event::Track { uri, title, artist, album, duration_ms, cover_url } => {
                app.set_track_title(title.as_str().into());
                app.set_track_artist(artist.as_str().into());
                app.set_track_album(album.as_str().into());
                app.set_total(clock(duration_ms).into());
                app.set_has_cover(false);
                app.set_status("".into());
                self.duration_ms = duration_ms;
                self.track_uri = uri;
                self.cover = None;
                self.set_lyrics(false, vec![], "");
                self.lyrics_uri.clear();
                self.want_lyrics();
                self.media_update(|m| {
                    m.set_metadata(MediaMetadata {
                        title: Some(&title),
                        artist: Some(&artist),
                        album: Some(&album),
                        cover_url: cover_url.as_deref(),
                        duration: Some(Duration::from_millis(duration_ms.into())),
                    })
                });
            }
            Event::Cover { uri, bytes } if uri == self.track_uri => match slint::Image::load_from_data(&bytes, None) {
                Ok(image) => {
                    if let Some(pixels) = image.to_rgba8() {
                        app.set_thumb(slint::Image::from_rgba8(art::shrink(&pixels, THUMB)));
                        app.set_has_cover(true);
                        self.set_look(art::look(&pixels));
                        self.cover = Some(pixels);
                        self.show_cover();
                    }
                }
                Err(e) => log::warn!("cover: {e}"),
            },
            // The song changed while its cover was loading.
            Event::Cover { .. } => {}
            Event::Thumb { id, bytes } => match slint::Image::load_from_data(&bytes, None) {
                Ok(image) => {
                    self.fill_cover(&id, image.clone());
                    self.thumbs.insert(id, Some(image));
                }
                Err(_) => {
                    self.thumbs.remove(&id);
                }
            },
            Event::Lyrics { uri, synced, lines, note } => {
                if uri == self.lyrics_uri && uri == self.track_uri {
                    self.set_lyrics(synced, lines, note);
                }
            }
            Event::Position { ms, playing } => self.set_playback(ms, playing),
            Event::Stopped => self.set_playback(0, false),
            Event::Volume(v) => app.set_volume(v as f32 / u16::MAX as f32),
            Event::Shuffle(on) => app.set_shuffle(on),
            Event::Repeat(mode) => app.set_repeat(mode.into()),
            Event::Quit => {
                self.save_settings();
                let _ = slint::quit_event_loop();
            }
        }
    }
}

/// Index of the line being sung, -1 before the first.
fn line_at(starts: &[u32], now: u32) -> i32 {
    starts.partition_point(|start| *start <= now) as i32 - 1
}

/// The native window appears some time after the event loop starts.
fn when_window_exists(tx: UnboundedSender<Cmd>) {
    slint::Timer::single_shot(Duration::from_millis(50), move || {
        let mut ready = true;
        with_ui(|ui| {
            ready = hwnd(&ui.app).is_some();
            if ready {
                ui.media = media_controls(&ui.app, tx.clone());
                ui.tint_title_bar();
            }
        });
        if !ready {
            when_window_exists(tx);
        }
    });
}

fn media_controls(app: &App, tx: UnboundedSender<Cmd>) -> Option<MediaControls> {
    let config = PlatformConfig { display_name: "Quaver", dbus_name: "quaver", hwnd: Some(hwnd(app)?) };
    let mut media = MediaControls::new(config).map_err(|e| log::warn!("media controls: {e:?}")).ok()?;
    let attached = media.attach(move |event| {
        let cmd = match event {
            MediaControlEvent::Play => Cmd::Resume,
            MediaControlEvent::Pause | MediaControlEvent::Stop => Cmd::Pause,
            MediaControlEvent::Toggle => Cmd::Toggle,
            MediaControlEvent::Next => Cmd::Next,
            MediaControlEvent::Previous => Cmd::Prev,
            MediaControlEvent::SetPosition(p) => Cmd::Seek(p.0.as_millis() as u32),
            _ => return,
        };
        let _ = tx.send(cmd);
    });
    attached.map_err(|e| log::warn!("media controls: {e:?}")).ok()?;
    log::info!("media controls attached");
    Some(media)
}

fn main() -> Result<(), slint::PlatformError> {
    env_logger::init();
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return Err("LOCALAPPDATA is not set".into());
    };
    let data_dir = PathBuf::from(local).join("Quaver");

    let settings_path = data_dir.join("settings.txt");
    let settings = Settings::load(&settings_path);
    let shared = Arc::new(Shared::new(settings.clone()));

    let app = App::new()?;
    let tray = Tray::new()?;
    let rows = Rc::new(VecModel::default());
    app.set_rows(rows.clone().into());
    app.set_message("Starting…".into());
    app.set_busy(true);

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    // Callbacks run while `with_ui` may be borrowed, so they only send commands.
    macro_rules! send {
        ($target:ident . $on:ident ( $($arg:ident),* ) $cmd:expr) => {{
            let tx = tx.clone();
            $target.$on(move |$($arg),*| { let _ = tx.send($cmd); });
        }};
    }
    send!(app.on_connect() Cmd::Connect);
    send!(app.on_logout() Cmd::Logout);
    send!(app.on_liked() Cmd::Liked);
    send!(app.on_queue() Cmd::Queue);
    send!(app.on_clear_queue() Cmd::ClearQueue);
    send!(app.on_open(title, uri) Cmd::Browse { title: title.into(), uri: uri.into() });
    send!(app.on_search(q) Cmd::Search(q.into()));
    send!(app.on_enqueue(uri) Cmd::Enqueue(uri.into()));
    send!(app.on_toggle() Cmd::Toggle);
    send!(app.on_next() Cmd::Next);
    send!(app.on_prev() Cmd::Prev);
    send!(app.on_set_volume(v) Cmd::Volume((v * u16::MAX as f32) as u16));
    send!(app.on_set_shuffle(on) Cmd::Shuffle(on));
    send!(app.on_set_repeat(mode) Cmd::Repeat(mode as u8));
    send!(tray.on_toggle() Cmd::Toggle);
    send!(tray.on_next() Cmd::Next);
    send!(tray.on_quit() Cmd::Quit);

    app.on_more({
        let (tx, app) = (tx.clone(), app.as_weak());
        move || {
            if let Some(app) = app.upgrade() {
                app.set_loading(true);
            }
            let _ = tx.send(Cmd::More);
        }
    });
    app.on_play({
        let (tx, app) = (tx.clone(), app.as_weak());
        move |uri| {
            if let Some(app) = app.upgrade() {
                let _ = tx.send(Cmd::Play { context: app.get_list_context().into(), track: uri.into() });
            }
        }
    });
    app.on_seek({
        let tx = tx.clone();
        move |fraction| with_ui(|ui| {
            let _ = tx.send(Cmd::Seek((fraction * ui.duration_ms as f32) as u32));
        })
    });
    app.on_art_resized(|size| {
        // Runs during layout, so the picture is replaced afterwards.
        let _ = slint::invoke_from_event_loop(move || with_ui(|ui| {
            ui.cover_size = (size * ui.app.window().scale_factor()) as u32;
            ui.show_cover();
        }));
    });
    app.on_toggle_fullscreen({
        let app = app.as_weak();
        move || if let Some(app) = app.upgrade() {
            app.window().set_fullscreen(!app.window().is_fullscreen());
        }
    });
    app.on_leave_fullscreen({
        let app = app.as_weak();
        move || if let Some(app) = app.upgrade() {
            app.window().set_fullscreen(false);
        }
    });
    // These can fire during layout or while `with_ui` is busy, so they run afterwards.
    app.on_need_cover(|id| {
        let _ = slint::invoke_from_event_loop(move || with_ui(|ui| ui.want_cover(id.into())));
    });
    app.on_lyrics_wanted(|| {
        let _ = slint::invoke_from_event_loop(|| with_ui(|ui| ui.want_lyrics()));
    });
    app.on_seek_line(|i| with_ui(|ui| {
        if let Some(start) = ui.lyrics.get(i as usize) {
            let _ = ui.tx.send(Cmd::Seek(*start));
        }
    }));
    app.on_set_eq_on(|on| with_ui(|ui| ui.change_settings(|s| s.eq_on = on)));
    app.on_set_band(|i, gain| with_ui(|ui| ui.change_settings(|s| {
        if let Some(band) = s.eq.get_mut(i as usize) {
            *band = gain.clamp(-settings::MAX_GAIN, settings::MAX_GAIN);
        }
    })));
    app.on_set_preset(|i| with_ui(|ui| ui.change_settings(|s| {
        if let Some((_, gains)) = eq::PRESETS.get(i as usize) {
            (s.eq, s.eq_on) = (*gains, true);
        }
    })));
    app.on_set_bitrate(|rate| with_ui(|ui| ui.change_settings(|s| s.bitrate = rate as u16)));
    app.on_set_normalise(|on| with_ui(|ui| ui.change_settings(|s| s.normalise = on)));
    app.on_set_tray(|on| with_ui(|ui| ui.change_settings(|s| s.tray = on)));
    app.on_set_tint(|tint| with_ui(|ui| ui.change_settings(|s| s.tint = tint.clamp(settings::MIN_TINT, 0.85))));
    app.set_presets(ModelRc::new(VecModel::from(eq::PRESETS.map(|p| p.0.into()).to_vec())));
    app.set_eq(ModelRc::new(VecModel::from(settings.eq.to_vec())));
    app.global::<Theme>().set_rim(art::glass_rim());
    app.on_tick(|| with_ui(|ui| ui.show_position()));
    app.window().on_close_requested({
        let (tx, shared) = (tx.clone(), shared.clone());
        move || {
            if !shared.lock().tray {
                let _ = tx.send(Cmd::Quit);
            }
            CloseRequestResponse::HideWindow
        }
    });
    tray.on_show_window({
        let app = app.as_weak();
        move || {
            if let Some(Err(e)) = app.upgrade().map(|app| app.show()) {
                log::error!("show window: {e}");
            }
        }
    });

    app.show()?;
    tray.show()?;
    when_window_exists(tx.clone());
    let mut ui = Ui {
        app,
        tray,
        rows,
        media: None,
        duration_ms: 0,
        position_ms: 0,
        since: Instant::now(),
        playing: false,
        title_bar: [0; 3],
        cover: None,
        cover_size: 0,
        track_uri: String::new(),
        tx,
        thumbs: HashMap::new(),
        lyrics: vec![],
        lyrics_uri: String::new(),
        shared: shared.clone(),
        settings_path,
        save: slint::Timer::default(),
    };
    ui.show_settings(&settings);
    ui.set_look(art::default_look());
    UI.set(Some(ui));

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
        let emit: backend::Emit = Arc::new(|event| {
            let _ = slint::invoke_from_event_loop(move || with_ui(|ui| ui.handle(event)));
        });
        runtime.block_on(backend::run(rx, emit, data_dir, shared));
        // A pending browser sign-in must not keep the process alive.
        runtime.shutdown_background();
    });

    let result = slint::run_event_loop_until_quit();
    UI.set(None);
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn lyrics_follow_the_position() {
        let starts = [1000, 2000, 2000, 5000];
        assert_eq!([0, 1000, 1999, 2000, 9000].map(|t| super::line_at(&starts, t)), [-1, 0, 0, 2, 3]);
        assert_eq!(super::line_at(&[], 10), -1);
    }

    #[test]
    fn clock_formats() {
        assert_eq!(super::clock(0), "0:00");
        assert_eq!(super::clock(61_999), "1:01");
        assert_eq!(super::clock(3_661_000), "1:01:01");
    }
}
