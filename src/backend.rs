use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use librespot_connect::{
    ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack, Spirc,
};
use librespot_core::{
    Error, FileId, Session, SessionConfig, SpotifyId, SpotifyUri, authentication::Credentials,
    cache::Cache, config::DeviceType, error::ErrorKind,
};
use librespot_metadata::{
    audio::{AudioItem, UniqueFields},
    lyrics::{Lyrics, SyncType},
};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, Bitrate, PlayerConfig},
    mixer::{self, MixerConfig},
    player::{Player, PlayerEvent, QueueTrack},
};
use librespot_protocol::{
    context_page::ContextPage,
    extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery},
    extension_kind::ExtensionKind,
    metadata,
    playlist4_external::SelectedListContent,
};
use protobuf::{EnumOrUnknown, Message};
use serde_json::Value;
use tokio::{
    sync::{Notify, Semaphore, mpsc},
    task::JoinHandle,
};

use crate::{
    eq::EqSink,
    library,
    local,
    settings::Shared,
    people::{self, Profile},
    soloist,
};

const CHUNK: usize = 100;
/// The home page is kept this long.
const HOME_AGE: Duration = Duration::from_secs(300);
/// Enough to fill a few screens; the feed goes on for much longer.
const SHELVES: usize = 14;
const CARDS: usize = 10;
/// Covers fetched at the same time.
const COVER_FETCHES: usize = 6;
/// Songs refused in a row before giving up.
const REFUSALS: u32 = 3;

#[derive(Debug)]
pub enum Cmd {
    /// Sign in, or retry the connection right away when offline.
    Connect,
    Logout,
    Play { context: String, track: String },
    Browse { title: String, uri: String },
    Liked,
    Search(String),
    Queue,
    More,
    Enqueue(String),
    ClearQueue,
    /// Load the home page, unless that was done a moment ago and `again` isn't set.
    Home { again: bool },
    /// Show a profile by its URI, or friends if that is empty.
    People(String),
    Follow { uri: String, on: bool },
    /// Adds a song to the liked songs, or takes it out.
    Like { uri: String, on: bool },
    /// Makes a playlist, with `track` in it unless that is empty.
    NewPlaylist { name: String, track: String },
    AddToPlaylist { playlist: String, name: String, track: String },
    /// Read a playlist's name and settings, for the edit dialog.
    PlaylistDetails(String),
    SetPlaylistDetails { playlist: String, old: library::Details, new: library::Details },
    DeletePlaylist { playlist: String, name: String },
    /// A song by its URI and how many times it is in the list before (see `library::Song`).
    RemoveSong { playlist: String, uri: String, nth: usize },
    /// Moves a song to the place of another.
    MoveSong { playlist: String, uri: String, nth: usize, other: String, other_nth: usize },
    /// Fetch a picture; `kind` only comes back with it.
    Picture { kind: i32, id: String },
    /// Fetch the lyrics of a track by its URI.
    Lyrics(String),
    Toggle,
    Resume,
    Pause,
    Next,
    Prev,
    Seek(u32),
    Volume(u16),
    Shuffle(bool),
    /// 0 off, 1 whole context, 2 single track
    Repeat(u8),
    /// Connect again with the other player.
    Restart,
    Quit,
}

pub enum Event {
    NeedLogin(String),
    Connecting(String),
    /// Waiting before the next attempt.
    Offline(String),
    Ready,
    Status(String),
    /// Spotify refuses every song; only Soloist can play.
    Blocked,
    Jam(Option<Jam>),
    Playlists(Vec<Card>),
    /// The playlists songs can be added to: one's own and collaborative ones.
    Editable(Vec<String>),
    /// The playlists of one's own, which can be renamed and deleted.
    Owned(Vec<String>),
    PlaylistDetails { uri: String, details: library::Details },
    /// Every liked song, once loaded.
    Liked(Vec<String>),
    LikeChanged { uri: String, on: bool },
    Home(Vec<Shelf>),
    /// The home page could not be loaded; why.
    NoHome(String),
    People { uri: String, profile: Result<Profile, String> },
    Followed { uri: String, on: bool },
    /// A new list is being shown. `context` is empty for the queue.
    List { title: String, context: String },
    Rows(Vec<Row>),
    ListDone,
    Track { uri: String, title: String, artist: String, artist_uri: String, album: String, duration_ms: u32, cover_url: Option<String> },
    Cover { uri: String, bytes: Vec<u8> },
    /// Several images make a mosaic; none means it could not be fetched.
    Picture { kind: i32, id: String, images: Vec<Vec<u8>> },
    /// Lines with their start time; `note` says why there are none.
    Lyrics { uri: String, synced: bool, lines: Vec<(u32, String)>, note: &'static str },
    Position { ms: u32, playing: bool },
    Stopped,
    Volume(u16),
    Shuffle(bool),
    Repeat(u8),
    /// How the chosen player is doing, for the settings.
    Player(String),
    /// Whether Soloist is signed in, once that is clear.
    Paired(bool),
    Quit,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub artist_uri: String,
    pub album: String,
    pub album_uri: String,
    pub duration_ms: u32,
    /// Image id of the small cover, empty when there is none.
    pub cover: String,
}

/// Something that opens a list: a playlist, album or artist.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Card {
    pub title: String,
    pub subtitle: String,
    pub uri: String,
    /// What `Cmd::Picture` takes, empty when there is none.
    pub picture: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shelf {
    pub title: String,
    pub cards: Vec<Card>,
}

/// People listening together.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Jam {
    pub names: Vec<String>,
    pub link: String,
}

pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

/// What made a connected session end.
pub enum End {
    Quit,
    Logout,
    Restart,
    /// Soloist can't be used; the reason is for the user.
    NoSoloist(String),
    Lost,
}

