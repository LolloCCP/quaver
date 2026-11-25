//! Plays through Spotify Soloist, Spotify's own headless player, over its local WebSocket API
//! (developer.spotify.com/documentation/soloist/reference/websocket-api).

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cpal::traits::{DeviceTrait, HostTrait};
use futures_util::{SinkExt, StreamExt};
use librespot_core::{Error, cache::Cache};
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    process::{Child, Command},
    sync::mpsc,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::{
    backend::{Cmd, Emit, End, Event, Jam, Library},
    sequence::{Sequence, Started},
};

const QUEUE_SHOWN: usize = 80;
/// How often to look whether Windows plays somewhere else now.
const OUTPUT_CHECK: Duration = Duration::from_secs(2);
/// How often to look for a Jam.
const JAM_CHECK: Duration = Duration::from_secs(15);
/// Pressing "previous" later than this into a song restarts it.
const RESTART_AFTER_MS: u32 = 3000;

pub struct Setup {
    pub exe: PathBuf,
    pub key: String,
}

impl Setup {
    pub fn dir(data_dir: &Path) -> PathBuf {
        data_dir.join("soloist")
    }

    // shortcut: the key is a plain file in the user's profile, like the login next to it,
    // move both to Windows Credential Manager if other people use this account.
    pub fn key_file(data_dir: &Path) -> PathBuf {
        data_dir.join("soloist-key.txt")
    }

    /// The zip may be unpacked into the folder or into a folder inside it.
    pub fn find_exe(data_dir: &Path) -> Option<PathBuf> {
        let dir = Self::dir(data_dir);
        let inside = std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path());
        std::iter::once(dir.clone()).chain(inside).map(|d| d.join("soloist.exe")).find(|exe| exe.is_file())
    }

    pub fn load(data_dir: &Path) -> Result<Self, &'static str> {
        let exe = Self::find_exe(data_dir).ok_or("Soloist is not in its folder yet")?;
        let key = std::fs::read_to_string(Self::key_file(data_dir)).unwrap_or_default().trim().to_owned();
        if key.is_empty() {
            return Err("Soloist needs your API key");
        }
        Ok(Self { exe, key })
    }
}

const DOWNLOAD: &str = "https://soloist-builds.spotifycdn.com/soloist_release_windows_x86_64.zip";

/// Fetches the current build straight from Spotify and puts it in place. Takes a while.
pub fn download(data_dir: &Path) -> Result<(), String> {
    let (zip, fresh, dir) = (data_dir.join("soloist.zip"), data_dir.join("soloist-new"), Setup::dir(data_dir));
    let _ = std::fs::remove_dir_all(&fresh);
    std::fs::create_dir_all(&fresh).map_err(|e| e.to_string())?;
    // Both programs are part of Windows; curl checks certificates and only follows https.
    run_tool("curl.exe", &["--fail", "--silent", "--show-error", "--location", "--proto", "=https", "--proto-redir", "=https", "--max-time", "900", "--output"], &[zip.as_os_str(), DOWNLOAD.as_ref()])?;
    let unpacked = run_tool("tar.exe", &["-xf"], &[zip.as_os_str(), "-C".as_ref(), fresh.as_os_str()]);
    let _ = std::fs::remove_file(&zip);
    unpacked?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("The old build could not be replaced: {e}"))?;
    }
    std::fs::rename(&fresh, &dir).map_err(|e| e.to_string())?;
    Setup::find_exe(data_dir).map(|_| ()).ok_or_else(|| "There was no soloist.exe in the download".into())
}

