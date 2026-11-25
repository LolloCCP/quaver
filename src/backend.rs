use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Duration,
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
use tokio::{
    sync::{Notify, Semaphore, mpsc},
    task::JoinHandle,
};

use crate::{
    eq::EqSink,
    settings::Shared,
};

const CHUNK: usize = 100;
/// Covers fetched at the same time.
const COVER_FETCHES: usize = 6;

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
    /// Fetch a small cover by its image id.
    Thumb(String),
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
    Quit,
}

pub enum Event {
    NeedLogin(String),
    Connecting(String),
    /// Waiting before the next attempt.
    Offline(String),
    Ready,
    Status(String),
    Playlists(Vec<(String, String)>),
    /// A new list is being shown. `context` is empty for the queue.
    List { title: String, context: String },
    Rows(Vec<Row>),
    ListDone,
    Track { uri: String, title: String, artist: String, album: String, duration_ms: u32, cover_url: Option<String> },
    Cover { uri: String, bytes: Vec<u8> },
    /// `bytes` is empty when the image could not be fetched.
    Thumb { id: String, bytes: Vec<u8> },
    /// Lines with their start time; `note` says why there are none.
    Lyrics { uri: String, synced: bool, lines: Vec<(u32, String)>, note: &'static str },
    Position { ms: u32, playing: bool },
    Stopped,
    Volume(u16),
    Shuffle(bool),
    Repeat(u8),
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

pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

/// What made a connected session end.
enum End {
    Quit,
    Logout,
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

    loop {
        let Some(login) = credentials.clone() else {
            emit(Event::NeedLogin(std::mem::take(&mut notice)));
            loop {
                match rx.recv().await {
                    Some(Cmd::Connect) => break,
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
        let error = match connected(&mut rx, &emit, &cache, login, &mut retry, &shared).await {
            Ok(End::Quit) => return emit(Event::Quit),
            Ok(End::Logout) => {
                notice = forget(&data_dir).err().unwrap_or_default();
                credentials = None;
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
                    Some(Cmd::Connect) => break,
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
            "This network is intercepting Spotify's secure connection, so Quaver stopped before ",
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
    .with_custom_message("Signed in. You can close this tab and go back to Quaver.")
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
) -> Result<End, Error> {
    let session = Session::new(SessionConfig::default(), Some(cache.clone()));
    // Fails early on a blocked network, before the audio device is opened.
    tokio::select! {
        r = session.spclient().client_token() => { r?; }
        () = quit_requested(rx) => return Ok(End::Quit),
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
        move || Box::new(EqSink::new(sink(None, AudioFormat::default()), eq_settings)),
    );
    let mut events = player.get_player_event_channel();
    let mut volume = cache.volume().unwrap_or(u16::MAX / 2);
    let config = ConnectConfig {
        name: "Quaver".into(),
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
                _ => {}
            },
        }
    };
    *retry = Duration::from_secs(5);
    let mut spirc_task = tokio::spawn(spirc_task);
    emit(Event::Ready);
    emit(Event::Volume(volume));

    spawn_reporting(emit, load_playlists(session.clone(), emit.clone()));

    let mut browse: Option<JoinHandle<()>> = None;
    let mut more = Arc::new(Notify::new());
    let mut queue: Vec<QueueTrack> = Vec::new();
    let mut queue_shown = false;
    let mut has_queued = false;
    let mut options = Options::default();
    let cover_slots = Arc::new(Semaphore::new(COVER_FETCHES));

    let end = loop {
        tokio::select! {
            _ = &mut spirc_task => break End::Lost,
            cmd = rx.recv() => {
                let mut open = |title: String, uri: String| {
                    if let Some(task) = browse.take() { task.abort() }
                    more = Arc::new(Notify::new());
                    emit(Event::List { title, context: uri.clone() });
                    browse = Some(spawn_reporting(emit, load_context(session.clone(), uri, emit.clone(), more.clone())));
                };
                let sent = match cmd {
                    Some(Cmd::Quit) | None => break End::Quit,
                    Some(Cmd::Logout) => break End::Logout,
                    Some(Cmd::Connect) => Ok(()),
                    Some(Cmd::Browse { title, uri }) => { queue_shown = false; open(title, uri); Ok(()) }
                    Some(Cmd::Liked) => {
                        queue_shown = false;
                        open("Liked Songs".into(), format!("spotify:user:{}:collection", session.username()));
                        Ok(())
                    }
                    Some(Cmd::Search(q)) => {
                        queue_shown = false;
                        open(format!("Results for “{q}”"), search_uri(&q));
                        Ok(())
                    }
                    Some(Cmd::Queue) => {
                        queue_shown = true;
                        show_queue(&session, emit, &mut browse, &queue);
                        Ok(())
                    }
                    Some(Cmd::More) => { more.notify_one(); Ok(()) }
                    Some(Cmd::Play { context, track }) => {
                        let request = LoadRequestOptions {
                            start_playing: true,
                            context_options: Some(LoadContextOptions::Options(options.clone())),
                            playing_track: Some(PlayingTrack::Uri(track.clone())),
                            ..Default::default()
                        };
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
                    Some(Cmd::Enqueue(uri)) => {
                        has_queued = true;
                        SpotifyUri::from_uri(&uri).and_then(|uri| spirc.add_to_queue(uri))
                    }
                    Some(Cmd::ClearQueue) => spirc.clear_queue(),
                    Some(Cmd::Thumb(id)) => {
                        let (session, emit, slots) = (session.clone(), emit.clone(), cover_slots.clone());
                        tokio::spawn(async move {
                            let _slot = slots.acquire().await;
                            let bytes = session.spclient().request_url(&format!("https://i.scdn.co/image/{id}")).await;
                            let bytes = bytes.inspect_err(|e| log::warn!("cover: {e}")).unwrap_or_default().to_vec();
                            emit(Event::Thumb { id, bytes });
                        });
                        Ok(())
                    }
                    Some(Cmd::Lyrics(uri)) => {
                        let (session, emit) = (session.clone(), emit.clone());
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
                        Ok(())
                    }
                    Some(Cmd::Toggle) => spirc.play_pause(),
                    Some(Cmd::Resume) => spirc.play(),
                    Some(Cmd::Pause) => spirc.pause(),
                    Some(Cmd::Next) => spirc.next(),
                    Some(Cmd::Prev) => spirc.prev(),
                    Some(Cmd::Seek(ms)) => spirc.set_position_ms(ms),
                    Some(Cmd::Volume(v)) => {
                        volume = v;
                        emit(Event::Volume(v));
                        spirc.set_volume(v)
                    }
                    Some(Cmd::Shuffle(on)) => {
                        options.shuffle = on;
                        emit(Event::Shuffle(on));
                        spirc.shuffle(on)
                    }
                    Some(Cmd::Repeat(mode)) => {
                        (options.repeat, options.repeat_track) = (mode == 1, mode == 2);
                        emit(Event::Repeat(mode));
                        spirc.repeat(mode == 1).and_then(|()| spirc.repeat_track(mode == 2))
                    }
                };
                if let Err(e) = sent {
                    emit(Event::Status(e.error.to_string()));
                }
            }
            event = events.recv() => match event {
                None => break End::Lost,
                Some(PlayerEvent::TrackChanged { audio_item }) => {
                    advance(&mut queue, &audio_item.uri);
                    if queue_shown { show_queue(&session, emit, &mut browse, &queue) }
                    track_changed(&session, emit, *audio_item);
                }
                Some(PlayerEvent::Playing { position_ms, .. }) => emit(Event::Position { ms: position_ms, playing: true }),
                Some(PlayerEvent::Paused { position_ms, .. } | PlayerEvent::Loading { position_ms, .. }) =>
                    emit(Event::Position { ms: position_ms, playing: false }),
                Some(PlayerEvent::Seeked { position_ms, .. } | PlayerEvent::PositionCorrection { position_ms, .. }) =>
                    emit(Event::Position { ms: position_ms, playing: true }),
                Some(PlayerEvent::Stopped { .. }) => emit(Event::Stopped),
                Some(PlayerEvent::Unavailable { .. }) => emit(Event::Status("This track isn't available.".into())),
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
                        if queue_shown { show_queue(&session, emit, &mut browse, &queue) }
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
                    if queue_shown { show_queue(&session, emit, &mut browse, &queue) }
                }
                Some(_) => {}
            },
        }
    };

    if let Some(task) = browse {
        task.abort();
    }
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

fn show_queue(session: &Session, emit: &Emit, browse: &mut Option<JoinHandle<()>>, queue: &[QueueTrack]) {
    if let Some(task) = browse.take() {
        task.abort();
    }
    emit(Event::List { title: "Up next".into(), context: String::new() });
    let uris: Vec<_> = queue.iter().map(|t| t.uri.clone()).collect();
    let (session, out) = (session.clone(), emit.clone());
    *browse = Some(spawn_reporting(emit, async move {
        for chunk in uris.chunks(CHUNK) {
            out(Event::Rows(load_rows(&session, chunk).await?));
        }
        out(Event::ListDone);
        Ok(())
    }));
}

/// Drops everything up to and including the track that just started.
fn advance(queue: &mut Vec<QueueTrack>, now: &str) {
    if let Some(i) = queue.iter().position(|t| t.uri == now) {
        queue.drain(..=i);
    }
}

fn track_changed(session: &Session, emit: &Emit, item: AudioItem) {
    let (artist, album) = match item.unique_fields {
        UniqueFields::Track { artists, album, .. } => {
            (artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "), album)
        }
        UniqueFields::Episode { show_name, .. } => (show_name, String::new()),
        UniqueFields::Local { artists, album, .. } => (artists.unwrap_or_default(), album.unwrap_or_default()),
    };
    let cover_url = item.covers.into_iter().max_by_key(|c| c.width).map(|c| c.url);
    emit(Event::Track { uri: item.uri.clone(), title: item.name, artist, album, duration_ms: item.duration_ms, cover_url: cover_url.clone() });
    if let Some(url) = cover_url {
        let (session, emit) = (session.clone(), emit.clone());
        tokio::spawn(async move {
            match session.spclient().request_url(&url).await {
                Ok(bytes) => emit(Event::Cover { uri: item.uri, bytes: bytes.to_vec() }),
                Err(e) => log::warn!("cover: {e}"),
            }
        });
    }
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
    let mut playlists = Vec::new();
    let mut from = 0;
    loop {
        let list = SelectedListContent::parse_from_bytes(&session.spclient().get_rootlist(from, None).await?)?;
        let count = list.contents.items.len();
        playlists.extend(playlists_in(&list));
        from += count;
        if count == 0 || from >= list.length() as usize {
            break;
        }
    }
    emit(Event::Playlists(playlists));
    Ok(())
}

/// The root list also holds folder markers, which have no name to show.
fn playlists_in(list: &SelectedListContent) -> Vec<(String, String)> {
    list.contents
        .items
        .iter()
        .zip(&list.contents.meta_items)
        .filter(|(item, meta)| item.uri().contains(":playlist:") && !meta.attributes.name().is_empty())
        .map(|(item, meta)| (meta.attributes.name().to_owned(), item.uri().to_owned()))
        .collect()
}

fn search_uri(query: &str) -> String {
    format!("spotify:search:{}", form_urlencoded::byte_serialize(query.trim().as_bytes()).collect::<String>())
}

/// `hm://artistplaycontext/v1/page/spotify/album/<id>/km` -> `spotify:album:<id>`
fn page_context(page_url: &str) -> Option<String> {
    let mut parts = page_url.split('/').skip_while(|p| *p != "spotify").skip(1);
    Some(format!("spotify:{}:{}", parts.next()?, parts.next()?))
}

async fn load_context(session: Session, uri: String, emit: Emit, more: Arc<Notify>) -> Result<(), Error> {
    let mut pages: VecDeque<ContextPage> = session.spclient().get_context(&uri).await?.pages.into();
    let mut uris: VecDeque<String> = VecDeque::new();
    loop {
        while uris.len() < CHUNK
            && let Some(page) = pages.pop_front()
        {
            if page.tracks.is_empty() && let Some(context) = page_context(page.page_url()) {
                let resolved = session.spclient().get_context(&context).await?;
                uris.extend(resolved.pages.iter().flat_map(track_uris));
            }
            uris.extend(track_uris(&page));
            if !page.next_page_url().is_empty() {
                let json = session.spclient().get_next_page(page.next_page_url()).await?;
                let options = protobuf_json_mapping::ParseOptions { ignore_unknown_fields: true, ..Default::default() };
                let next = protobuf_json_mapping::parse_from_str_with_options(&String::from_utf8_lossy(&json), &options)?;
                pages.push_front(next);
            }
        }
        if uris.is_empty() {
            emit(Event::ListDone);
            return Ok(());
        }
        let chunk: Vec<_> = uris.drain(..uris.len().min(CHUNK)).collect();
        emit(Event::Rows(load_rows(&session, &chunk).await?));
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
        assert_eq!(playlists_in(&list), [("Mix".to_string(), "spotify:playlist:x".to_string())]);
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
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Quaver");
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
    #[ignore = "needs the network; prints why Spotify can't be reached from here"]
    async fn probe() {
        match Session::new(SessionConfig::default(), None).spclient().client_token().await {
            Ok(_) => println!("reachable"),
            Err(e) => println!("{}\n{e:?}", explain(&e)),
        }
    }
}