pub async fn run(mut rx: mpsc::UnboundedReceiver<Cmd>, emit: Emit, data_dir: PathBuf, shared: Arc<Shared>) {
    // shortcut: the reusable login is a plain file in the user's profile,
    // move it to Windows Credential Manager if other people use this account.
    let cache = match Cache::new(Some(&data_dir), Some(&data_dir), None, None) {
        Ok(cache) => cache,
        Err(e) => return emit(Event::NeedLogin(format!("Can't write to {}: {e}", data_dir.display()))),
    };
    let mut credentials = cache.credentials();
    let mut retry = Duration::from_secs(5);
    let mut notice = String::new();
    let mut no_soloist: Option<String> = None;

    'again: loop {
        if shared.lock().offline {
            let end = local::run(&mut rx, &emit, &cache, &shared).await;
            emit(Event::Stopped);
            match end {
                End::Quit => return emit(Event::Quit),
                End::Logout => {
                    notice = forget(&data_dir).err().unwrap_or_default();
                    credentials = None;
                }
                _ => {}
            }
            continue;
        }
        let Some(login) = credentials.clone() else {
            emit(Event::NeedLogin(std::mem::take(&mut notice)));
            loop {
                match rx.recv().await {
                    Some(Cmd::Connect) => break,
                    Some(Cmd::Restart) => continue 'again,
                    Some(Cmd::Quit) | None => return emit(Event::Quit),
                    _ => {}
                }
            }
            // Don't ask for a sign-in that the network won't let us use.
            emit(Event::Connecting("Checking the connection…".into()));
            let session = Session::new(SessionConfig::default(), None);
            let reachable = tokio::select! {
                r = session.spclient().client_token() => r,
                () = quit_requested(&mut rx) => return emit(Event::Quit),
            };
            if let Err(e) = reachable {
                notice = format!("{}.", explain(&e));
                continue;
            }
            emit(Event::Connecting("Finish signing in in your browser…".into()));
            // shortcut: closing the browser tab leaves this waiting until the app restarts,
            // because librespot's login listener can't be cancelled.
            let mut sign_in = tokio::task::spawn_blocking(sign_in);
            let result = loop {
                tokio::select! {
                    r = &mut sign_in => break r.unwrap_or_else(|e| Err(e.to_string())),
                    cmd = rx.recv() => if let Some(Cmd::Quit) | None = cmd { return emit(Event::Quit) },
                }
            };
            match result {
                Ok(c) => credentials = Some(c),
                Err(e) => notice = format!("Sign-in failed: {e}"),
            }
            continue;
        };

        emit(Event::Connecting("Connecting…".into()));
        let error = match connected(&mut rx, &emit, &cache, login, &mut retry, &shared, &data_dir, &no_soloist).await {
            Ok(End::Quit) => return emit(Event::Quit),
            Ok(End::Logout) => {
                notice = forget(&data_dir).err().unwrap_or_default();
                credentials = None;
                continue;
            }
            Ok(End::NoSoloist(reason)) => {
                // Go on without it, so that Settings stays reachable.
                no_soloist = Some(reason);
                credentials = cache.credentials().or(credentials);
                continue;
            }
            Ok(End::Restart) => {
                no_soloist = None;
                credentials = cache.credentials().or(credentials);
                continue;
            }
            Ok(End::Lost) => "Connection lost".to_string(),
            Err(e) if e.kind == ErrorKind::PermissionDenied => {
                notice = forget(&data_dir).err().unwrap_or_else(|| e.error.to_string());
                credentials = None;
                continue;
            }
            Err(e) => explain(&e),
        };
        // A first login is only reusable once librespot has stored it.
        credentials = cache.credentials().or(credentials);

        emit(Event::Offline(format!("{error}. Retrying in {} s.", retry.as_secs())));
        let wake = tokio::time::sleep(retry);
        tokio::pin!(wake);
        loop {
            tokio::select! {
                () = &mut wake => break,
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Connect | Cmd::Restart) => break,
                    Some(Cmd::Quit) | None => return emit(Event::Quit),
                    _ => {}
                },
            }
        }
        retry = (retry * 2).min(Duration::from_secs(60));
    }
}

async fn quit_requested(rx: &mut mpsc::UnboundedReceiver<Cmd>) {
    while !matches!(rx.recv().await, Some(Cmd::Quit) | None) {}
}

/// Windows reports an untrusted, mismatched or expired certificate with these codes;
/// the text around them is translated, so it can't be matched.
const CERTIFICATE_ERRORS: [i32; 5] = [
    0x800B0109_u32 as i32, // CERT_E_UNTRUSTEDROOT
    0x80090325_u32 as i32, // SEC_E_UNTRUSTED_ROOT
    0x800B010F_u32 as i32, // CERT_E_CN_NO_MATCH
    0x800B0101_u32 as i32, // CERT_E_EXPIRED
    0x800B010A_u32 as i32, // CERT_E_CHAINING
];

fn explain(e: &Error) -> String {
    log::warn!("{e:?}");
    explain_chain(&*e.error)
}

fn explain_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(e) = source {
        text = e.to_string();
        source = e.source();
    }
    if CERTIFICATE_ERRORS.iter().any(|code| text.contains(&format!("os error {code}"))) {
        return concat!(
            "This network is intercepting Spotify's secure connection, so Rustbeat stopped before ",
            "sending your login through it. Switch to another network, such as a phone hotspot",
        )
        .into();
    }
    text
}

fn sign_in() -> Result<Credentials, String> {
    let token = librespot_oauth::OAuthClientBuilder::new(
        &SessionConfig::default().client_id,
        "http://127.0.0.1:8898/login",
        vec!["streaming"],
    )
    .open_in_browser()
    .with_custom_message("Signed in. You can close this tab and go back to Rustbeat.")
    .build()
    .and_then(|client| client.get_access_token())
    .map_err(|e| e.to_string())?;
    Ok(Credentials::with_access_token(token.access_token))
}

fn forget(data_dir: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(data_dir.join("credentials.json")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("Signed out, but the saved login could not be deleted: {e}"))
        }
        _ => Ok(()),
    }
}