fn run_tool(program: &str, options: &[&str], rest: &[&std::ffi::OsStr]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_default()).join("System32");
    let output = std::process::Command::new(system.join(program)).args(options).args(rest).creation_flags(CREATE_NO_WINDOW).output();
    match output {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("it failed without a reason").to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The name of what Windows plays through, such as the earbuds that were just connected.
fn output() -> Option<String> {
    cpal::default_host().default_output_device()?.name().ok()
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn launch(setup: &Setup, data_dir: &Path) -> Result<(Child, Socket), Error> {
    let state = data_dir.join("soloist-state");
    std::fs::create_dir_all(&state)?;
    let port_file = state.join("ws.port");
    // Left behind if Soloist was killed; it would point at a port that is gone.
    let _ = std::fs::remove_file(&port_file);
    // shortcut: if Quaver itself crashes, Soloist keeps running until it is ended in Task Manager,
    // put both in a Windows job object if that happens.
    let mut child = Command::new(&setup.exe)
        .args(["--device-name", "Quaver", "--ws", "127.0.0.1:0", "--cache-size", "500", "--api-key", &setup.key])
        .arg("--data-dir")
        .arg(&state)
        .arg("--cache-dir")
        .arg(data_dir.join("soloist-cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .spawn()?;
    for _ in 0..150 {
        if let Some(status) = child.try_wait()? {
            return Err(Error::unavailable(match status.code() {
                Some(10) => "This Soloist build has expired. Download the current one in Settings",
                _ => "Soloist stopped. Check the API key, and that no other Soloist is running",
            }));
        }
        if let Some(port) = std::fs::read_to_string(&port_file).ok().and_then(|p| p.trim().parse::<u16>().ok())
            && let Ok((socket, _)) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}")).await
        {
            return Ok((child, socket));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Error::deadline_exceeded("Soloist did not start"))
}

pub async fn run(
    library: &mut Library,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    emit: &Emit,
    cache: &Cache,
    setup: &Setup,
    data_dir: &Path,
) -> Result<End, Error> {
    let mut playing_on = tokio::task::spawn_blocking(output).await.ok().flatten();
    let (mut child, socket) = {
        let starting = launch(setup, data_dir);
        tokio::pin!(starting);
        loop {
            tokio::select! {
                r = &mut starting => break r?,
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Quit) | None => return Ok(End::Quit),
                    Some(Cmd::Logout) => return Ok(End::Logout),
                    Some(Cmd::Restart) => return Ok(End::Restart),
                    _ => {}
                },
            }
        }
    };
    emit(Event::Ready);
    library.load_start();
    let mut session_check = tokio::time::interval(Duration::from_secs(5));
    let mut output_check = tokio::time::interval(OUTPUT_CHECK);
    let mut jam_check = tokio::time::interval(JAM_CHECK);
    let (jam_tx, mut jams) = mpsc::unbounded_channel();
    let mut player = Player { socket, emit: emit.clone(), sequence: Sequence::default(), track: String::new(), playing: false, position: (0, 0), paired: None, jam: None, outside: vec![], resume: None, catch_up: None };
    player.set_volume(cache.volume().unwrap_or(u16::MAX / 2)).await;

    loop {
        tokio::select! {
            _ = session_check.tick() => if library.session.is_invalid() { return Ok(End::Lost) },
            _ = output_check.tick() => {
                // Soloist stays on the output it started with and can't be told to change it.
                let now = tokio::task::spawn_blocking(output).await.ok().flatten();
                if now.is_none() || now == playing_on {
                    continue;
                }
                playing_on = now;
                player.remember();
                if let Err(e) = child.kill().await {
                    log::warn!("soloist: {e}");
                }
                let (new_child, socket) = launch(setup, data_dir).await?;
                (child, player.socket) = (new_child, socket);
                player.set_volume(cache.volume().unwrap_or(u16::MAX / 2)).await;
            }
            _ = jam_check.tick() => {
                let (session, tx) = (library.session.clone(), jam_tx.clone());
                tokio::spawn(async move {
                    match Library::jam(&session).await {
                        Ok(jam) => drop(tx.send(jam)),
                        Err(e) => log::info!("jam: {e}"),
                    }
                });
            }
            Some(jam) = jams.recv() => player.set_jam(jam),
            status = child.wait() => {
                log::warn!("soloist ended: {status:?}");
                return Err(Error::unavailable("Soloist stopped"));
            }
            message = player.socket.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let Ok(event) = serde_json::from_str(text.as_str()) else { continue };
                    if player.event(&event).await {
                        if let Some(url) = cover(&event["item"]) {
                            library.load_cover(player.track.clone(), url);
                        }
                        player.show_queue(library).await;
                    } else if event["type"] == "queue_changed" && player.queue_changed(&event) && library.queue_shown {
                        library.show_queue(player.outside.clone());
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return Ok(End::Lost),
            },
            cmd = rx.recv() => {
                let cmd = match cmd {
                    Some(Cmd::Quit) | None => return Ok(End::Quit),
                    Some(Cmd::Logout) => return Ok(End::Logout),
                    Some(Cmd::Restart) => return Ok(End::Restart),
                    Some(Cmd::Volume(v)) => {
                        cache.save_volume(v);
                        Cmd::Volume(v)
                    }
                    Some(cmd) => cmd,
                };
                let Some(cmd) = library.handle(cmd) else { continue };
                if player.jam.is_some() {
                    player.jam_command(cmd).await;
                } else if let Cmd::Play { context, track } = &cmd && track.is_empty() {
                    match library.all_tracks(context).await {
                        Ok(tracks) => {
                            let first = player.sequence.start(tracks, "");
                            player.play(first).await;
                            player.hand_over().await;
                        }
                        Err(e) => emit(Event::Status(e.error.to_string())),
                    }
                } else if let Cmd::Play { context, track } = &cmd && !context.is_empty() {
                    // The whole list is needed to go on after this song; meanwhile it already plays.
                    let first = player.sequence.start(vec![track.clone()], track);
                    player.play(first).await;
                    match library.all_tracks(context).await {
                        Ok(tracks) => player.extend(tracks).await,
                        Err(e) => emit(Event::Status(format!("Only this song could be loaded: {}", e.error))),
                    }
                } else {
                    player.command(cmd).await;
                }
                player.show_queue(library).await;
            }
        }
    }
}

struct Player {
    socket: Socket,
    emit: Emit,
    sequence: Sequence,
    track: String,
    playing: bool,
    /// Position and when it was valid, in ms since 1970.
    position: (u32, u64),
    paired: Option<bool>,
    /// While there is a Jam, the order of songs is its business.
    jam: Option<Jam>,
    /// What Soloist says comes next, for when the order isn't ours.
    outside: Vec<String>,
    /// What to go on with after a restart: the song, where in it, and whether it was playing.
    resume: Option<(String, u32, bool)>,
    /// Where to move to once the resumed song plays, and whether it should.
    catch_up: Option<(u32, bool)>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

impl Player {
    fn remember(&mut self) {
        self.catch_up = None;
        self.resume = (!self.track.is_empty()).then(|| (self.track.clone(), self.position_ms(), self.playing));
        self.sequence.player_restarted();
        self.playing = false;
    }

    async fn resume(&mut self) {
        let Some((uri, ms, playing)) = self.resume.take() else { return };
        self.send("activate", json!({})).await;
        if self.sequence.active() {
            self.send("play", json!({ "uri": uri })).await;
            self.catch_up = Some((ms, playing));
            self.hand_over().await;
        } else {
            // Someone else's order: Spotify knows how it goes on.
            if playing {
                self.send("play", json!({})).await;
            }
        }
    }

    /// A song that is still loading can't be moved in, so this waits until it plays.
    async fn catch_up(&mut self) {
        if self.playing && let Some((ms, playing)) = self.catch_up.take() {
            self.send("seek", json!({ "position_ms": ms })).await;
            if !playing {
                self.send("pause", json!({})).await;
            }
        }
    }

    async fn send(&mut self, command: &str, mut fields: Value) {
        fields["type"] = "command".into();
        fields["command"] = command.into();
        // A broken connection ends the loop in `run` through the reading side.
        if let Err(e) = self.socket.send(Message::text(fields.to_string())).await {
            log::warn!("soloist {command}: {e}");
        }
    }

    async fn play(&mut self, uri: Option<String>) {
        if let Some(uri) = uri {
            self.send("activate", json!({})).await;
            self.send("play", json!({ "uri": uri })).await;
        }
    }

    async fn hand_over(&mut self) {
        if let Some(uri) = self.sequence.hand_over() {
            self.send("add_to_queue", json!({ "uri": uri })).await;
        }
    }

    /// Fills in the list around the song that was started alone.
    async fn extend(&mut self, tracks: Vec<String>) {
        if let Some(current) = self.sequence.current().filter(|c| tracks.contains(c)) {
            self.sequence.start(tracks, &current);
            self.hand_over().await;
        }
    }

    async fn set_volume(&mut self, volume: u16) {
        (self.emit)(Event::Volume(volume));
        self.send("set_volume", json!({ "volume": (volume as u32 * 100 + 32767) / 65535 })).await;
    }

    fn position_ms(&self) -> u32 {
        let run = if self.playing { now_ms().saturating_sub(self.position.1) as u32 } else { 0 };
        self.position.0.saturating_add(run)
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Play { track, .. } => {
                let uri = self.sequence.jump(&track);
                self.play(uri).await;
            }
            Cmd::Toggle => self.send(if self.playing { "pause" } else { "play" }, json!({})).await,
            Cmd::Resume => self.send("play", json!({})).await,
            Cmd::Pause => self.send("pause", json!({})).await,
            Cmd::Next => self.send("skip_next", json!({})).await,
            Cmd::Prev => match self.position_ms() < RESTART_AFTER_MS && self.sequence.active() {
                true => match self.sequence.previous() {
                    Some(uri) => self.play(Some(uri)).await,
                    None => self.send("seek", json!({ "position_ms": 0 })).await,
                },
                false => self.send(if self.sequence.active() { "seek" } else { "skip_prev" }, json!({ "position_ms": 0 })).await,
            },
            Cmd::Seek(ms) => self.send("seek", json!({ "position_ms": ms })).await,
            Cmd::Volume(v) => self.set_volume(v).await,
            Cmd::Enqueue(uri) => {
                if self.sequence.active() {
                    self.sequence.enqueue(uri);
                    self.hand_over().await;
                } else {
                    self.send("add_to_queue", json!({ "uri": uri })).await;
                }
            }
            Cmd::ClearQueue => {
                self.sequence.clear_queue();
                self.hand_over().await;
            }
            Cmd::Shuffle(on) => {
                self.sequence.set_shuffle(on);
                (self.emit)(Event::Shuffle(on));
                self.hand_over().await;
            }
            Cmd::Repeat(mode) => {
                self.sequence.repeat = mode == 1;
                (self.emit)(Event::Repeat(mode));
                self.send("set_repeat_track", json!({ "enabled": mode == 2 })).await;
                self.hand_over().await;
            }
            _ => {}
        }
    }

    /// Returns whether the song changed.
    async fn event(&mut self, event: &Value) -> bool {
        let mut changed = false;
        match event["type"].as_str().unwrap_or_default() {
            "auth_state" => {
                let paired = event["logged_in"].as_bool().unwrap_or(false);
                if paired {
                    self.resume().await;
                }
                if self.paired.replace(paired) != Some(paired) {
                    (self.emit)(Event::Player(if paired {
                        "Connected. Soloist plays the sound.".into()
                    } else {
                        "Almost there: open Spotify on your phone or computer, on this network, and pick “Quaver” as the device. Once is enough.".into()
                    }));
                }
            }
            "playback_state" => {
                changed = self.item(&event["item"]).await;
                self.place(&event["position"]);
                self.status(&event["status"]);
                self.catch_up().await;
                if let Some(volume) = event["volume"].as_u64() {
                    (self.emit)(Event::Volume((volume.min(100) * 65535 / 100) as u16));
                }
            }
            "track_changed" => changed = self.item(&event["item"]).await,
            "playback_changed" => {
                self.status(&event["status"]);
                self.catch_up().await;
            }
            "position_sync" => {
                self.place(&event["position"]);
                self.show_position();
            }
            "volume_changed" => {
                if let Some(volume) = event["volume"].as_u64() {
                    (self.emit)(Event::Volume((volume.min(100) * 65535 / 100) as u16));
                }
            }
            "error" => {
                let message = event["message"].as_str().unwrap_or("Soloist refused that");
                log::warn!("soloist: {message}");
                (self.emit)(Event::Status(format!("Soloist: {message}")));
            }
            _ => {}
        }
        changed
    }

    async fn item(&mut self, item: &Value) -> bool {
        let Some(uri) = item["uri"].as_str().filter(|u| !u.is_empty() && *u != self.track) else { return false };
        match if self.jam.is_some() { Started::Foreign } else { self.sequence.started(uri) } {
            Started::Skip => {
                self.send("skip_next", json!({})).await;
                return false;
            }
            Started::Ours => self.hand_over().await,
            Started::Foreign => {}
        }
        self.track = uri.to_owned();
        self.position = (0, now_ms());
        let facts = &item["decorations"];
        let name = |entity: &Value| entity["decorations"]["identity"]["name"].as_str().unwrap_or_default().to_owned();
        let artists: Vec<_> = facts["creators"].as_array().into_iter().flatten().map(|c| name(&c["entity"])).collect();
        (self.emit)(Event::Track {
            uri: uri.to_owned(),
            title: name(item),
            artist: artists.join(", "),
            album: name(&facts["parent"]["entity"]),
            duration_ms: facts["playback"]["duration_ms"].as_u64().unwrap_or(0) as u32,
            cover_url: cover(item),
        });
        true
    }

    fn place(&mut self, position: &Value) {
        if let Some(ms) = position["position_ms"].as_u64() {
            self.position = (ms as u32, position["timestamp_ms"].as_u64().unwrap_or_else(now_ms));
        }
    }

    fn status(&mut self, status: &Value) {
        let position = self.position_ms();
        let playing = status == "playing";
        if playing != self.playing {
            self.position = (position, now_ms());
            self.playing = playing;
        }
        self.show_position();
    }

    fn show_position(&self) {
        (self.emit)(Event::Position { ms: self.position_ms(), playing: self.playing });
    }

    async fn show_queue(&mut self, library: &mut Library) {
        if !library.queue_shown {
            return;
        }
        if self.sequence.active() {
            library.show_queue(self.sequence.upcoming(QUEUE_SHOWN));
        } else {
            // The answer is a `queue_changed`.
            self.send("get_queue", json!({ "limit": QUEUE_SHOWN })).await;
        }
    }

    /// Returns whether the queue shown has to change.
    fn queue_changed(&mut self, event: &Value) -> bool {
        let uris: Vec<_> = event["upcoming"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["item"]["uri"].as_str())
            .filter(|uri| uri.starts_with("spotify:track:"))
            .map(String::from)
            .collect();
        let changed = !self.sequence.active() && uris != self.outside;
        self.outside = uris;
        changed
    }

    fn set_jam(&mut self, jam: Option<Jam>) {
        if self.jam == jam {
            return;
        }
        if jam.is_some() && self.jam.is_none() {
            self.sequence = Sequence::default();
        }
        (self.emit)(Event::Jam(jam.clone()));
        self.jam = jam;
    }

    /// In a Jam nothing is arranged here: everything is passed on as it is.
    async fn jam_command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Play { context, track } => self.play(Some(if track.is_empty() { context } else { track })).await,
            Cmd::Prev => self.send("skip_prev", json!({})).await,
            Cmd::Enqueue(uri) => self.send("add_to_queue", json!({ "uri": uri })).await,
            Cmd::ClearQueue => (self.emit)(Event::Status("The queue belongs to the Jam and can't be cleared here.".into())),
            Cmd::Shuffle(on) => {
                (self.emit)(Event::Shuffle(on));
                self.send("set_shuffle", json!({ "enabled": on })).await;
            }
            cmd => self.command(cmd).await,
        }
    }
}

