#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod art;
mod backend;
mod eq;
mod history;
mod sequence;
mod settings;
mod soloist;

use std::{
    cell::{Cell, RefCell}, collections::HashMap, ffi::c_void, path::PathBuf, rc::Rc,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use art::{STYLES, Style};
use backend::{Cmd, Event};
use history::{History, Page};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use settings::{Settings, Shared};
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent}};
use slint::{CloseRequestResponse, ComponentHandle, Model, ModelRc, VecModel};
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};
use tokio::sync::mpsc::UnboundedSender;

slint::include_modules!();

/// Side of the cover in the panel on the right, as in app.slint.
const PANEL_COVER: f32 = 256.;
/// Side of the cover in the player bar, as in app.slint.
const THUMB: f32 = 66.;
/// How long one song's look takes to become the next one's.
const FADE_MS: i64 = 700;
/// Panels that are painted: library, main, player bar, now playing.
const PANELS: usize = 4;
const STARTING: &str = "Starting…";
/// Longest double-click Windows allows by default.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const TYPING_PAUSE: Duration = Duration::from_millis(300);
/// Shorter searches while typing would mostly be noise.
const MIN_SEARCH: usize = 2;
// shortcut: forgotten all at once when full, use an LRU if scrolling back refetches too much.
const MAX_PICTURES: usize = 600;

/// Where a picture is shown, as in the UI: its side and how round its corners are.
const KINDS: [(f32, f32); 4] = [(48., 0.22), (156., 0.1), (156., 0.5), (88., 0.16)];
const SMALL: i32 = 0;
const HEADER: i32 = 3;

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
    /// Pictures by kind and id; `None` while one is being fetched.
    pictures: HashMap<(i32, String), Option<slint::Image>>,
    playlists: Rc<VecModel<Card>>,
    tiles: Rc<VecModel<Card>>,
    shelves: Vec<Rc<VecModel<Card>>>,
    /// Start of each lyrics line. Belongs to `lyrics_uri`.
    lyrics: Vec<u32>,
    lyrics_uri: String,
    shared: Arc<Shared>,
    settings_path: PathBuf,
    data_dir: PathBuf,
    save: slint::Timer,
    /// The palette on screen, and the fade to the next one.
    palette: art::Palette,
    fade: slint::Timer,
    /// What is painted behind the content. The palette in it is the one being faded to.
    scene: art::Scene,
    panels: [art::Panel; PANELS],
    /// Counts paintings, so that only the latest is shown.
    painting: u32,
    repaint: slint::Timer,
    /// Whether the next painting is a new look, which fades in.
    new_look: bool,
    tidy: slint::Timer,
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
    /// Fills in a DWM_TIMING_INFO, of which only two fields are read (see `Refresh`).
    fn DwmGetCompositionTimingInfo(hwnd: *mut c_void, info: *mut [u8; TIMING_INFO]) -> i32;
    fn DwmSetWindowAttribute(hwnd: *mut c_void, attribute: u32, value: *const u32, size: u32) -> i32;
}

/// Size of DWM_TIMING_INFO, which has no padding.
const TIMING_INFO: usize = 292;

/// When the screen is drawn, in seconds of the system's fine clock.
#[derive(Clone, Copy)]
struct Refresh {
    now: f64,
    /// The next time the screen is drawn.
    next: f64,
}

impl Refresh {
    fn get() -> Self {
        let (mut ticks, mut per_second) = (0, 0);
        unsafe {
            QueryPerformanceCounter(&mut ticks);
            QueryPerformanceFrequency(&mut per_second);
        }
        let seconds = |ticks: f64| ticks / (per_second.max(1) as f64);
        let now = seconds(ticks as f64);
        let mut info = [0; TIMING_INFO];
        info[..4].copy_from_slice(&(TIMING_INFO as u32).to_le_bytes());
        let known = unsafe { DwmGetCompositionTimingInfo(std::ptr::null_mut(), &mut info) } == 0;
        let field = |at: usize| u64::from_le_bytes(std::array::from_fn(|i| info[at + i]));
        let period = seconds(field(12) as f64);
        // Without an answer, or with a strange one, assume an ordinary screen.
        if !known || !(0.002..0.05).contains(&period) {
            return Self { now, next: now + 1. / 60. };
        }
        Self { now, next: next_refresh(now, seconds(field(28) as f64), period) }
    }
}

/// The first of the refreshes, one of which was at `some`, that is clearly after `now`.
fn next_refresh(now: f64, some: f64, period: f64) -> f64 {
    let ahead = ((now - some) / period + 0.1).ceil();
    some + ahead * period
}

fn hwnd(app: &App) -> Option<*mut c_void> {
    match app.window().window_handle().window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win) => Some(win.hwnd.get() as *mut c_void),
        _ => None,
    }
}

/// Opens a page or folder the way a double-click would.
fn open(target: &std::ffi::OsStr) {
    if let Err(e) = std::process::Command::new("explorer").arg(target).spawn() {
        log::warn!("open: {e}");
    }
}