async fn connected(
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    emit: &Emit,
    cache: &Cache,
    credentials: Credentials,
    retry: &mut Duration,
    shared: &Arc<Shared>,
    data_dir: &std::path::Path,
    no_soloist: &Option<String>,
) -> Result<End, Error> {
    let session = Session::new(SessionConfig::default(), Some(cache.clone()));
    // Fails early on a blocked network, before the audio device is opened.
    tokio::select! {
        r = session.spclient().client_token() => { r?; }
        () = quit_requested(rx) => return Ok(End::Quit),
    }
    let mut library = Library::new(session.clone(), emit.clone());
    if shared.lock().soloist {
        let setup = match no_soloist {
            Some(reason) => Err(reason.clone()),
            None => soloist::Setup::load(data_dir).map_err(String::from),
        };
        match setup {
            Ok(setup) => {
                tokio::select! {
                    r = session.connect(credentials, true) => r?,
                    () = quit_requested(rx) => return Ok(End::Quit),
                }
                *retry = Duration::from_secs(5);
                let tuning = soloist::Tuning::of(&shared.lock());
                let end = soloist::run(&mut library, rx, emit, cache, &setup, data_dir, tuning).await;
                emit(Event::Stopped);
                session.shutdown();
                return match end {
                    Err(e) => Ok(End::NoSoloist(e.error.to_string())),
                    end => end,
                };
            }
            // Browsing still works, and Settings says what is missing.
            Err(missing) => emit(Event::Player(format!("{missing}. The built-in player is used for now; press Save or choose Soloist again to retry."))),
        }
    } else {
        emit(Event::Player(String::new()));
    }
    let mixer = mixer::find(None).expect("softvol is always built in")(MixerConfig::default())?;
    let sink = audio_backend::find(None).expect("rodio backend is enabled in Cargo.toml");
    let player_config = {
        let settings = shared.lock();
        PlayerConfig {
            bitrate: match settings.bitrate {
                96 => Bitrate::Bitrate96,
                160 => Bitrate::Bitrate160,
                _ => Bitrate::Bitrate320,
            },
            normalisation: settings.normalise,
            ..Default::default()
        }
    };
    let eq_settings = shared.clone();
    let player = Player::new(
        player_config,
        session.clone(),
        mixer.get_soft_volume(),
        move || {
            let out: Box<dyn audio_backend::Sink> = match crate::resample::Out::open() {
                Ok(out) => Box::new(out),
                Err(e) => {
                    log::warn!("Falling back to librespot's own output: {e}");
                    sink(None, AudioFormat::F32)
                }
            };
            Box::new(EqSink::new(out, eq_settings))
        },
    );
    let mut events = player.get_player_event_channel();
    let mut volume = cache.volume().unwrap_or(u16::MAX / 2);
    let config = ConnectConfig {
        name: "Rustbeat".into(),
        device_type: DeviceType::Computer,
        initial_volume: volume,
        emit_set_queue_events: true,
        ..Default::default()
    };

    let connecting = Spirc::new(config, session.clone(), credentials, player, mixer);
    tokio::pin!(connecting);
    let (spirc, spirc_task) = loop {
        tokio::select! {
            r = &mut connecting => break r?,
            cmd = rx.recv() => match cmd {
                Some(Cmd::Quit) | None => return Ok(End::Quit),
                Some(Cmd::Logout) => return Ok(End::Logout),
                Some(Cmd::Restart) => return Ok(End::Restart),
                _ => {}
            },
        }
    };
    *retry = Duration::from_secs(5);
    let mut spirc_task = tokio::spawn(spirc_task);
    emit(Event::Ready);
    emit(Event::Volume(volume));

    library.load_start();

    let mut queue: Vec<QueueTrack> = Vec::new();
    let mut has_queued = false;
    let mut options = Options::default();
    let mut refused = 0;
    let mut blocked = false;
    // Whether Spotify unlocks songs for this player, asked once, when something is first played.
    let (refusal_tx, mut refusal) = mpsc::unbounded_channel();
    let mut asked = false;

    let end = loop {
        tokio::select! {
            _ = &mut spirc_task => break End::Lost,
            cmd = rx.recv() => {
                let cmd = match cmd {
                    Some(Cmd::Quit) | None => break End::Quit,
                    Some(Cmd::Logout) => break End::Logout,
                    Some(Cmd::Restart) => break End::Restart,
                    Some(cmd) => cmd,
                };
                let Some(cmd) = library.handle(cmd) else { continue };
                let sent = match cmd {
                    Cmd::Queue => { library.show_queue(uris_of(&queue)); Ok(()) }
                    Cmd::Play { .. } if blocked => {
                        emit(Event::Blocked);
                        Ok(())
                    }
                    Cmd::Play { context, track } => {
                        if !std::mem::replace(&mut asked, true) {
                            let (session, tx) = (session.clone(), refusal_tx.clone());
                            let (context, track) = (context.clone(), track.clone());
                            tokio::spawn(async move {
                                match key_refused(&session, &context, &track).await {
                                    Ok(true) => drop(tx.send(())),
                                    Ok(false) => {}
                                    Err(e) => log::warn!("key check: {e}"),
                                }
                            });
                        }
                        let request = LoadRequestOptions {
                            start_playing: true,
                            context_options: Some(LoadContextOptions::Options(options.clone())),
                            // Without a song, the list plays from its start.
                            playing_track: Some(track.clone()).filter(|t| !t.is_empty()).map(PlayingTrack::Uri),
                            ..Default::default()
                        };
                        refused = 0;
                        // The queue view has no context of its own: play from that entry onwards.
                        let load = if context.is_empty() {
                            let from = queue.iter().position(|t| t.uri == track).unwrap_or(0);
                            LoadRequest::from_tracks(queue[from..].iter().map(|t| t.uri.clone()).collect(), request)
                        } else {
                            LoadRequest::from_context_uri(context, request)
                        };
                        // Volume changes made while another device was active were not applied yet.
                        spirc.activate().and_then(|()| spirc.set_volume(volume)).and_then(|()| spirc.load(load))
                    }
                    Cmd::Enqueue(uri) => {
                        has_queued = true;
                        SpotifyUri::from_uri(&uri).and_then(|uri| spirc.add_to_queue(uri))
                    }
                    Cmd::ClearQueue => spirc.clear_queue(),
                    Cmd::Toggle => spirc.play_pause(),
                    Cmd::Resume => spirc.play(),
                    Cmd::Pause => spirc.pause(),
                    Cmd::Next => spirc.next(),
                    Cmd::Prev => spirc.prev(),
                    Cmd::Seek(ms) => spirc.set_position_ms(ms),
                    Cmd::Volume(v) => {
                        volume = v;
                        emit(Event::Volume(v));
                        spirc.set_volume(v)
                    }
                    Cmd::Shuffle(on) => {
                        options.shuffle = on;
                        emit(Event::Shuffle(on));
                        spirc.shuffle(on)
                    }
                    Cmd::Repeat(mode) => {
                        (options.repeat, options.repeat_track) = (mode == 1, mode == 2);
                        emit(Event::Repeat(mode));
                        spirc.repeat(mode == 1).and_then(|()| spirc.repeat_track(mode == 2))
                    }
                    _ => Ok(()),
                };
                if let Err(e) = sent {
                    emit(Event::Status(e.error.to_string()));
                }
            }
            Some(()) = refusal.recv() => {
                blocked = true;
                if let Err(e) = spirc.disconnect(true) {
                    log::warn!("stop: {e}");
                }
                emit(Event::Stopped);
                emit(Event::Blocked);
            }
            event = events.recv() => match event {
                None => break End::Lost,
                Some(PlayerEvent::TrackChanged { audio_item }) => {
                    advance(&mut queue, &audio_item.uri);
                    if library.queue_shown { library.show_queue(uris_of(&queue)) }
                    library.track_changed(*audio_item);
                }
                Some(PlayerEvent::Playing { position_ms, .. }) => {
                    (refused, blocked) = (0, false);
                    emit(Event::Position { ms: position_ms, playing: true });
                }
                Some(PlayerEvent::Paused { position_ms, .. } | PlayerEvent::Loading { position_ms, .. }) =>
                    emit(Event::Position { ms: position_ms, playing: false }),
                Some(PlayerEvent::Seeked { position_ms, .. } | PlayerEvent::PositionCorrection { position_ms, .. }) =>
                    emit(Event::Position { ms: position_ms, playing: true }),
                Some(PlayerEvent::Stopped { .. }) => emit(Event::Stopped),
                Some(PlayerEvent::Unavailable { .. }) => {
                    refused += 1;
                    if blocked {
                        // Already said so.
                    } else if refused < REFUSALS {
                        emit(Event::Status("Spotify refused this song, trying the next one…".into()));
                    } else {
                        // Spotify refuses some accounts every song; don't run through the whole list.
                        blocked = true;
                        if let Err(e) = spirc.disconnect(true) {
                            log::warn!("stop: {e}");
                        }
                        emit(Event::Stopped);
                        emit(Event::Blocked);
                    }
                }
                Some(PlayerEvent::VolumeChanged { volume: v }) => {
                    volume = v;
                    emit(Event::Volume(v));
                }
                Some(PlayerEvent::ShuffleChanged { shuffle }) => {
                    options.shuffle = shuffle;
                    emit(Event::Shuffle(shuffle));
                    // librespot doesn't report the reshuffled order. Clearing an already empty
                    // queue makes it; with queued tracks, only those are still known.
                    if has_queued {
                        queue.retain(|t| t.provider == "queue");
                        if library.queue_shown { library.show_queue(uris_of(&queue)) }
                    } else if !queue.is_empty() && let Err(e) = spirc.clear_queue() {
                        log::warn!("queue refresh: {e}");
                    }
                }
                Some(PlayerEvent::RepeatChanged { context, track }) => {
                    (options.repeat, options.repeat_track) = (context, track);
                    emit(Event::Repeat(if track { 2 } else { context as u8 }));
                }
                Some(PlayerEvent::SetQueue { next_tracks, .. }) => {
                    queue = next_tracks;
                    queue.retain(|t| t.uri.starts_with("spotify:track:"));
                    has_queued = queue.iter().any(|t| t.provider == "queue");
                    if library.queue_shown { library.show_queue(uris_of(&queue)) }
                }
                Some(_) => {}
            },
        }
    };

    emit(Event::Stopped);
    if !spirc_task.is_finished() && spirc.shutdown().is_ok() {
        let _ = tokio::time::timeout(Duration::from_secs(2), spirc_task).await;
    }
    session.shutdown();
    Ok(end)
}

fn spawn_reporting(
    emit: &Emit,
    task: impl Future<Output = Result<(), Error>> + Send + 'static,
) -> JoinHandle<()> {
    let emit = emit.clone();
    tokio::spawn(async move {
        if let Err(e) = task.await {
            emit(Event::ListDone);
            emit(Event::Status(if e.kind == ErrorKind::NotFound {
                "Nothing found.".into()
            } else {
                e.error.to_string()
            }));
        }
    })
}

fn uris_of(queue: &[QueueTrack]) -> Vec<String> {
    queue.iter().map(|t| t.uri.clone()).collect()
}