/// The biggest cover of an item.
fn cover(item: &Value) -> Option<String> {
    let covers = item["decorations"]["visual_identity"]["cover"].as_array()?;
    let rank = |c: &&Value| ["small", "default", "large", "xlarge"].iter().position(|s| c["size"] == *s);
    Some(covers.iter().max_by_key(rank)?["url"].as_str()?.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    fn song(uri: &str) -> Value {
        json!({ "uri": uri, "entity_type": "track", "decorations": {
            "identity": { "name": format!("Name {uri}") },
            "visual_identity": { "cover": [{ "url": "s", "size": "small" }, { "url": "x", "size": "xlarge" }, { "url": "d", "size": "default" }] },
            "parent": { "entity": { "decorations": { "identity": { "name": "Album" } } } },
            "creators": [{ "entity": { "decorations": { "identity": { "name": "A" } } } }, { "entity": { "decorations": { "identity": { "name": "B" } } } }],
            "playback": { "duration_ms": 210000 } } })
    }

    /// A player connected to a stand-in for Soloist that records what it is sent.
    async fn player() -> (Player, Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<Event>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sent = Arc::new(Mutex::new(vec![]));
        let record = sent.clone();
        tokio::spawn(async move {
            let mut socket = tokio_tungstenite::accept_async(listener.accept().await.unwrap().0).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let v: Value = serde_json::from_str(text.as_str()).unwrap();
                assert_eq!(v["type"], "command");
                let extra = ["uri", "position_ms", "volume", "enabled"].iter().find_map(|k| v.get(*k)).map(|v| format!(" {v}"));
                record.lock().unwrap().push(format!("{}{}", v["command"].as_str().unwrap(), extra.unwrap_or_default()));
            }
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}")).await.unwrap();
        let events = Arc::new(Mutex::new(vec![]));
        let log = events.clone();
        let emit: Emit = Arc::new(move |e| log.lock().unwrap().push(e));
        let player = Player { socket, emit, sequence: Sequence::default(), track: String::new(), playing: false, position: (0, 0), paired: None, jam: None, outside: vec![], resume: None, catch_up: None };
        (player, sent, events)
    }

    async fn settled(sent: &Mutex<Vec<String>>) -> Vec<String> {
        tokio::time::sleep(Duration::from_millis(60)).await;
        std::mem::take(&mut *sent.lock().unwrap())
    }

    #[tokio::test]
    async fn a_list_plays_song_after_song() {
        let (mut p, sent, events) = player().await;
        let first = p.sequence.start(vec!["a".into()], "a");
        p.play(first).await;
        assert!(p.event(&json!({ "type": "track_changed", "item": song("a") })).await);
        p.extend(vec!["z".into(), "a".into(), "b".into(), "c".into()]).await;
        assert_eq!(settled(&sent).await, ["activate", "play \"a\"", "add_to_queue \"b\""]);
        assert!(p.event(&json!({ "type": "track_changed", "item": song("b") })).await);
        assert_eq!(settled(&sent).await, ["add_to_queue \"c\""]);
        assert!(!p.event(&json!({ "type": "track_changed", "item": song("b") })).await, "the same song again is no change");
        let events = events.lock().unwrap();
        let Event::Track { title, artist, album, duration_ms, cover_url, .. } = &events[0] else { panic!() };
        assert_eq!((title.as_str(), artist.as_str(), album.as_str(), *duration_ms), ("Name a", "A, B", "Album", 210000));
        assert_eq!(cover_url.as_deref(), Some("x"), "the biggest cover");
    }

    #[tokio::test]
    async fn leftovers_are_skipped() {
        let (mut p, sent, _) = player().await;
        for tracks in [["a", "b"], ["x", "y"]] {
            let first = p.sequence.start(tracks.map(String::from).to_vec(), tracks[0]);
            p.play(first).await;
            p.event(&json!({ "type": "track_changed", "item": song(tracks[0]) })).await;
        }
        settled(&sent).await;
        assert!(!p.event(&json!({ "type": "track_changed", "item": song("b") })).await);
        assert_eq!(settled(&sent).await, ["skip_next"]);
        assert!(p.event(&json!({ "type": "track_changed", "item": song("y") })).await);
    }

    #[tokio::test]
    async fn position_volume_and_repeat_follow_the_documented_fields() {
        let (mut p, sent, events) = player().await;
        let state = json!({ "type": "playback_state", "status": "playing", "item": song("a"), "volume": 100,
            "position": { "position_ms": 45000, "timestamp_ms": now_ms(), "speed": 1.0 } });
        p.event(&state).await;
        assert!((45000..45500).contains(&p.position_ms()), "{}", p.position_ms());
        assert!(events.lock().unwrap().iter().any(|e| matches!(e, Event::Volume(u16::MAX))));
        p.event(&json!({ "type": "playback_changed", "status": "paused" })).await;
        let at = p.position_ms();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(p.position_ms(), at, "paused stands still");
        p.command(Cmd::Volume(u16::MAX / 2)).await;
        p.command(Cmd::Toggle).await;
        p.command(Cmd::Repeat(2)).await;
        p.command(Cmd::Seek(1234)).await;
        assert_eq!(settled(&sent).await, ["set_volume 50", "play", "set_repeat_track true", "seek 1234"]);
    }

    #[tokio::test]
    async fn a_jam_keeps_its_own_order() {
        let (mut p, sent, events) = player().await;
        let first = p.sequence.start(["a", "b", "c"].map(String::from).to_vec(), "a");
        p.play(first).await;
        p.event(&json!({ "type": "track_changed", "item": song("a") })).await;
        settled(&sent).await;
        let jam = Jam { names: vec!["A".into(), "B".into()], link: String::new() };
        p.set_jam(Some(jam.clone()));
        p.set_jam(Some(jam));
        // A song someone else added is neither skipped nor followed by one of ours.
        assert!(p.event(&json!({ "type": "track_changed", "item": song("z") })).await);
        assert!(p.event(&json!({ "type": "track_changed", "item": song("b") })).await);
        assert_eq!(settled(&sent).await, Vec::<String>::new());
        p.jam_command(Cmd::Enqueue("q".into())).await;
        p.jam_command(Cmd::Play { context: "list".into(), track: "t".into() }).await;
        p.jam_command(Cmd::Prev).await;
        p.jam_command(Cmd::ClearQueue).await;
        assert_eq!(settled(&sent).await, ["add_to_queue \"q\"", "activate", "play \"t\"", "skip_prev"]);
        p.set_jam(None);
        let jams = events.lock().unwrap().iter().filter(|e| matches!(e, Event::Jam(_))).count();
        assert_eq!(jams, 2, "told once when it starts and once when it ends");
    }

    #[tokio::test]
    async fn a_new_output_goes_on_with_the_same_song() {
        let (mut p, sent, _) = player().await;
        let first = p.sequence.start(["a", "b"].map(String::from).to_vec(), "a");
        p.play(first).await;
        p.event(&json!({ "type": "playback_state", "status": "playing", "item": song("a"), "position": { "position_ms": 61000, "timestamp_ms": now_ms() } })).await;
        settled(&sent).await;
        p.remember();
        p.event(&json!({ "type": "auth_state", "logged_in": false })).await;
        assert_eq!(settled(&sent).await, Vec::<String>::new(), "not before it is signed in");
        p.event(&json!({ "type": "auth_state", "logged_in": true })).await;
        assert_eq!(settled(&sent).await, ["activate", "play \"a\"", "add_to_queue \"b\""]);
        p.event(&json!({ "type": "playback_changed", "status": "buffering" })).await;
        assert_eq!(settled(&sent).await, Vec::<String>::new(), "still loading");
        p.event(&json!({ "type": "playback_changed", "status": "playing" })).await;
        let commands = settled(&sent).await;
        let at: u32 = commands[0].strip_prefix("seek ").unwrap().parse().unwrap();
        assert!((61000..61600).contains(&at) && commands.len() == 1, "{commands:?}");
        p.event(&json!({ "type": "auth_state", "logged_in": true })).await;
        p.event(&json!({ "type": "playback_changed", "status": "playing" })).await;
        assert_eq!(settled(&sent).await, Vec::<String>::new(), "only once");
        p.event(&json!({ "type": "playback_changed", "status": "paused" })).await;
        p.remember();
        p.resume().await;
        p.event(&json!({ "type": "playback_changed", "status": "playing" })).await;
        assert_eq!(settled(&sent).await.last().unwrap(), "pause", "what was paused stays paused");
    }

    #[tokio::test]
    async fn the_queue_comes_from_soloist_when_the_order_is_not_ours() {
        let (mut p, _, _) = player().await;
        let queue = json!({ "type": "queue_changed", "upcoming": [{ "item": { "uri": "spotify:track:1" } }, { "item": { "uri": "spotify:episode:2" } }] });
        assert!(p.queue_changed(&queue) && !p.queue_changed(&queue), "only a change is a change");
        assert_eq!(p.outside, ["spotify:track:1"]);
        p.sequence.start(vec!["a".into()], "a");
        assert!(!p.queue_changed(&json!({ "upcoming": [] })), "our own order is shown instead");
    }

    #[tokio::test]
    async fn pairing_is_announced_once() {
        let (mut p, _, events) = player().await;
        for logged_in in [false, false, true] {
            p.event(&json!({ "type": "auth_state", "logged_in": logged_in })).await;
        }
        assert_eq!(events.lock().unwrap().iter().filter(|e| matches!(e, Event::Player(_))).count(), 2);
    }

    #[tokio::test]
    async fn a_soloist_that_ends_at_once_is_reported() {
        let dir = std::env::temp_dir().join(format!("quaver-launch-{}", std::process::id()));
        // Any program that doesn't know these options ends with an error.
        let exe = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("where.exe");
        let failed = launch(&Setup { exe, key: "k".into() }, &dir).await.err().expect("must fail");
        assert!(failed.to_string().contains("API key"), "{failed}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tools_report_why_they_failed() {
        assert!(run_tool("tar.exe", &["-xf"], &["no such file.zip".as_ref()]).unwrap_err().len() > 5);
        assert!(run_tool("no-such-tool.exe", &[], &[]).is_err());
        assert_eq!(run_tool("tar.exe", &["--version"], &[]), Ok(()));
    }

    #[test]
    fn soloist_is_found_where_the_zip_puts_it() {
        let dir = std::env::temp_dir().join(format!("quaver-test-{}", std::process::id()));
        let inner = Setup::dir(&dir).join("soloist_release");
        std::fs::create_dir_all(&inner).unwrap();
        assert!(Setup::load(&dir).is_err());
        std::fs::write(inner.join("soloist.exe"), "").unwrap();
        assert!(Setup::load(&dir).is_err(), "no key yet");
        std::fs::write(Setup::key_file(&dir), " key\r\n").unwrap();
        assert_eq!(Setup::load(&dir).map(|s| s.key), Ok("key".into()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