/// Puts plain text on the clipboard.
fn copy(text: &str) -> std::io::Result<()> {
    use std::{io::Write, os::windows::process::CommandExt};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut clip = std::process::Command::new("clip.exe").stdin(std::process::Stdio::piped()).creation_flags(CREATE_NO_WINDOW).spawn()?;
    if let Some(mut input) = clip.stdin.take() {
        input.write_all(text.as_bytes())?;
    }
    clip.wait().map(|_| ())
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

    fn want_picture(&mut self, kind: i32, id: String) {
        if id.is_empty() || !(0..KINDS.len() as i32).contains(&kind) {
            return;
        }
        if self.pictures.len() >= MAX_PICTURES {
            self.pictures.clear();
        }
        let key = (kind, id);
        match self.pictures.get(&key) {
            Some(Some(image)) => self.fill_picture(kind, &key.1, image.clone()),
            Some(None) => {}
            None => {
                let _ = self.tx.send(Cmd::Picture { kind, id: key.1.clone() });
                self.pictures.insert(key, None);
            }
        }
    }

    fn got_picture(&mut self, kind: i32, id: String, images: Vec<Vec<u8>>) {
        let (side, rounding) = KINDS[kind as usize];
        let size = (side * self.app.window().scale_factor()) as u32;
        let pictures: Vec<_> = images
            .iter()
            .filter_map(|bytes| slint::Image::load_from_data(bytes, None).ok()?.to_rgba8())
            .collect();
        match art::mosaic(&pictures, size) {
            Some(picture) => {
                let image = slint::Image::from_rgba8(art::jewel(&picture, size, rounding, self.scene.style.gloss));
                self.fill_picture(kind, &id, image.clone());
                self.pictures.insert((kind, id), Some(image));
            }
            None => {
                self.pictures.remove(&(kind, id));
            }
        }
    }

    fn fill_picture(&self, kind: i32, id: &str, image: slint::Image) {
        let fill = |cards: &VecModel<Card>| {
            for (i, mut card) in cards.iter().enumerate() {
                if card.picture_id == id && card.kind == kind && !card.has_picture {
                    (card.picture, card.has_picture) = (image.clone(), true);
                    cards.set_row_data(i, card);
                }
            }
        };
        fill(&self.playlists);
        fill(&self.tiles);
        self.shelves.iter().for_each(|s| fill(s));
        if kind == SMALL {
            for (i, mut row) in self.rows.iter().enumerate() {
                if row.cover_id == id && !row.has_cover {
                    (row.cover, row.has_cover) = (image.clone(), true);
                    self.rows.set_row_data(i, row);
                }
            }
        }
        if kind == HEADER && self.app.get_list_picture_id() == id {
            self.app.set_list_picture(image);
            self.app.set_list_has_picture(true);
        }
    }

    /// Pictures are drawn for one scale; another monitor may have another.
    fn redraw_pictures(&mut self) {
        self.pictures.clear();
        let mut wanted = Vec::new();
        for cards in [&self.playlists, &self.tiles].into_iter().chain(&self.shelves) {
            for (i, mut card) in cards.iter().enumerate() {
                if std::mem::take(&mut card.has_picture) {
                    wanted.push((card.kind, card.picture_id.to_string()));
                    cards.set_row_data(i, card);
                }
            }
        }
        for (i, mut row) in self.rows.iter().enumerate() {
            if std::mem::take(&mut row.has_cover) {
                wanted.push((SMALL, row.cover_id.to_string()));
                self.rows.set_row_data(i, row);
            }
        }
        if self.app.get_list_has_picture() {
            wanted.push((HEADER, self.app.get_list_picture_id().into()));
            self.app.set_list_has_picture(false);
        }
        wanted.into_iter().for_each(|(kind, id)| self.want_picture(kind, id));
        self.show_cover();
    }

    fn change_settings(&mut self, change: impl FnOnce(&mut Settings)) {
        let retint;
        let settings = {
            let mut settings = self.shared.lock();
            let old = settings.clone();
            change(&mut settings);
            retint = old.tint != settings.tint;
            if (old.bitrate, old.normalise) != (settings.bitrate, settings.normalise) {
                self.app.set_needs_restart(true);
            }
            settings.clone()
        };
        self.shared.eq_version.fetch_add(1, Ordering::Release);
        self.show_settings(&settings);
        if retint {
            self.repaint_soon();
        }
        // A dragged slider changes many times a second; write once it rests.
        self.save.start(slint::TimerMode::SingleShot, Duration::from_millis(600), || with_ui(|ui| ui.save_settings()));
    }

    /// Which of the setup steps are done.
    fn show_soloist(&self) {
        self.app.set_soloist_here(soloist::Setup::find_exe(&self.data_dir).is_some());
        self.app.set_soloist_has_key(std::fs::metadata(soloist::Setup::key_file(&self.data_dir)).is_ok_and(|f| f.len() > 0));
    }

    fn save_soloist_key(&mut self, key: &str) {
        match std::fs::write(soloist::Setup::key_file(&self.data_dir), key) {
            Ok(()) => {
                self.show_soloist();
                self.app.set_player_note("Key saved. Starting Soloist…".into());
                self.change_settings(|s| s.soloist = true);
                let _ = self.tx.send(Cmd::Restart);
            }
            Err(e) => self.app.set_player_note(format!("The key could not be saved: {e}").into()),
        }
    }

    fn save_settings(&mut self) {
        self.save.stop();
        let settings = self.shared.lock().clone();
        if let Err(e) = settings.save(&self.settings_path) {
            self.toast(&format!("Settings could not be saved: {e}"));
        }
    }

    fn show_settings(&self, s: &Settings) {
        let app = &self.app;
        app.set_eq_on(s.eq_on);
        app.set_bitrate(s.bitrate.into());
        app.set_normalise(s.normalise);
        app.set_tray(s.tray);
        app.global::<Theme>().set_motion(s.motion);
        app.set_soloist(s.soloist);
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
        self.scene.backdrop = look.backdrop;
        self.scene.palette = look.palette;
        self.new_look = true;
        self.repaint_soon();
        self.fade_to(look.palette);
        self.title_bar = look.edge;
        self.tint_title_bar();
    }

    /// The parts of a theme that aren't colors.
    fn show_style(&self) {
        let (theme, style) = (self.app.global::<Theme>(), self.scene.style);
        let [r, g, b] = style.over();
        theme.set_over(slint::Color::from_rgb_u8(r, g, b));
        theme.set_bright(style.bright);
        theme.set_gloss(style.gloss);
        theme.set_pointer_light(style.glow);
        self.app.set_theme(style.id.into());
        self.app.set_glass_theme(style.id == STYLES[0].id);
    }

    fn set_style(&mut self, style: &'static Style) {
        self.scene.style = style;
        self.show_style();
        self.set_look(art::look(self.cover.as_ref(), style));
        // Gloss is painted into the pictures.
        self.redraw_pictures();
    }

    /// Pictures of every theme in the colors of the current cover.
    fn show_themes(&self) {
        let scale = self.app.window().scale_factor();
        let size = ((176. * scale) as u32, (103. * scale) as u32);
        let tint = self.shared.lock().tint;
        let themes = STYLES.iter().map(|style| ThemeCard {
            id: style.id.into(),
            name: style.name.into(),
            preview: slint::Image::from_rgb8(art::preview(self.cover.as_ref(), style, tint, size)),
        });
        self.app.set_themes(ModelRc::new(VecModel::from(themes.collect::<Vec<_>>())));
    }

    /// Says something where it can't be missed.
    fn toast(&self, text: &str) {
        self.app.set_toast(text.into());
        self.app.invoke_show_toast();
    }

    fn place(&mut self, panel: i32, at: art::Panel) {
        if let Some(old) = (panel as usize).checked_sub(1).and_then(|i| self.panels.get_mut(i))
            && *old != at
        {
            *old = at;
            self.repaint_soon();
        }
    }

    /// Collects changes for a moment; a window being resized makes many.
    fn repaint_soon(&mut self) {
        if !self.repaint.running() {
            self.repaint.start(slint::TimerMode::SingleShot, Duration::from_millis(24), || with_ui(|ui| ui.paint()));
        }
    }

    fn paint(&mut self) {
        let size = self.app.window().size();
        let mut scene = self.scene.clone();
        scene.size = (size.width, size.height);
        scene.scale = self.app.window().scale_factor();
        scene.tint = self.shared.lock().tint;
        let panels = self.app.get_ready() && !self.app.get_now_playing();
        scene.panels = self.panels.iter().filter(|p| panels && p.width > 0.).copied().collect();
        self.painting += 1;
        let (painting, fade) = (self.painting, std::mem::take(&mut self.new_look));
        std::thread::spawn(move || {
            let picture = scene.paint();
            let _ = slint::invoke_from_event_loop(move || with_ui(|ui| {
                if painting == ui.painting {
                    ui.show_scene(slint::Image::from_rgb8(picture), fade);
                } else {
                    // The newer painting has to fade in instead.
                    ui.new_look |= fade;
                }
            }));
        });
    }

    /// Two layers cross-fade; a new look always goes to the hidden one.
    fn show_scene(&mut self, picture: slint::Image, fade: bool) {
        let scene = self.app.global::<Scene>();
        let motion = self.shared.lock().motion;
        let on_b = scene.get_show_b() != fade;
        if on_b { scene.set_b(picture) } else { scene.set_a(picture) }
        if fade {
            scene.set_fade(if motion { FADE_MS } else { 0 });
            scene.set_show_b(on_b);
        }
        // The hidden layer is as big as the window; free it once it can't be seen.
        self.tidy.start(slint::TimerMode::SingleShot, Duration::from_millis(FADE_MS as u64 + 200), || with_ui(|ui| {
            let scene = ui.app.global::<Scene>();
            if scene.get_show_b() { scene.set_a(Default::default()) } else { scene.set_b(Default::default()) }
        }));
    }

    fn fade_to(&mut self, to: art::Palette) {
        if !self.shared.lock().motion {
            self.fade.stop();
            return self.show_palette(to);
        }
        let (from, start) = (self.palette, Instant::now());
        self.fade.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || with_ui(|ui| {
            let t = (start.elapsed().as_millis() as f32 / FADE_MS as f32).min(1.);
            ui.show_palette(from.fade(&to, t * t * (3. - 2. * t)));
            if t >= 1. {
                ui.fade.stop();
            }
        }));
    }

    fn show_palette(&mut self, p: art::Palette) {
        self.palette = p;
        let theme = self.app.global::<Theme>();
        let color = |[r, g, b]: [u8; 3]| slint::Color::from_rgb_u8(r, g, b);
        theme.set_accent(color(p.accent));
        theme.set_fill(color(p.fill));
        theme.set_fill2(color(p.fill2));
        theme.set_ink(color(p.ink));
        theme.set_glass(color(p.tint));
        theme.set_text(color(p.text));
        theme.set_dim(color(p.dim));
    }

    fn show_cover(&self) {
        if let Some(pixels) = &self.cover {
            let thumb = (THUMB * self.app.window().scale_factor()) as u32;
            self.app.set_thumb(slint::Image::from_rgba8(art::jewel(pixels, thumb, 0.18, self.scene.style.gloss)));
            let panel = (PANEL_COVER * self.app.window().scale_factor()) as u32;
            self.app.set_panel_cover(slint::Image::from_rgba8(art::jewel(pixels, panel, 0.08, self.scene.style.gloss)));
            // Rounded by 24 logical pixels, whatever its size.
            let size = if self.cover_size == 0 { pixels.width() } else { self.cover_size };
            let rounding = 24. * self.app.window().scale_factor() / size.max(1) as f32;
            self.app.set_cover(slint::Image::from_rgba8(art::jewel(pixels, size, rounding, self.scene.style.gloss)));
        }
    }

    fn tint_title_bar(&self) {
        if let Some(hwnd) = hwnd(&self.app) {
            let [r, g, b] = self.title_bar;
            let set = |attribute, value: u32| unsafe {
                DwmSetWindowAttribute(hwnd, attribute, &raw const value, size_of::<u32>() as u32)
            };
            // Purely cosmetic, so older Windows versions may refuse it.
            set(DWMWA_USE_IMMERSIVE_DARK_MODE, !self.scene.style.bright as u32);
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
            Event::Status(text) => {
                app.set_status(text.as_str().into());
                // An empty list shows it in its place.
                if app.get_home_open() || self.rows.row_count() > 0 {
                    self.toast(&text);
                }
            }
            Event::Jam(jam) => {
                let jam = jam.unwrap_or_default();
                let names: Vec<slint::SharedString> = jam.names.into_iter().map(Into::into).collect();
                app.set_jam(ModelRc::new(VecModel::from(names)));
                app.set_jam_link(jam.link.into());
            }
            Event::Blocked => {
                app.set_toast("".into());
                app.set_blocked(true);
            }
            Event::Playlists(list) => self.playlists.set_vec(list.into_iter().map(|c| card(c, SMALL)).collect::<Vec<_>>()),
            Event::Home(shelves) => {
                app.set_home_note("".into());
                app.set_greeting(greeting(local_hour()).into());
                let mut shelves = shelves.into_iter();
                let tiles = shelves.next().map(|s| s.cards).unwrap_or_default();
                self.tiles.set_vec(tiles.into_iter().take(8).map(|c| card(c, SMALL)).collect::<Vec<_>>());
                self.shelves.clear();
                let mut shown = Vec::new();
                for shelf in shelves {
                    let cards = shelf.cards.into_iter().map(|c| {
                        let kind = if c.uri.starts_with("spotify:artist:") { 2 } else { 1 };
                        card(c, kind)
                    });
                    let cards = Rc::new(VecModel::from(cards.collect::<Vec<_>>()));
                    shown.push(Shelf { title: shelf.title.into(), cards: cards.clone().into() });
                    self.shelves.push(cards);
                }
                app.set_shelves(ModelRc::new(VecModel::from(shown)));
            }
            Event::NoHome(note) => app.set_home_note(note.into()),
            Event::List { title, context } => {
                self.rows.set_vec(vec![]);
                // The queue refreshes by itself and must not close the home page.
                let refresh = title == "Up next" && app.get_list_title() == "Up next";
                if !refresh {
                    app.set_home_open(false);
                }
                if context.as_str() != app.get_list_context().as_str() && app.get_opening() != context.as_str() {
                    app.set_list_picture_id("".into());
                    app.set_list_has_picture(false);
                }
                app.set_list_title(title.into());
                app.set_list_context(context.into());
                app.set_status("".into());
                app.set_loading(true);
                app.set_has_more(true);
                // What is left of a glide belongs to the old list.
                app.invoke_stop_glide();
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
                        app.set_has_cover(true);
                        self.set_look(art::look(Some(&pixels), self.scene.style));
                        self.cover = Some(pixels);
                        self.show_cover();
                    }
                }
                Err(e) => log::warn!("cover: {e}"),
            },
            // The song changed while its cover was loading.
            Event::Cover { .. } => {}
            Event::Picture { kind, id, images } => self.got_picture(kind, id, images),
            Event::Lyrics { uri, synced, lines, note } => {
                if uri == self.lyrics_uri && uri == self.track_uri {
                    self.set_lyrics(synced, lines, note);
                }
            }
            Event::Position { ms, playing } => {
                if playing && app.get_toast() == STARTING {
                    app.set_toast("".into());
                }
                self.set_playback(ms, playing);
            }
            Event::Stopped => self.set_playback(0, false),
            Event::Volume(v) => app.set_volume(v as f32 / u16::MAX as f32),
            Event::Shuffle(on) => app.set_shuffle(on),
            Event::Repeat(mode) => app.set_repeat(mode.into()),
            Event::Player(note) => {
                app.set_player_note(note.as_str().into());
                if !note.is_empty() {
                    self.toast(&note);
                }
            }
            Event::Quit => {
                self.save_settings();
                let _ = slint::quit_event_loop();
            }
        }
    }
}