/// Everything that only reads from Spotify, the same for both players.
pub struct Library {
    pub session: Session,
    pub queue_shown: bool,
    emit: Emit,
    browse: Option<JoinHandle<()>>,
    more: Arc<Notify>,
    cover_slots: Arc<Semaphore>,
    home_loaded: Option<Instant>,
}

impl Drop for Library {
    fn drop(&mut self) {
        if let Some(task) = self.browse.take() {
            task.abort();
        }
    }
}

impl Library {
    fn new(session: Session, emit: Emit) -> Self {
        Self { session, queue_shown: false, emit, browse: None, more: Arc::default(), cover_slots: Arc::new(Semaphore::new(COVER_FETCHES)), home_loaded: None }
    }

    /// Does the command, or gives it back if it is for the player.
    pub fn handle(&mut self, cmd: Cmd) -> Option<Cmd> {
        match cmd {
            Cmd::Connect => {}
            Cmd::Browse { title, uri } => {
                (self.emit)(Event::List { title, context: uri.clone() });
                self.open(None, uri);
            }
            Cmd::Liked => {
                let uri = format!("spotify:user:{}:collection", self.session.username());
                (self.emit)(Event::List { title: "Liked Songs".into(), context: uri.clone() });
                self.open(None, uri);
            }
            // The old list stays until there is something to show instead.
            Cmd::Search(q) => self.open(Some(format!("Results for “{}”", q.trim())), search_uri(&q)),
            Cmd::Queue => {
                self.queue_shown = true;
                return Some(Cmd::Queue);
            }
            Cmd::More => self.more.notify_one(),
            Cmd::Home { again } => self.load_home(again),
            Cmd::People(uri) => {
                // It takes the place of the home page.
                self.home_loaded = None;
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    let mut shared = vec![];
                    if uri.is_empty() {
                        shared = playlists(&session).await.inspect_err(|e| log::warn!("playlists: {e}")).unwrap_or_default().0;
                        shared.retain(|c| c.subtitle == BLEND || c.subtitle == GROUP);
                    }
                    let profile = people::load(&session, &uri, shared).await.map_err(|e| format!("This page could not be loaded: {}", e.error));
                    emit(Event::People { uri, profile });
                });
            }
            Cmd::Like { uri, on } => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    emit(match library::like(&session, &uri, on).await {
                        Ok(()) => Event::LikeChanged { uri, on },
                        Err(e) => Event::Status(format!("That didn't work: {}", e.error)),
                    });
                });
            }
            Cmd::NewPlaylist { name, track } => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    let made = async {
                        let uri = library::create_playlist(&session, &name).await?;
                        if !track.is_empty() {
                            library::add_to_playlist(&session, &uri, &track).await?;
                        }
                        Ok::<_, Error>(())
                    };
                    emit(Event::Status(match made.await {
                        Ok(()) if track.is_empty() => format!("Made “{}”.", name.trim()),
                        Ok(()) => format!("Made “{}” with the song in it.", name.trim()),
                        Err(e) => format!("The playlist could not be made: {}", e.error),
                    }));
                    let _ = load_playlists(session, emit).await;
                });
            }
            Cmd::AddToPlaylist { playlist, name, track } => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    emit(Event::Status(match library::add_to_playlist(&session, &playlist, &track).await {
                        Ok(()) => format!("Added to “{name}”."),
                        Err(e) => format!("That didn't work: {}", e.error),
                    }));
                });
            }
            Cmd::PlaylistDetails(uri) => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    emit(match library::details(&session, &uri).await {
                        Ok(details) => Event::PlaylistDetails { uri, details },
                        Err(e) => Event::Status(format!("The playlist could not be read: {}", e.error)),
                    });
                });
            }
            Cmd::SetPlaylistDetails { playlist, old, new } => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    emit(Event::Status(match library::set_details(&session, &playlist, &old, &new).await {
                        Ok(()) => format!("Saved “{}”.", new.name.trim()),
                        Err(e) => format!("That didn't work: {}", e.error),
                    }));
                    let _ = load_playlists(session, emit).await;
                });
            }
            Cmd::DeletePlaylist { playlist, name } => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    emit(Event::Status(match library::delete_playlist(&session, &playlist).await {
                        Ok(()) => format!("Deleted “{name}”."),
                        Err(e) => format!("That didn't work: {}", e.error),
                    }));
                    let _ = load_playlists(session, emit).await;
                });
            }
            Cmd::RemoveSong { playlist, uri, nth } => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    // The row is already gone; only a failure needs saying.
                    if let Err(e) = library::remove_song(&session, &playlist, &library::Song { uri: &uri, nth }).await {
                        emit(Event::Status(format!("The song could not be removed: {}. Open the playlist again to see it as it is.", e.error)));
                    }
                });
            }
            Cmd::MoveSong { playlist, uri, nth, other, other_nth } => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    let (song, other) = (library::Song { uri: &uri, nth }, library::Song { uri: &other, nth: other_nth });
                    if let Err(e) = library::move_song(&session, &playlist, &song, &other).await {
                        emit(Event::Status(format!("The song could not be moved: {}. Open the playlist again to see it as it is.", e.error)));
                    }
                });
            }
            Cmd::Follow { uri, on } => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    emit(match people::follow(&session, &uri, on).await {
                        Ok(()) => Event::Followed { uri, on },
                        Err(e) => Event::Status(format!("That didn't work: {}", e.error)),
                    });
                });
            }
            Cmd::Picture { kind, id } => {
                let (session, emit, slots) = (self.session.clone(), self.emit.clone(), self.cover_slots.clone());
                tokio::spawn(async move {
                    let _slot = slots.acquire().await;
                    let mut images = Vec::new();
                    for url in picture_urls(&id) {
                        match session.spclient().request_url(&url).await {
                            Ok(bytes) => images.push(bytes.to_vec()),
                            Err(e) => log::warn!("picture: {e}"),
                        }
                    }
                    emit(Event::Picture { kind, id, images });
                });
            }
            Cmd::Lyrics(uri) => {
                let (session, emit) = (self.session.clone(), self.emit.clone());
                tokio::spawn(async move {
                    let (synced, lines, note) = match load_lyrics(&session, &uri).await {
                        Ok((synced, lines)) => (synced, lines, "No lyrics for this song"),
                        Err(e) if e.kind == ErrorKind::NotFound => (false, vec![], "No lyrics for this song"),
                        Err(e) => {
                            log::warn!("lyrics: {e}");
                            (false, vec![], "Lyrics could not be loaded")
                        }
                    };
                    emit(Event::Lyrics { uri, synced, lines, note });
                });
            }
            cmd => return Some(cmd),
        }
        None
    }

    fn start(&mut self, task: impl Future<Output = Result<(), Error>> + Send + 'static) {
        if let Some(task) = self.browse.take() {
            task.abort();
        }
        self.browse = Some(spawn_reporting(&self.emit, task));
    }

    fn open(&mut self, title: Option<String>, uri: String) {
        self.queue_shown = false;
        self.more = Arc::default();
        self.start(load_context(self.session.clone(), uri, title, self.emit.clone(), self.more.clone()));
    }

    /// What the window needs first.
    pub fn load_start(&mut self) {
        self.load_home(true);
        spawn_reporting(&self.emit, load_playlists(self.session.clone(), self.emit.clone()));
        let (session, emit) = (self.session.clone(), self.emit.clone());
        tokio::spawn(async move {
            match library::liked(&session).await {
                Ok(liked) => emit(Event::Liked(liked.into_iter().collect())),
                Err(e) => log::warn!("liked songs: {e}"),
            }
        });
    }

    fn load_home(&mut self, again: bool) {
        if !again && self.home_loaded.is_some_and(|at| at.elapsed() < HOME_AGE) {
            return;
        }
        self.home_loaded = Some(Instant::now());
        let (session, emit) = (self.session.clone(), self.emit.clone());
        tokio::spawn(async move {
            let feed = session.spclient().request_as_json(&http::Method::GET, "/homeview/v1/home", None, None).await;
            let shelves = feed.and_then(|json| Ok(shelves_in(&serde_json::from_slice(&json)?)));
            emit(match shelves {
                Ok(shelves) if !shelves.is_empty() => Event::Home(shelves),
                Ok(_) => Event::NoHome("Spotify sent an empty home page.".into()),
                Err(e) => Event::NoHome(format!("The home page could not be loaded: {}", e.error)),
            });
        });
    }

    pub fn show_queue(&mut self, uris: Vec<String>) {
        (self.emit)(Event::List { title: "Up next".into(), context: String::new() });
        let (session, emit) = (self.session.clone(), self.emit.clone());
        self.start(async move {
            for chunk in uris.chunks(CHUNK) {
                emit(Event::Rows(load_rows(&session, chunk).await?));
            }
            emit(Event::ListDone);
            Ok(())
        });
    }

    /// The Jam the account is in. This only looks; starting one is up to the Spotify app.
    pub async fn jam(session: &Session) -> Result<Option<Jam>, Error> {
        match session.spclient().request_as_json(&http::Method::GET, "/social-connect/v2/sessions/current", None, None).await {
            Ok(json) => Ok(jam_in(&serde_json::from_slice(&json)?)),
            Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn all_tracks(&self, context: &str) -> Result<Vec<String>, Error> {
        let mut pages = Pages::open(&self.session, context).await?;
        pages.fill(&self.session, usize::MAX).await?;
        Ok(pages.uris.into())
    }

    pub fn load_cover(&self, uri: String, url: String) {
        let (session, emit) = (self.session.clone(), self.emit.clone());
        tokio::spawn(async move {
            match session.spclient().request_url(&url).await {
                Ok(bytes) => emit(Event::Cover { uri, bytes: bytes.to_vec() }),
                Err(e) => log::warn!("cover: {e}"),
            }
        });
    }

    fn track_changed(&self, item: AudioItem) {
        let (artist, artist_uri, album) = match item.unique_fields {
            UniqueFields::Track { artists, album, .. } => (
                artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "),
                artists.first().map(|a| a.id.to_uri()).unwrap_or_default(),
                album,
            ),
            UniqueFields::Episode { show_name, .. } => (show_name, String::new(), String::new()),
            UniqueFields::Local { artists, album, .. } => (artists.unwrap_or_default(), String::new(), album.unwrap_or_default()),
        };
        let cover_url = item.covers.into_iter().max_by_key(|c| c.width).map(|c| c.url);
        (self.emit)(Event::Track { uri: item.uri.clone(), title: item.name, artist, artist_uri, album, duration_ms: item.duration_ms, cover_url: cover_url.clone() });
        if let Some(url) = cover_url {
            self.load_cover(item.uri, url);
        }
    }
}

/// Drops everything up to and including the track that just started.
fn advance(queue: &mut Vec<QueueTrack>, now: &str) {
    if let Some(i) = queue.iter().position(|t| t.uri == now) {
        queue.drain(..=i);
    }
}

/// Asks for the key of one song, as the player is about to. It takes the player much longer to
/// find out, because it first tries to play the locked file.
async fn key_refused(session: &Session, context: &str, track: &str) -> Result<bool, Error> {
    let uri = if track.is_empty() {
        let mut pages = Pages::open(session, context).await?;
        pages.fill(session, 1).await?;
        pages.uris.pop_front().unwrap_or_default()
    } else {
        track.to_owned()
    };
    let uri = SpotifyUri::from_uri(&uri)?;
    let SpotifyUri::Track { id } = uri else { return Ok(false) };
    let item = AudioItem::get_file(session, uri).await?;
    let Some(file) = item.files.values().next() else { return Ok(false) };
    // A refusal is "unavailable"; no answer in time is something else.
    Ok(matches!(session.audio_key().request(id, *file).await, Err(e) if e.kind == ErrorKind::Unavailable))
}

async fn load_lyrics(session: &Session, uri: &str) -> Result<(bool, Vec<(u32, String)>), Error> {
    let SpotifyUri::Track { id } = SpotifyUri::from_uri(uri)? else {
        return Ok((false, vec![]));
    };
    let lyrics = Lyrics::get(session, &id).await?.lyrics;
    let synced = lyrics.sync_type == SyncType::LineSynced;
    Ok((synced, lyrics.lines.into_iter().map(|l| (l.start_time_ms.parse().unwrap_or(0), l.words)).collect()))
}

async fn load_playlists(session: Session, emit: Emit) -> Result<(), Error> {
    let (cards, editable, owned) = playlists(&session).await?;
    emit(Event::Playlists(cards));
    emit(Event::Editable(editable));
    emit(Event::Owned(owned));
    Ok(())
}

/// The library's playlists, which of them songs can be added to, and which are one's own.
async fn playlists(session: &Session) -> Result<(Vec<Card>, Vec<String>, Vec<String>), Error> {
    let (mut playlists, mut editable, mut owned) = (Vec::new(), Vec::new(), Vec::new());
    let user = session.username();
    let mut from = 0;
    loop {
        let list = SelectedListContent::parse_from_bytes(&session.spclient().get_rootlist(from, None).await?)?;
        let count = list.contents.items.len();
        playlists.extend(playlists_in(&list));
        editable.extend(editable_in(&list, &user));
        owned.extend(owned_in(&list, &user));
        from += count;
        if count == 0 || from >= list.length() as usize {
            break;
        }
    }
    Ok((playlists, editable, owned))
}

fn editable_in(list: &SelectedListContent, user: &str) -> Vec<String> {
    let items = list.contents.items.iter().zip(&list.contents.meta_items);
    items.filter(|(item, meta)| item.uri().contains(":playlist:") && (meta.owner_username() == user || meta.attributes.collaborative())).map(|(item, _)| item.uri().to_owned()).collect()
}

fn owned_in(list: &SelectedListContent, user: &str) -> Vec<String> {
    let items = list.contents.items.iter().zip(&list.contents.meta_items);
    items.filter(|(item, meta)| item.uri().contains(":playlist:") && meta.owner_username() == user).map(|(item, _)| item.uri().to_owned()).collect()
}

const BLEND: &str = "Blend";
const GROUP: &str = "Group playlist";

/// The root list also holds folder markers, which have no name to show.
fn playlists_in(list: &SelectedListContent) -> Vec<Card> {
    list.contents
        .items
        .iter()
        .zip(&list.contents.meta_items)
        .filter(|(item, meta)| item.uri().contains(":playlist:") && !meta.attributes.name().is_empty())
        .map(|(item, meta)| {
            let sizes = &meta.attributes.picture_size;
            let sized = sizes.iter().find(|p| p.target_name() == "default").or(sizes.first());
            let own = || match meta.attributes.picture() {
                id if id.len() == 20 => FileId::from_raw(id).to_base16(),
                _ => String::new(),
            };
            Card {
                title: meta.attributes.name().to_owned(),
                subtitle: match &meta.attributes {
                    a if a.format() == "blend" => BLEND,
                    a if a.collaborative() => GROUP,
                    _ if meta.owner_username() == "spotify" => "Playlist · Spotify",
                    _ => "Playlist",
                }
                .into(),
                uri: item.uri().to_owned(),
                picture: sized.map(|p| p.url().to_owned()).unwrap_or_else(own),
            }
        })
        .collect()
}

/// The feed is one flat list in which headings are followed by their cards.
fn shelves_in(feed: &Value) -> Vec<Shelf> {
    let mut shelves: Vec<Shelf> = Vec::new();
    for entry in feed["body"].as_array().into_iter().flatten() {
        let text = |name: &str| entry["text"][name].as_str().unwrap_or_default().trim().to_owned();
        match entry["component"]["category"].as_str().unwrap_or_default() {
            "header" => shelves.push(Shelf { title: text("title"), cards: vec![] }),
            _ => {
                let uri = entry["target"]["uri"].as_str().unwrap_or_default();
                // shortcut: music only, like the rest of the app.
                let opens = uri.ends_with(":collection") || ["playlist", "album", "artist"].iter().any(|k| uri.starts_with(&format!("spotify:{k}:")));
                if let Some(shelf) = shelves.last_mut().filter(|s| opens && s.cards.len() < CARDS) {
                    let picture = entry["images"]["main"]["uri"].as_str().unwrap_or_default().to_owned();
                    shelf.cards.push(Card { title: text("title"), subtitle: text("subtitle"), uri: uri.to_owned(), picture });
                }
            }
        }
    }
    shelves.retain(|s| !s.cards.is_empty());
    shelves.truncate(SHELVES);
    shelves
}

fn jam_in(session: &Value) -> Option<Jam> {
    let text = |v: &Value, names: [&str; 2]| names.iter().find_map(|n| v[*n].as_str()).unwrap_or_default().to_owned();
    let members = ["session_members", "sessionMembers"].iter().find_map(|n| session[*n].as_array())?;
    let names: Vec<_> = members
        .iter()
        .map(|m| match text(m, ["display_name", "displayName"]) {
            name if name.is_empty() => "Someone".to_owned(),
            name => name,
        })
        .collect();
    let link = text(session, ["join_session_url", "joinSessionUrl"]);
    // It is copied for other people to open.
    let link = if link.starts_with("https://") { link } else { String::new() };
    (!names.is_empty()).then_some(Jam { names, link })
}

/// Pictures come as addresses, as image ids, or as four ids to lay out as a mosaic.
fn picture_urls(id: &str) -> Vec<String> {
    let cdn = |id: &str| format!("https://i.scdn.co/image/{id}");
    let hex = |id: &&str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit());
    match id.strip_prefix("spotify:mosaic:").or(id.strip_prefix("spotify:image:")) {
        Some(ids) => ids.split(':').filter(hex).take(4).map(cdn).collect(),
        None if id.starts_with("https://") => vec![id.to_owned()],
        None => Some(id).filter(hex).into_iter().map(cdn).collect(),
    }
}