fn card(c: backend::Card, kind: i32) -> Card {
    Card { title: c.title.into(), subtitle: c.subtitle.into(), uri: c.uri.into(), picture_id: c.picture.into(), kind, ..Default::default() }
}

fn greeting(hour: u16) -> &'static str {
    match hour {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(ticks: *mut i64) -> i32;
    fn QueryPerformanceFrequency(per_second: *mut i64) -> i32;
    /// Fills in a SYSTEMTIME: year, month, weekday, day, hour, minute, second, milliseconds.
    fn GetLocalTime(time: *mut [u16; 8]);
}

fn local_hour() -> u16 {
    let mut time = [0; 8];
    unsafe { GetLocalTime(&mut time) };
    time[4]
}

/// Index of the line being sung, -1 before the first.
fn line_at(starts: &[u32], now: u32) -> i32 {
    starts.partition_point(|start| *start <= now) as i32 - 1
}

/// Feeds the pointer to the UI and turns the wheel into gliding scrolling. This only talks
/// to the window: Windows can deliver events while `with_ui` is busy.
fn follow_pointer(app: &App) {
    let glide = Rc::new(Glide { spring: Cell::default(), running: Cell::new(false), last: Cell::new(0.), rest: Cell::new(0.) });
    app.on_glide({
        let (glide, app) = (glide.clone(), app.as_weak());
        move |distance| {
            if let Some(app) = app.upgrade() {
                glide.push(&app, distance);
            }
        }
    });
    app.on_stop_glide({
        let (glide, app) = (glide.clone(), app.as_weak());
        move || {
            if let Some(app) = app.upgrade() {
                glide.stop(&app);
                app.set_bounce(0.);
            }
        }
    });
    let weak = app.as_weak();
    // Pointers report far more often than there are frames.
    let pointer = Rc::new((Cell::new((0., 0.)), slint::Timer::default()));
    app.window().on_winit_window_event(move |window, event| {
        let Some(app) = weak.upgrade() else { return EventResult::Propagate };
        let scene = app.global::<Scene>();
        match event {
            WindowEvent::RedrawRequested => {
                glide.frame(&app);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = window.scale_factor();
                pointer.0.set((position.x as f32 / scale, position.y as f32 / scale));
                if !pointer.1.running() {
                    let (weak, latest) = (app.as_weak(), Rc::downgrade(&pointer));
                    pointer.1.start(slint::TimerMode::SingleShot, Duration::from_millis(12), move || {
                        if let (Some(app), Some(latest)) = (weak.upgrade(), latest.upgrade()) {
                            let scene = app.global::<Scene>();
                            let (x, y) = latest.0.get();
                            scene.set_mouse_x(x);
                            scene.set_mouse_y(y);
                            scene.set_hovering(true);
                        }
                    });
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                // Covers are drawn for one scale; the event may arrive while `with_ui` is busy.
                let _ = slint::invoke_from_event_loop(|| with_ui(|ui| ui.redraw_pictures()));
            }
            // The side buttons of a mouse.
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Back, .. } => app.invoke_go_back(),
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Forward, .. } => app.invoke_go_forward(),
            WindowEvent::CursorLeft { .. } => {
                pointer.1.stop();
                scene.set_hovering(false);
            }
            WindowEvent::MouseWheel { delta, .. } if app.global::<Theme>().get_motion() && app.invoke_over_list() => {
                let distance = match delta {
                    MouseScrollDelta::LineDelta(_, lines) => lines * 96.,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / window.scale_factor(),
                };
                glide.push(&app, distance);
                return EventResult::PreventDefault;
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

/// Scrolls on a spring: it picks up speed and settles softly. Each frame is placed where the list
/// should be when the screen next shows it, and on whole pixels, so that everything moves together.
struct Glide {
    spring: Cell<Spring>,
    running: Cell<bool>,
    /// The refresh the last frame was made for.
    last: Cell<f64>,
    /// Part of a pixel not moved yet.
    rest: Cell<f32>,
}

#[derive(Clone, Copy, Default)]
struct Spring {
    left: f32,
    speed: f32,
}

impl Glide {
    fn push(&self, app: &App, distance: f32) {
        let mut spring = self.spring.get();
        spring.push(distance);
        self.spring.set(spring);
        if !self.running.replace(true) {
            self.last.set(Refresh::get().now);
            self.rest.set(0.);
            app.set_gliding(true);
            request_frame(app);
        }
    }

    fn stop(&self, app: &App) {
        self.spring.set(Spring::default());
        self.running.set(false);
        app.set_gliding(false);
    }

    /// Called right before a frame is drawn.
    fn frame(&self, app: &App) {
        if !self.running.get() {
            return;
        }
        let refresh = Refresh::get();
        let passed = refresh.next - self.last.get();
        // This refresh already has its frame.
        if passed < 0.001 {
            return;
        }
        self.last.set(refresh.next);
        let mut spring = self.spring.get();
        // After a hiccup, go on from here instead of jumping.
        let moved = spring.step(Duration::from_secs_f64(passed.min(0.05)));
        self.spring.set(spring);
        let scale = app.window().scale_factor();
        let wanted = self.rest.get() + moved;
        let whole = (wanted * scale).round() / scale;
        self.rest.set(wanted - whole);
        let stopped = whole != 0. && app.invoke_scroll_by(whole);
        if stopped {
            // The end of the list gives a little and springs back.
            app.set_bounce((spring.speed * 0.02).clamp(-30., 30.));
            let weak = app.as_weak();
            slint::Timer::single_shot(Duration::from_millis(110), move || {
                if let Some(app) = weak.upgrade() {
                    app.set_bounce(0.);
                }
            });
        }
        if stopped || spring.at_rest() {
            return self.stop(app);
        }
        let weak = app.as_weak();
        // The next frame starts right after the refresh, with all the time until the one after.
        let wait = Duration::from_secs_f64(refresh.next - refresh.now + 0.0005);
        slint::Timer::single_shot(wait, move || {
            if let Some(app) = weak.upgrade() {
                request_frame(&app);
            }
        });
    }
}

/// Asks the system directly: Slint's own requests are on a timer that isn't in step with the screen.
fn request_frame(app: &App) {
    app.window().request_redraw();
    app.window().with_winit_window(|window| window.request_redraw());
}

impl Spring {
    /// Turning the wheel back stops the movement instead of fighting it.
    fn push(&mut self, distance: f32) {
        if self.left * distance < 0. {
            *self = Self::default();
        }
        self.left += distance;
    }

    /// Critically damped, so it never overshoots. Returns how far to move.
    fn step(&mut self, time: Duration) -> f32 {
        const OMEGA: f32 = 17.;
        let time = time.as_secs_f32();
        let decay = (-OMEGA * time).exp();
        let pull = self.speed - OMEGA * self.left;
        let left = (self.left - pull * time) * decay;
        self.speed = (self.speed - OMEGA * pull * time) * decay;
        if left.abs() < 0.4 && self.speed.abs() < 12. {
            self.speed = 0.;
            return std::mem::take(&mut self.left);
        }
        let moved = self.left - left;
        self.left = left;
        moved
    }

    fn at_rest(&self) -> bool {
        self.left == 0. && self.speed == 0.
    }
}

/// The native window appears some time after the event loop starts.
fn when_window_exists(tx: UnboundedSender<Cmd>) {
    slint::Timer::single_shot(Duration::from_millis(50), move || {
        let mut ready = true;
        with_ui(|ui| {
            ready = hwnd(&ui.app).is_some();
            if ready {
                ui.media = media_controls(&ui.app, tx.clone());
                ui.repaint_soon();
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
    let (playlists, tiles) = (Rc::new(VecModel::default()), Rc::new(VecModel::default()));
    app.set_playlists(playlists.clone().into());
    app.set_tiles(tiles.clone().into());
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
    let history = Rc::new(RefCell::new(History::default()));
    // Every page is opened by one of these four, also when going back to it.
    let visit = {
        let (history, app) = (history.clone(), app.as_weak());
        move |page| {
            let mut history = history.borrow_mut();
            history.visit(page);
            if let Some(app) = app.upgrade() {
                app.set_can_go_back(history.can_go_back());
                app.set_can_go_forward(history.can_go_forward());
            }
        }
    };
    app.on_liked({
        let (tx, visit) = (tx.clone(), visit.clone());
        move || {
            visit(Page::Liked);
            let _ = tx.send(Cmd::Liked);
        }
    });
    app.on_queue({
        let (tx, visit) = (tx.clone(), visit.clone());
        move || {
            visit(Page::Queue);
            let _ = tx.send(Cmd::Queue);
        }
    });
    app.on_load_home({
        let (tx, visit) = (tx.clone(), visit.clone());
        move || {
            visit(Page::Home);
            let _ = tx.send(Cmd::Home { again: false });
        }
    });
    let go = {
        let (history, app) = (history.clone(), app.as_weak());
        move |step: fn(&mut History) -> Option<Page>| {
            let Some(app) = app.upgrade() else { return };
            // The borrow has to end before a page is opened, which visits it.
            let page = step(&mut history.borrow_mut());
            match page {
                None => return,
                Some(Page::Home) => app.invoke_go_home(),
                Some(Page::Liked) => {
                    app.invoke_show_list("liked".into(), "".into());
                    app.invoke_liked();
                }
                Some(Page::Queue) => {
                    app.invoke_show_list("".into(), "".into());
                    app.invoke_queue();
                }
                Some(Page::List { title, uri, picture }) => {
                    app.invoke_show_list(uri.as_str().into(), picture.into());
                    app.invoke_open(title.into(), uri.into());
                }
                Some(Page::Search(q)) => {
                    app.set_settings_open(false);
                    app.invoke_search(q.into());
                }
            }
            let history = history.borrow();
            app.set_can_go_back(history.can_go_back());
            app.set_can_go_forward(history.can_go_forward());
        }
    };
    app.on_go_back({
        let (go, app) = (go.clone(), app.as_weak());
        move || {
            // What lies over the page goes first.
            let Some(app) = app.upgrade() else { return };
            if app.get_now_playing() {
                app.set_now_playing(false);
            } else if app.get_settings_open() {
                app.set_settings_open(false);
            } else {
                go(History::back);
            }
        }
    });
    app.on_go_forward(move || go(History::forward));
    send!(app.on_retry_home() Cmd::Home { again: true });
    send!(app.on_clear_queue() Cmd::ClearQueue);
    app.on_open({
        let (tx, visit, app) = (tx.clone(), visit.clone(), app.as_weak());
        move |title, uri| {
            let picture = app.upgrade().map(|app| app.get_list_picture_id().into()).unwrap_or_default();
            visit(Page::List { title: title.as_str().into(), uri: uri.as_str().into(), picture });
            let _ = tx.send(Cmd::Browse { title: title.into(), uri: uri.into() });
        }
    });
    send!(app.on_enqueue(uri) Cmd::Enqueue(uri.into()));
    app.on_toggle({
        let (tx, app) = (tx.clone(), app.as_weak());
        move || {
            let Some(app) = app.upgrade() else { return };
            if app.get_track_title().is_empty() {
                app.set_toast("Nothing is loaded yet. Pick a song or a playlist first.".into());
                app.invoke_show_toast();
            } else {
                let _ = tx.send(Cmd::Toggle);
            }
        }
    });
    send!(app.on_next() Cmd::Next);
    send!(app.on_prev() Cmd::Prev);
    send!(app.on_set_volume(v) Cmd::Volume((v * u16::MAX as f32) as u16));
    send!(app.on_set_shuffle(on) Cmd::Shuffle(on));
    send!(app.on_set_repeat(mode) Cmd::Repeat(mode as u8));
    send!(tray.on_toggle() Cmd::Toggle);
    send!(tray.on_next() Cmd::Next);
    send!(tray.on_quit() Cmd::Quit);

    // Typing searches by itself after a short pause; Enter doesn't wait.
    let typing = Rc::new(slint::Timer::default());
    app.on_search({
        let (tx, typing, app) = (tx.clone(), typing.clone(), app.as_weak());
        move |q| {
            typing.stop();
            visit(Page::Search(q.as_str().into()));
            if let Some(app) = app.upgrade() {
                app.set_loading(true);
            }
            let _ = tx.send(Cmd::Search(q.into()));
        }
    });
    app.on_typed({
        let app = app.as_weak();
        move |q| {
            typing.stop();
            if q.trim().chars().count() < MIN_SEARCH {
                return;
            }
            let app = app.clone();
            typing.start(slint::TimerMode::SingleShot, TYPING_PAUSE, move || {
                if let Some(app) = app.upgrade() {
                    app.set_settings_open(false);
                    app.invoke_search(q.clone());
                }
            });
        }
    });
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
        let last = RefCell::new((slint::SharedString::default(), Instant::now()));
        move |uri| {
            // A cover plays on one click and its row on two, so a double-click asks three times.
            let (song, when) = last.replace((uri.clone(), Instant::now()));
            if song == uri && when.elapsed() < DOUBLE_CLICK {
                return;
            }
            if let Some(app) = app.upgrade() {
                app.set_toast(STARTING.into());
                app.invoke_show_toast();
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
    app.on_need_picture(|kind, id| {
        let _ = slint::invoke_from_event_loop(move || with_ui(|ui| ui.want_picture(kind, id.into())));
    });
    app.on_play_list({
        let (tx, app) = (tx.clone(), app.as_weak());
        move |uri| {
            if let Some(app) = app.upgrade() {
                app.set_toast(STARTING.into());
                app.invoke_show_toast();
            }
            let _ = tx.send(Cmd::Play { context: uri.into(), track: String::new() });
        }
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
    app.on_set_soloist(|on| with_ui(|ui| {
        ui.change_settings(|s| s.soloist = on);
        let _ = ui.tx.send(Cmd::Restart);
    }));
    app.on_soloist_key(|key| with_ui(|ui| ui.save_soloist_key(key.trim())));
    app.on_soloist_folder(|| with_ui(|ui| {
        let dir = soloist::Setup::dir(&ui.data_dir);
        match std::fs::create_dir_all(&dir) {
            Ok(()) => open(dir.as_os_str()),
            Err(e) => ui.app.set_player_note(format!("The folder could not be made: {e}").into()),
        }
    }));
    app.on_discord(|| open("discord://-/settings/connections".as_ref()));
    app.on_copy_jam_link(|| with_ui(|ui| {
        ui.toast(match copy(&ui.app.get_jam_link()) {
            Ok(()) => "Invite link copied.",
            Err(_) => "The link could not be copied.",
        });
    }));
    app.on_soloist_about(|| open("https://developer.spotify.com/documentation/soloist".as_ref()));
    app.on_soloist_download(|| with_ui(|ui| {
        ui.app.set_downloading(true);
        ui.app.set_player_note("Downloading Soloist from Spotify…".into());
        // A running Soloist would keep its files from being replaced.
        if ui.shared.lock().soloist {
            ui.change_settings(|s| s.soloist = false);
            let _ = ui.tx.send(Cmd::Restart);
        }
        let dir = ui.data_dir.clone();
        std::thread::spawn(move || {
            let done = soloist::download(&dir);
            let _ = slint::invoke_from_event_loop(move || with_ui(|ui| {
                ui.app.set_downloading(false);
                ui.app.set_player_note(match done {
                    Ok(()) => "Soloist is here. Now add your key.".into(),
                    Err(e) => format!("Soloist could not be downloaded: {e}").into(),
                });
                ui.show_soloist();
                if soloist::Setup::load(&ui.data_dir).is_ok() {
                    ui.change_settings(|s| s.soloist = true);
                    let _ = ui.tx.send(Cmd::Restart);
                }
            }));
        });
    }));
    app.on_set_theme_id(|id| with_ui(|ui| {
        let style = Style::by_id(&id);
        ui.change_settings(|s| s.theme = style.id.into());
        ui.set_style(style);
    }));
    app.on_settings_shown(|| {
        let _ = slint::invoke_from_event_loop(|| with_ui(|ui| {
            ui.show_themes();
            ui.show_soloist();
        }));
    });
    app.on_soloist_get_key(|| open("https://developer.spotify.com/dashboard/soloist".as_ref()));
    app.on_set_motion(|on| with_ui(|ui| ui.change_settings(|s| s.motion = on)));
    app.on_set_tray(|on| with_ui(|ui| ui.change_settings(|s| s.tray = on)));
    app.on_set_tint(|tint| with_ui(|ui| ui.change_settings(|s| s.tint = tint.clamp(settings::MIN_TINT, 0.85))));
    app.set_presets(ModelRc::new(VecModel::from(eq::PRESETS.map(|p| p.0.into()).to_vec())));
    app.set_eq(ModelRc::new(VecModel::from(settings.eq.to_vec())));
    let theme = app.global::<Theme>();
    theme.set_glow(art::glow());
    app.global::<Scene>().on_placed(|panel, x, y, width, height| {
        let _ = slint::invoke_from_event_loop(move || with_ui(|ui| ui.place(panel, art::Panel { x, y, width, height })));
    });
    app.on_repaint(|| {
        let _ = slint::invoke_from_event_loop(|| with_ui(|ui| ui.repaint_soon()));
    });
    follow_pointer(&app);
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
    let style = Style::by_id(&settings.theme);
    let look = art::look(None, style);
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
        pictures: HashMap::new(),
        playlists,
        tiles,
        shelves: vec![],
        lyrics: vec![],
        lyrics_uri: String::new(),
        shared: shared.clone(),
        settings_path,
        data_dir: data_dir.clone(),
        save: slint::Timer::default(),
        palette: look.palette,
        fade: slint::Timer::default(),
        scene: art::Scene { backdrop: look.backdrop.clone(), palette: look.palette, style, tint: 0., size: (0, 0), scale: 1., panels: vec![] },
        panels: [art::Panel { x: 0., y: 0., width: 0., height: 0. }; PANELS],
        painting: 0,
        repaint: slint::Timer::default(),
        new_look: false,
        tidy: slint::Timer::default(),
    };
    ui.show_settings(&settings);
    ui.show_style();
    ui.show_palette(look.palette);
    ui.set_look(look);
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
    fn frames_are_made_for_the_next_refresh() {
        let next = |now| super::next_refresh(now, 100., 0.016);
        assert!((next(100.004) - 100.016).abs() < 1e-9);
        assert!((next(100.0155) - 100.032).abs() < 1e-9, "too late for the one that is about to happen");
        assert!((next(99.99) - 100.).abs() < 1e-9, "the known refresh may lie ahead");
        let refresh = super::Refresh::get();
        assert!(refresh.next > refresh.now && refresh.next - refresh.now < 0.06, "{} {}", refresh.now, refresh.next);
    }

    #[test]
    fn scrolling_is_smooth_exact_and_the_same_at_any_frame_rate() {
        for frame in [4, 7, 17, 33] {
            let mut spring = super::Spring::default();
            let (mut moved, mut time, mut last) = (0., 0, 0f32);
            spring.push(-96.);
            spring.push(-96.);
            while !spring.at_rest() {
                let step = spring.step(super::Duration::from_millis(frame));
                assert!(step <= 0.01, "never turns around");
                // No jumps: speed changes by a little at a time, from standing still.
                assert!((step - last).abs() / frame as f32 <= 1.1, "{last} to {step} at {frame}");
                (moved, time, last) = (moved + step, time + frame, step);
            }
            assert!((moved + 192f32).abs() < 0.01 && (250..900).contains(&time), "{moved} in {time} ms at {frame}");
        }
        let mut spring = super::Spring { left: 100., speed: 400. };
        spring.push(-30.);
        assert_eq!((spring.left, spring.speed), (-30., 0.), "reversing stops at once");
    }

    #[test]
    fn greetings_follow_the_day() {
        assert_eq!([4, 5, 11, 12, 17, 18, 0].map(super::greeting).map(|g| &g[5..6]), ["e", "m", "m", "a", "a", "e", "e"]);
        assert!(super::local_hour() < 24);
    }

    #[test]
    fn clock_formats() {
        assert_eq!(super::clock(0), "0:00");
        assert_eq!(super::clock(61_999), "1:01");
        assert_eq!(super::clock(3_661_000), "1:01:01");
    }
}