pub const SEARCH: &str = "spotify:search:";

fn search_uri(query: &str) -> String {
    format!("{SEARCH}{}", form_urlencoded::byte_serialize(query.trim().as_bytes()).collect::<String>())
}

/// `hm://artistplaycontext/v1/page/spotify/album/<id>/km` -> `spotify:album:<id>`
fn page_context(page_url: &str) -> Option<String> {
    let mut parts = page_url.split('/').skip_while(|p| *p != "spotify").skip(1);
    Some(format!("spotify:{}:{}", parts.next()?, parts.next()?))
}

/// The songs of a context, read page by page.
struct Pages {
    pages: VecDeque<ContextPage>,
    uris: VecDeque<String>,
}

impl Pages {
    async fn open(session: &Session, uri: &str) -> Result<Self, Error> {
        Ok(Self { pages: session.spclient().get_context(uri).await?.pages.into(), uris: VecDeque::new() })
    }

    /// Reads on until `count` songs are waiting or there are no more.
    async fn fill(&mut self, session: &Session, count: usize) -> Result<(), Error> {
        while self.uris.len() < count
            && let Some(page) = self.pages.pop_front()
        {
            if page.tracks.is_empty() && let Some(context) = page_context(page.page_url()) {
                let resolved = session.spclient().get_context(&context).await?;
                self.uris.extend(resolved.pages.iter().flat_map(track_uris));
            }
            self.uris.extend(track_uris(&page));
            if !page.next_page_url().is_empty() {
                let json = session.spclient().get_next_page(page.next_page_url()).await?;
                let options = protobuf_json_mapping::ParseOptions { ignore_unknown_fields: true, ..Default::default() };
                let next = protobuf_json_mapping::parse_from_str_with_options(&String::from_utf8_lossy(&json), &options)?;
                self.pages.push_front(next);
            }
        }
        Ok(())
    }
}

/// With a `title`, the list is only replaced once the first songs are here.
async fn load_context(session: Session, uri: String, mut title: Option<String>, emit: Emit, more: Arc<Notify>) -> Result<(), Error> {
    let show = |title: &mut Option<String>| {
        if let Some(title) = title.take() {
            emit(Event::List { title, context: uri.clone() });
        }
    };
    let mut pages = match Pages::open(&session, &uri).await {
        Err(e) if e.kind == ErrorKind::NotFound => {
            show(&mut title);
            return Err(e);
        }
        pages => pages?,
    };
    loop {
        pages.fill(&session, CHUNK).await?;
        if pages.uris.is_empty() {
            show(&mut title);
            emit(Event::ListDone);
            return Ok(());
        }
        let chunk: Vec<_> = pages.uris.drain(..pages.uris.len().min(CHUNK)).collect();
        let rows = load_rows(&session, &chunk).await?;
        show(&mut title);
        emit(Event::Rows(rows));
        more.notified().await;
    }
}

// shortcut: music only, add episodes and local files here if podcasts are wanted.
fn track_uris(page: &ContextPage) -> impl Iterator<Item = String> {
    page.tracks
        .iter()
        .filter_map(|t| t.uri.as_deref())
        .map(|uri| uri.split('?').next().unwrap_or(uri).to_owned())
        .filter(|uri| uri.starts_with("spotify:track:"))
}

async fn load_rows(session: &Session, uris: &[String]) -> Result<Vec<Row>, Error> {
    let request = BatchedEntityRequest {
        entity_request: uris
            .iter()
            .map(|uri| EntityRequest {
                entity_uri: uri.clone(),
                query: vec![ExtensionQuery {
                    extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let response = session.spclient().get_extended_metadata(request).await?;
    let mut rows = HashMap::new();
    for data in response.extended_metadata.iter().flat_map(|a| &a.extension_data) {
        if let Some(any) = data.extension_data.as_ref()
            && !any.value.is_empty()
        {
            let track = metadata::Track::parse_from_bytes(&any.value)?;
            rows.insert(data.entity_uri.clone(), row(&data.entity_uri, &track));
        }
    }
    Ok(uris.iter().filter_map(|uri| rows.get(uri).cloned()).collect())
}

fn row(uri: &str, track: &metadata::Track) -> Row {
    let uri_of = |kind: &str, gid: &[u8]| {
        SpotifyId::from_raw(gid).map(|id| format!("spotify:{kind}:{}", id.to_base62())).unwrap_or_default()
    };
    Row {
        uri: uri.to_owned(),
        title: track.name().to_owned(),
        artist: track.artist.iter().map(|a| a.name()).collect::<Vec<_>>().join(", "),
        artist_uri: track.artist.first().map(|a| uri_of("artist", a.gid())).unwrap_or_default(),
        album: track.album.name().to_owned(),
        album_uri: uri_of("album", track.album.gid()),
        duration_ms: track.duration().max(0) as u32,
        cover: small_cover(&track.album.cover_group.image),
    }
}

fn small_cover(images: &[metadata::Image]) -> String {
    images
        .iter()
        .filter(|i| i.file_id().len() == 20)
        .min_by_key(|i| i.width())
        .map(|i| FileId::from_raw(i.file_id()).to_base16())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_protocol::{context_track::ContextTrack, playlist4_external::{Item, MetaItem}};

    #[test]
    fn search_text_cannot_break_out_of_the_request_path() {
        assert_eq!(search_uri(" never gonna "), "spotify:search:never+gonna");
        assert_eq!(search_uri("AC/DC? #1 & é"), "spotify:search:AC%2FDC%3F+%231+%26+%C3%A9");
    }

    #[test]
    fn page_urls_become_contexts() {
        assert_eq!(page_context("hm://artistplaycontext/v1/page/spotify/album/5LFz/km_artist").as_deref(), Some("spotify:album:5LFz"));
        assert_eq!(page_context(""), None);
        assert_eq!(page_context("hm://x/spotify/album"), None);
    }

    #[test]
    fn queue_advances_past_the_playing_track() {
        let track = |u: &str| QueueTrack { uri: u.into(), provider: "context".into() };
        let mut queue = vec![track("a"), track("b"), track("c")];
        advance(&mut queue, "b");
        assert_eq!(queue.iter().map(|t| t.uri.as_str()).collect::<Vec<_>>(), ["c"]);
        advance(&mut queue, "unknown");
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn only_tracks_survive_and_queries_are_dropped() {
        let page = ContextPage {
            tracks: ["spotify:track:1?q=x", "spotify:episode:2", "spotify:local:a:b:c:1", "spotify:track:3"]
                .iter()
                .map(|u| ContextTrack { uri: Some(u.to_string()), ..Default::default() })
                .collect(),
            ..Default::default()
        };
        assert_eq!(track_uris(&page).collect::<Vec<_>>(), ["spotify:track:1", "spotify:track:3"]);
    }

    #[test]
    fn folders_are_not_playlists() {
        let mut list = SelectedListContent::default();
        let contents = list.contents.mut_or_insert_default();
        for (uri, name) in [("spotify:start-group:1:F", ""), ("spotify:playlist:x", "Mix"), ("spotify:end-group:1", "")] {
            contents.items.push(Item { uri: Some(uri.into()), ..Default::default() });
            let mut meta = MetaItem::default();
            meta.attributes.mut_or_insert_default().set_name(name.into());
            contents.meta_items.push(meta);
        }
        assert_eq!(playlists_in(&list), [Card { title: "Mix".into(), subtitle: "Playlist".into(), uri: "spotify:playlist:x".into(), picture: String::new() }]);
        list.contents.mut_or_insert_default().meta_items[1].set_owner_username("me".into());
        assert_eq!(editable_in(&list, "me"), ["spotify:playlist:x"]);
        assert_eq!(owned_in(&list, "me"), ["spotify:playlist:x"]);
        assert!(owned_in(&list, "someone else").is_empty());
        assert!(editable_in(&list, "someone else").is_empty());
    }

    #[test]
    fn the_feed_becomes_shelves() {
        let header = |title: &str| serde_json::json!({ "component": { "category": "header" }, "text": { "title": title } });
        let card = |title: &str, uri: &str| serde_json::json!({ "component": { "category": "card" }, "text": { "title": title, "subtitle": "s" },
            "target": { "uri": uri }, "images": { "main": { "uri": "https://x/y" } } });
        let mut body = vec![card("before any heading", "spotify:album:1"), header("Empty"), header(" Mixes ")];
        body.extend((0..30).map(|i| card(&format!("Mix {i}"), "spotify:playlist:1")));
        body.extend([header("Shows"), card("A show", "spotify:show:1"), header("Last"), card("Liked", "spotify:user:me:collection"), serde_json::json!({})]);
        let shelves = shelves_in(&serde_json::json!({ "body": body }));
        assert_eq!(shelves.iter().map(|s| (s.title.as_str(), s.cards.len())).collect::<Vec<_>>(), [("Mixes", CARDS), ("Last", 1)]);
        assert_eq!(shelves[0].cards[0].picture, "https://x/y");
        assert_eq!(shelves_in(&serde_json::json!({ "error": 1 })), []);
    }

    #[test]
    fn jams_are_read_in_either_spelling() {
        let jam = jam_in(&serde_json::json!({ "session_members": [{ "display_name": "A" }, {}], "join_session_url": "https://x/y" }));
        assert_eq!(jam, Some(Jam { names: vec!["A".into(), "Someone".into()], link: "https://x/y".into() }));
        let jam = jam_in(&serde_json::json!({ "sessionMembers": [{ "displayName": "B" }], "joinSessionUrl": "javascript:x" }));
        assert_eq!(jam, Some(Jam { names: vec!["B".into()], link: String::new() }));
        assert_eq!(jam_in(&serde_json::json!({ "session_members": [] })), None);
        assert_eq!(jam_in(&serde_json::json!({})), None);
    }

    #[test]
    fn pictures_are_only_fetched_from_safe_addresses() {
        let id = "ab".repeat(20);
        let cdn = format!("https://i.scdn.co/image/{id}");
        assert_eq!(picture_urls(&id), [cdn.clone()]);
        assert_eq!(picture_urls(&format!("spotify:image:{id}")), [cdn.clone()]);
        assert_eq!(picture_urls(&format!("spotify:mosaic:{id}:{id}:{id}:{id}")).len(), 4);
        assert_eq!(picture_urls("https://a/b"), ["https://a/b"]);
        for bad in ["", "http://a/b", "file:///c", "../x", "spotify:image:../x"] {
            assert_eq!(picture_urls(bad), Vec::<String>::new(), "{bad}");
        }
    }

    #[test]
    fn row_links_to_its_album_and_first_artist() {
        let mut track = metadata::Track::new();
        track.set_name("Song".into());
        track.set_duration(61_000);
        for name in ["A", "B"] {
            let mut artist = metadata::Artist::new();
            artist.set_name(name.into());
            artist.set_gid(vec![0; 16]);
            track.artist.push(artist);
        }
        let row = row("spotify:track:x", &track);
        assert_eq!((row.artist.as_str(), row.duration_ms), ("A, B", 61_000));
        assert_eq!(row.artist_uri, "spotify:artist:0000000000000000000000");
        assert_eq!(row.album_uri, "", "a track without album data must not link anywhere");
    }

    #[test]
    fn only_certificate_errors_blame_the_network() {
        let os = |code| explain_chain(&std::io::Error::from_raw_os_error(code));
        assert!(os(0x800B0109_u32 as i32).contains("intercepting"));
        assert!(!os(10060).contains("intercepting"), "a timeout is not interception");
    }

    #[test]
    fn the_smallest_usable_cover_is_picked() {
        let image = |width, id: Vec<u8>| {
            let mut image = metadata::Image::new();
            image.set_width(width);
            image.set_file_id(id);
            image
        };
        let images = [image(640, vec![1; 20]), image(64, vec![0xab; 20]), image(32, vec![9; 3])];
        assert_eq!(small_cover(&images), "ab".repeat(20));
        assert_eq!(small_cover(&[]), "");
    }

    /// Reads the signed-in account without playing anything; prints counts only.
    #[tokio::test]
    #[ignore = "needs the network and a saved login"]
    async fn live() {
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Rustbeat");
        let login = Cache::new(Some(&dir), None, None, None).unwrap().credentials().expect("not signed in");
        let session = Session::new(SessionConfig::default(), None);
        session.connect(login, false).await.unwrap();
        let playlists = session.spclient().get_rootlist(0, None).await.unwrap();
        let playlists = playlists_in(&SelectedListContent::parse_from_bytes(&playlists).unwrap());
        println!("playlists: {}", playlists.len());
        for uri in [format!("spotify:user:{}:collection", session.username()), search_uri("piano")] {
            let pages = session.spclient().get_context(&uri).await.unwrap().pages;
            let uris: Vec<_> = pages.iter().flat_map(track_uris).collect();
            let rows = load_rows(&session, &uris[..uris.len().min(CHUNK)]).await.unwrap();
            let covers = rows.iter().filter(|r| !r.cover.is_empty()).count();
            println!("tracks: {}, first rows: {}, with cover: {covers}", uris.len(), rows.len());
            let mut lyrics = [0; 3];
            for row in rows.iter().take(8) {
                match load_lyrics(&session, &row.uri).await {
                    Ok((synced, lines)) if !lines.is_empty() => lyrics[synced as usize] += 1,
                    Ok(_) => lyrics[2] += 1,
                    Err(e) => {
                        lyrics[2] += 1;
                        println!("  lyrics error kind: {:?}", e.kind);
                    }
                }
            }
            println!("  lyrics of 8: {} plain, {} timed, {} none", lyrics[0], lyrics[1], lyrics[2]);
            if let Some(row) = rows.iter().find(|r| !r.cover.is_empty()) {
                let bytes = session.spclient().request_url(&format!("https://i.scdn.co/image/{}", row.cover)).await.unwrap();
                println!("  cover bytes: {}", bytes.len());
            }
        }
    }

    #[tokio::test]
    #[ignore = "needs the network and a saved login; prints counts only"]
    async fn home() {
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Rustbeat");
        let login = Cache::new(Some(&dir), None, None, None).unwrap().credentials().expect("not signed in");
        let session = Session::new(SessionConfig::default(), None);
        session.connect(login, false).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut library = Library::new(session, Arc::new(move |e| drop(tx.send(e))));
        library.load_start();
        tokio::time::sleep(Duration::from_secs(4)).await;
        let mut pictures = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::Home(shelves) => {
                    println!("shelves {}, cards {}", shelves.len(), shelves.iter().map(|s| s.cards.len()).sum::<usize>());
                    pictures.extend(shelves.into_iter().flat_map(|s| s.cards).map(|c| c.picture));
                }
                Event::Playlists(p) => println!("playlists {}, with picture {}", p.len(), p.iter().filter(|c| !c.picture.is_empty()).count()),
                Event::NoHome(e) | Event::Status(e) => println!("{e}"),
                _ => {}
            }
        }
        let wanted = pictures.len().min(40);
        for id in pictures.into_iter().take(wanted) {
            library.handle(Cmd::Picture { kind: 0, id });
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut got = 0;
        while let Ok(event) = rx.try_recv() {
            got += matches!(event, Event::Picture { images, .. } if images.iter().any(|i| i.len() > 500)) as i32;
        }
        println!("pictures {got} of {wanted}");
    }

    /// Presses play on Liked Songs with the built-in player and prints what the window is told.
    #[tokio::test]
    #[ignore = "needs the network and a saved login; takes over playback"]
    async fn press_play() {
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Rustbeat");
        let (tx, rx) = mpsc::unbounded_channel();
        let (out, mut events) = mpsc::unbounded_channel();
        tokio::spawn(run(rx, Arc::new(move |e| drop(out.send(e))), dir, Arc::new(Shared::new(Default::default()))));
        let start = Instant::now();
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(45), events.recv()).await {
            let at = start.elapsed().as_secs_f32();
            match event {
                Event::Ready => {
                    println!("{at:.1} ready, pressing play");
                    let user = Cache::new(Some(PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Rustbeat")), None, None, None).unwrap();
                    let user = user.credentials().unwrap().username.unwrap();
                    tx.send(Cmd::Play { context: format!("spotify:user:{user}:collection"), track: String::new() }).unwrap();
                }
                Event::Status(s) => println!("{at:.1} says: {s}"),
                Event::Offline(s) => println!("{at:.1} offline: {s}"),
                Event::Position { playing: true, .. } => println!("{at:.1} PLAYING"),
                Event::Blocked => {
                    println!("{at:.1} BLOCKED dialog");
                    break;
                }
                _ => {}
            }
        }
        tx.send(Cmd::Quit).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    /// Typing: each search replaces the one before, and the list changes only when there are songs.
    #[tokio::test]
    #[ignore = "needs the network and a saved login"]
    async fn typing() {
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Rustbeat");
        let login = Cache::new(Some(&dir), None, None, None).unwrap().credentials().expect("not signed in");
        let session = Session::new(SessionConfig::default(), None);
        session.connect(login, false).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut library = Library::new(session, Arc::new(move |e| drop(tx.send(e))));
        let start = Instant::now();
        for q in ["pi", "pian", "piano", "zzqxjvkwqpz"] {
            library.handle(Cmd::Search(q.into()));
            tokio::time::sleep(Duration::from_millis(if q == "piano" { 2500 } else { 150 })).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::List { title, .. } => println!("list: {title}"),
                Event::Rows(rows) => println!("rows: {}", rows.len()),
                Event::ListDone => println!("done"),
                Event::Status(s) => println!("status: {s}"),
                _ => {}
            }
        }
        println!("in {:?}", start.elapsed());
    }

    /// Walks every step before sound for a few songs, without playing; prints where it stops.
    #[tokio::test]
    #[ignore = "needs the network and a saved login"]
    async fn streaming() {
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Rustbeat");
        let login = Cache::new(Some(&dir), None, None, None).unwrap().credentials().expect("not signed in");
        let session = Session::new(SessionConfig::default(), None);
        session.connect(login, false).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!("plan: {:?}", session.get_user_attribute("type"));
        for uri in [format!("spotify:user:{}:collection", session.username()), search_uri("love")] {
            let pages = session.spclient().get_context(&uri).await.unwrap().pages;
            for uri in pages.iter().flat_map(track_uris).take(5) {
                let track = SpotifyUri::from_uri(&uri).unwrap();
                let SpotifyUri::Track { id } = track else { continue };
                let item = match AudioItem::get_file(&session, track).await {
                    Ok(item) => item,
                    Err(e) => {
                        println!("metadata: {e}");
                        continue;
                    }
                };
                let formats: Vec<_> = item.files.keys().map(|f| format!("{f:?}")).collect();
                let vorbis = item.files.iter().find(|(f, _)| format!("{f:?}").contains("OGG_VORBIS_160")).map(|(_, id)| *id);
                let Some(file) = vorbis else {
                    println!("available {:?}, no vorbis, formats {formats:?}, alternatives {}", item.availability, item.alternatives.map_or(0, |a| a.0.len()));
                    continue;
                };
                let cdn = librespot_core::cdn_url::CdnUrl::new(file).resolve_audio(&session).await;
                let bytes = match &cdn {
                    Ok(cdn) => match session.http_client().request_body(
                        http::Request::get(cdn.try_get_urls().unwrap()[0]).header("range", "bytes=0-65535").body(Default::default()).unwrap(),
                    ).await {
                        Ok(b) => format!("{} bytes", b.len()),
                        Err(e) => format!("CDN {e}"),
                    },
                    Err(e) => format!("resolve {e}"),
                };
                let key = session.audio_key().request(id, file).await.map(|_| "ok").map_err(|e| e.to_string());
                println!("available {}, file {bytes}, key {key:?}, {} formats", item.availability.is_ok(), formats.len());
            }
        }
    }

    #[tokio::test]
    #[ignore = "needs the network; prints why Spotify can't be reached from here"]
    async fn probe() {
        match Session::new(SessionConfig::default(), None).spclient().client_token().await {
            Ok(_) => println!("reachable"),
            Err(e) => println!("{}\n{e:?}", explain(&e)),
        }
    }
}
