//! Plays through Spotify Soloist, Spotify's own headless player, over its local WebSocket API
//! (developer.spotify.com/documentation/soloist/reference/websocket-api).

use std::{
    ffi::c_void,
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
    announce::{self, Announcer},
    backend::{Cmd, Emit, End, Event, Jam, Library, SEARCH},
    connect::Connect,
    settings::{BANDS, Settings},
};

const QUEUE_SHOWN: usize = 80;
/// Without being asked, Soloist tells only so much of the queue.
const QUEUE_TOLD: usize = 10;
/// How often to look whether Windows plays somewhere else now.
pub const OUTPUT_CHECK: Duration = Duration::from_secs(2);
/// How often to look for a Jam.
pub const JAM_CHECK: Duration = Duration::from_secs(15);
/// A paired Soloist says it isn't signed in, too, until it has reached Spotify.
const SIGN_IN: Duration = Duration::from_secs(5);
/// Pressing "previous" later than this into a song restarts it.
pub const RESTART_AFTER_MS: u32 = 3000;

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
    fetch(DOWNLOAD, &zip)?;
    let unpacked = run_tool("tar.exe", &["-xf"], &[zip.as_os_str(), "-C".as_ref(), fresh.as_os_str()]);
    let _ = std::fs::remove_file(&zip);
    unpacked?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("The old build could not be replaced: {e}"))?;
    }
    std::fs::rename(&fresh, &dir).map_err(|e| e.to_string())?;
    Setup::find_exe(data_dir).map(|_| ()).ok_or_else(|| "There was no soloist.exe in the download".into())
}

/// The programs used here are part of Windows; curl checks certificates and only follows https.
pub fn fetch(url: &str, to: &Path) -> Result<(), String> {
    run_tool("curl.exe", &["--fail", "--silent", "--show-error", "--location", "--proto", "=https", "--proto-redir", "=https", "--max-time", "900", "--output"], &[to.as_os_str(), url.as_ref()])
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
pub fn output() -> Option<String> {
    cpal::default_host().default_output_device()?.name().ok()
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(security: *mut c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(job: *mut c_void, class: i32, info: *const JobLimits, size: u32) -> i32;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
}

/// JOBOBJECT_EXTENDED_LIMIT_INFORMATION, of which only the flags are used.
#[repr(C, align(8))]
struct JobLimits {
    times: [u64; 2],
    flags: u32,
    rest: [u32; 31],
}

/// Puts Soloist in a job that Windows ends together with this program, however that ends.
fn end_with_us(child: &Child) {
    const EXTENDED_LIMITS: i32 = 9;
    const KILL_ON_JOB_CLOSE: u32 = 0x2000;
    // Never closed on purpose: Windows does that when this program is gone.
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        let limits = JobLimits { times: [0; 2], flags: KILL_ON_JOB_CLOSE, rest: [0; 31] };
        let set = SetInformationJobObject(job, EXTENDED_LIMITS, &limits, size_of::<JobLimits>() as u32);
        if job.is_null() || set == 0 { 0 } else { job as usize }
    });
    let assigned = child.raw_handle().is_some_and(|process| job != 0 && unsafe { AssignProcessToJobObject(job as *mut c_void, process) } != 0);
    if !assigned {
        log::warn!("soloist may outlive rustbeat: {}", std::io::Error::last_os_error());
    }
}

/// The port on which a Soloist that isn't signed in waits for the Spotify app.
fn pairing_port(pid: u32) -> Option<u16> {
    use std::os::windows::process::CommandExt;
    let system = PathBuf::from(std::env::var_os("SystemRoot")?).join("System32");
    let output = std::process::Command::new(system.join("netstat.exe")).args(["-ano", "-p", "TCP"]).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    port_of(&String::from_utf8_lossy(&output.stdout), pid)
}

/// The words in netstat's list are translated, its numbers aren't.
fn port_of(netstat: &str, pid: u32) -> Option<u16> {
    netstat.lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        let [_, local, remote, .., owner] = fields[..] else { return None };
        (remote == "0.0.0.0:0" && owner.parse() == Ok(pid)).then(|| local.strip_prefix("0.0.0.0:")?.parse().ok())?
    })
}

/// Makes Soloist ask for lossless, and for even volume only if that is wanted. Nothing documented
/// does that, but these are lines of its own settings, which it reads when it starts.
// shortcut: an account has no such file before its first sign-in, so that first time plays
// untuned; start Soloist again after pairing if that matters.
/// What Rustbeat sets in Soloist's own settings, which it reads when it starts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tuning {
    pub normalise: bool,
    /// Rustbeat's bands; all at 0 when the equalizer is off.
    pub eq: [f32; BANDS],
}

impl Tuning {
    pub fn of(settings: &Settings) -> Self {
        Self { normalise: settings.normalise, eq: if settings.eq_on { settings.eq } else { [0.; BANDS] } }
    }
}

/// The bands of Spotify's own equalizer, and the names of their settings.
const SPOTIFY_BANDS: [(f32, &str); 6] = [
    (60., "low_shelf"),
    (150., "low_peak"),
    (400., "low_mid_peak"),
    (1000., "high_mid_peak"),
    (2400., "high_peak"),
    (15000., "high_shelf"),
];

/// Rustbeat's gain at `hz`, read between its bands, which are an octave apart from 31.25 Hz.
fn gain_at(eq: &[f32; BANDS], hz: f32) -> f32 {
    let place = (hz / 31.25).log2().clamp(0., (BANDS - 1) as f32);
    let (low, part) = (place.floor() as usize, place.fract());
    let high = (low + 1).min(BANDS - 1);
    eq[low] + (eq[high] - eq[low]) * part
}

/// Spotify keeps a gain as a share of its ±12 dB range, in units of 2^-30.
fn spotify_gain(db: f32) -> i64 {
    (f64::from(db.clamp(-12., 12.)) / 12. * f64::from(1u32 << 30)).round() as i64
}

fn tune(state: &Path, tuning: Tuning) -> std::io::Result<()> {
    let Ok(users) = std::fs::read_dir(state.join("settings").join("Users")) else { return Ok(()) };
    for user in users.flatten() {
        let path = user.path().join("prefs");
        let Ok(old) = std::fs::read_to_string(&path) else { continue };
        let tuned = tuned(&old, tuning);
        if tuned != old {
            let new = path.with_extension("new");
            std::fs::write(&new, tuned)?;
            std::fs::rename(new, path)?;
        }
    }
    Ok(())
}

fn tuned(prefs: &str, tuning: Tuning) -> String {
    const LOSSLESS: &str = "5";
    let mut wanted = vec![
        ("audio.play_bitrate_enumeration".to_owned(), LOSSLESS.to_owned()),
        ("audio.play_bitrate_non_metered_enumeration".into(), LOSSLESS.into()),
        ("audio.play_bitrate_non_metered_migrated".into(), "true".into()),
        ("audio.normalize_v2".into(), tuning.normalise.to_string()),
        ("audio.equalizer_v2".into(), tuning.eq.iter().any(|g| *g != 0.).to_string()),
    ];
    wanted.extend(SPOTIFY_BANDS.map(|(hz, name)| (format!("audio.equalizer.{name}_gain_v2"), spotify_gain(gain_at(&tuning.eq, hz)).to_string())));
    let key = |line: &str| line.split('=').next().unwrap_or_default().to_owned();
    let kept: Vec<String> = prefs.lines().filter(|l| !l.is_empty() && !wanted.iter().any(|w| w.0 == key(l))).map(String::from).collect();
    kept.into_iter().chain(wanted.into_iter().map(|(key, value)| format!("{key}={value}"))).map(|l| l + "\n").collect()
}

async fn launch(setup: &Setup, data_dir: &Path, tuning: Tuning) -> Result<(Child, Socket), Error> {
    let state = data_dir.join("soloist-state");
    std::fs::create_dir_all(&state)?;
    // Worse sound is better than none.
    if let Err(e) = tune(&state, tuning) {
        log::warn!("soloist settings: {e}");
    }
    let port_file = state.join("ws.port");
    // Left behind if Soloist was killed; it would point at a port that is gone.
    let _ = std::fs::remove_file(&port_file);
    let mut child = Command::new(&setup.exe)
        .args(["--device-name", "Rustbeat", "--ws", "127.0.0.1:0", "--cache-size", "500", "--api-key", &setup.key])
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
    end_with_us(&child);
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
    tuning: Tuning,
) -> Result<End, Error> {
    let mut playing_on = tokio::task::spawn_blocking(output).await.ok().flatten();
    let (mut child, socket) = {
        let starting = launch(setup, data_dir, tuning);
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
    let mut announcer: Option<Announcer> = None;
    let mut output_check = tokio::time::interval(OUTPUT_CHECK);
    let mut jam_check = tokio::time::interval(JAM_CHECK);
    let (jam_tx, mut jams) = mpsc::unbounded_channel();
    let mut player = Player { socket, emit: emit.clone(), track: String::new(), active: false, says_playing: false, playing: false, position: (0, 0), paired: None, unpaired: None, jam: None, upcoming: None, queue_asked: false, resume: None, catch_up: None, volume: u16::MAX / 2 };
    player.volume = cache.volume().unwrap_or(u16::MAX / 2);
    let connect = Connect::new(library.session.clone(), &data_dir.join("soloist-state"));

    loop {
        tokio::select! {
            _ = session_check.tick() => if library.session.is_invalid() { return Ok(End::Lost) },
            _ = output_check.tick() => {
                player.ask_to_pair(SIGN_IN);
                announcer = if player.paired == Some(true) { None } else { announced(announcer.take(), child.id()).await };
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
                let (new_child, socket) = launch(setup, data_dir, tuning).await?;
                (child, player.socket) = (new_child, socket);
                player.paired = None;
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
                    } else if event["type"] == "queue_changed" && library.queue_shown {
                        match player.queue_changed(&event) {
                            Queue::Same => {}
                            Queue::Cut => player.show_queue(library).await,
                            Queue::New(uris) => library.show_queue(uris),
                        }
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
                } else if let Err(e) = player.shared_command(cmd, &connect, library).await {
                    log::warn!("connect: {e}");
                    emit(Event::Status(format!("Spotify refused that: {}", e.error)));
                }
                player.show_queue(library).await;
            }
        }
    }
}

/// Keeps the announcement up to date: the network may change, and Soloist picks a new port each time.
async fn announced(old: Option<Announcer>, soloist: Option<u32>) -> Option<Announcer> {
    let soloist = soloist?;
    let (address, port) = tokio::task::spawn_blocking(move || Some((announce::local_address()?, pairing_port(soloist)?))).await.ok()??;
    if old.as_ref().is_some_and(|a| (a.address, a.port) == (address, port)) {
        return old;
    }
    // The old one has to let go of the port first.
    drop(old);
    Announcer::start(address, port).inspect_err(|e| log::warn!("announce: {e}")).ok()
}

#[derive(Debug, PartialEq)]
enum Queue {
    Same,
    /// There is more of it, which has to be asked for.
    Cut,
    New(Vec<String>),
}

struct Player {
    socket: Socket,
    emit: Emit,
    track: String,
    /// Whether the sound is ours to make, and not another device's.
    active: bool,
    /// What Soloist last said, which may be about another device.
    says_playing: bool,
    playing: bool,
    /// Position and when it was valid, in ms since 1970.
    position: (u32, u64),
    paired: Option<bool>,
    /// Since when it isn't signed in, until that was said.
    unpaired: Option<Instant>,
    /// While there is a Jam, the order of songs is its business.
    jam: Option<Jam>,
    /// What comes next, as it is shown.
    upcoming: Option<Vec<String>>,
    /// Whether the whole queue was asked for and is still to come.
    queue_asked: bool,
    /// What to go on with after a restart: where in the song, and whether it was playing.
    resume: Option<(u32, bool)>,
    /// Where to move to once the resumed song plays.
    catch_up: Option<u32>,
    /// What the volume should be; Soloist starts with its own.
    volume: u16,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

impl Player {
    /// What plays on another device is left there.
    fn remember(&mut self) {
        self.catch_up = None;
        self.resume = (self.active && !self.track.is_empty()).then(|| (self.position_ms(), self.playing));
        self.active = false;
        self.follow();
    }

    /// Spotify knows the song and how it goes on, but not exactly where in it we were.
    async fn resume(&mut self) {
        let Some((ms, playing)) = self.resume.take() else { return };
        self.send("activate", json!({})).await;
        if playing {
            self.catch_up = Some(ms);
            self.send("play", json!({})).await;
        }
    }

    /// A song that is still loading can't be moved in, so this waits until it plays.
    async fn catch_up(&mut self) {
        if self.playing && let Some(ms) = self.catch_up.take() {
            self.send("seek", json!({ "position_ms": ms })).await;
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

    async fn play(&mut self, uri: &str) {
        self.send("activate", json!({})).await;
        self.send("play", json!({ "uri": uri })).await;
    }

    /// What only works, or works better, the way Spotify's own apps do it.
    async fn shared_command(&mut self, cmd: Cmd, connect: &Connect, library: &Library) -> Result<(), Error> {
        match cmd {
            Cmd::Play { context, track } if context.is_empty() => {
                if !self.active {
                    connect.take_over().await?;
                }
                connect.jump(&track).await
            }
            Cmd::Play { context, track } => {
                let played = if context.starts_with(SEARCH) {
                    // Spotify's apps don't know a search as a list.
                    let songs = library.all_tracks(&context).await.unwrap_or_else(|_| vec![track.clone()]);
                    connect.play_songs(&songs, &track).await
                } else {
                    connect.play(&context, &track).await
                };
                if let Err(e) = played {
                    log::warn!("connect play: {e}");
                    // The documented way, which knows no place in a list.
                    self.play(if track.is_empty() { &context } else { &track }).await;
                }
                Ok(())
            }
            Cmd::ClearQueue => connect.clear_queue().await,
            Cmd::EditQueue { uri, nth, to } => connect.edit_queue((&uri, nth), to.as_ref().map(|(u, n)| (u.as_str(), *n))).await,
            Cmd::JoinJam(uri) => {
                tokio::spawn(crate::backend::join_jam(library.session.clone(), connect.clone(), uri, self.emit.clone()));
                Ok(())
            }
            Cmd::Toggle | Cmd::Resume if !self.active => {
                if let Err(e) = connect.take_over().await {
                    log::warn!("connect take over: {e}");
                    self.send("activate", json!({})).await;
                    self.send("play", json!({})).await;
                }
                Ok(())
            }
            cmd => {
                self.command(cmd).await;
                Ok(())
            }
        }
    }

    async fn set_volume(&mut self, volume: u16) {
        self.volume = volume;
        (self.emit)(Event::Volume(volume));
        self.send("set_volume", json!({ "volume": (volume as u32 * 100 + 32767) / 65535 })).await;
    }

    fn position_ms(&self) -> u32 {
        let run = if self.playing { now_ms().saturating_sub(self.position.1) as u32 } else { 0 };
        self.position.0.saturating_add(run)
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Toggle => self.send(if self.playing { "pause" } else { "play" }, json!({})).await,
            Cmd::Resume => self.send("play", json!({})).await,
            Cmd::Pause => self.send("pause", json!({})).await,
            Cmd::Next => self.send("skip_next", json!({})).await,
            Cmd::Prev => self.send("skip_prev", json!({})).await,
            Cmd::Seek(ms) => self.send("seek", json!({ "position_ms": ms })).await,
            Cmd::Volume(v) => self.set_volume(v).await,
            Cmd::Enqueue(uri) => self.send("add_to_queue", json!({ "uri": uri })).await,
            // Opened again, so shown even if it is what was shown before.
            Cmd::Queue => self.upcoming = None,
            // The answer to these two is an `options_changed`.
            Cmd::Shuffle(on) => self.send("set_shuffle", json!({ "enabled": on })).await,
            Cmd::Repeat(mode) => {
                // Turning one off first keeps them from being on together.
                let modes = [("set_repeat_context", mode == 1), ("set_repeat_track", mode == 2)];
                for (command, enabled) in if mode == 1 { [modes[1], modes[0]] } else { modes } {
                    self.send(command, json!({ "enabled": enabled })).await;
                }
            }
            _ => {}
        }
    }

    /// Returns whether the song changed.
    async fn event(&mut self, event: &Value) -> bool {
        let mut changed = false;
        if let Some(active) = event["is_active"].as_bool() {
            self.active = active;
            self.follow();
        }
        self.options(&event["options"]);
        match event["type"].as_str().unwrap_or_default() {
            "auth_state" => {
                let paired = event["logged_in"].as_bool().unwrap_or(false);
                let news = self.paired.replace(paired) != Some(paired);
                if paired && news {
                    // Before it is signed in, Soloist refuses this.
                    self.set_volume(self.volume).await;
                }
                if paired {
                    self.resume().await;
                }
                if news {
                    self.unpaired = (!paired).then(Instant::now);
                    if paired {
                        (self.emit)(Event::Paired(true));
                        (self.emit)(Event::Player("Connected. Soloist plays the sound.".into()));
                    }
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

    fn ask_to_pair(&mut self, patience: Duration) {
        if self.unpaired.take_if(|since| since.elapsed() >= patience).is_some() {
            (self.emit)(Event::Paired(false));
            (self.emit)(Event::Player("Almost there: open Spotify on your phone or computer, on this network, and pick “Rustbeat” as the device. Once is enough.".into()));
        }
    }

    async fn item(&mut self, item: &Value) -> bool {
        let Some(uri) = item["uri"].as_str().filter(|u| !u.is_empty()) else { return false };
        if uri == self.track {
            return false;
        }
        self.track = uri.to_owned();
        self.position = (0, now_ms());
        self.catch_up = None;
        let facts = &item["decorations"];
        let name = |entity: &Value| entity["decorations"]["identity"]["name"].as_str().unwrap_or_default().to_owned();
        let artists: Vec<_> = facts["creators"].as_array().into_iter().flatten().map(|c| name(&c["entity"])).collect();
        (self.emit)(Event::Track {
            uri: uri.to_owned(),
            title: name(item),
            artist: artists.join(", "),
            artist_uri: facts["creators"][0]["entity"]["uri"].as_str().unwrap_or_default().to_owned(),
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
        self.says_playing = status == "playing";
        self.follow();
    }

    fn follow(&mut self) {
        let position = self.position_ms();
        let playing = self.active && self.says_playing;
        if playing != self.playing {
            self.position = (position, now_ms());
            self.playing = playing;
        }
        self.show_position();
    }

    fn options(&self, options: &Value) {
        if let Some(on) = options["shuffle"].as_bool() {
            (self.emit)(Event::Shuffle(on));
        }
        if let Some(mode) = ["off", "context", "track"].iter().position(|m| options["repeat"] == *m) {
            (self.emit)(Event::Repeat(mode as u8));
        }
    }

    fn show_position(&self) {
        (self.emit)(Event::Position { ms: self.position_ms(), playing: self.playing });
    }

    async fn show_queue(&mut self, library: &Library) {
        if library.queue_shown {
            // The answer is a `queue_changed`.
            self.queue_asked = true;
            self.send("get_queue", json!({ "limit": QUEUE_SHOWN })).await;
        }
    }

    fn queue_changed(&mut self, event: &Value) -> Queue {
        let entries = event["upcoming"].as_array().map_or(&[][..], Vec::as_slice);
        let uris: Vec<_> = entries
            .iter()
            .filter_map(|e| e["item"]["uri"].as_str())
            .filter(|uri| uri.starts_with("spotify:track:"))
            .map(String::from)
            .collect();
        let cut = !std::mem::take(&mut self.queue_asked) && entries.len() == QUEUE_TOLD;
        if self.upcoming.as_ref().is_some_and(|shown| *shown == uris || cut && shown.starts_with(&uris)) {
            return Queue::Same;
        }
        if cut {
            return Queue::Cut;
        }
        self.upcoming = Some(uris.clone());
        Queue::New(uris)
    }

    fn set_jam(&mut self, jam: Option<Jam>) {
        if self.jam == jam {
            return;
        }
        (self.emit)(Event::Jam(jam.clone()));
        self.jam = jam;
    }

    /// In a Jam nothing is arranged here: everything is passed on as it is.
    async fn jam_command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Play { context, track } => self.play(if track.is_empty() { &context } else { &track }).await,
            Cmd::ClearQueue => (self.emit)(Event::Status("The queue belongs to the Jam and can't be cleared here.".into())),
            Cmd::EditQueue { .. } => (self.emit)(Event::Status("The queue belongs to the Jam and can't be changed here.".into())),
            Cmd::JoinJam(_) => (self.emit)(Event::Status("You're in a Jam already; leave it in the Spotify app first.".into())),
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
        let player = Player { socket, emit, track: String::new(), active: true, says_playing: false, playing: false, position: (0, 0), paired: None, unpaired: None, jam: None, upcoming: None, queue_asked: false, resume: None, catch_up: None, volume: u16::MAX / 2 };
        (player, sent, events)
    }

    async fn settled(sent: &Mutex<Vec<String>>) -> Vec<String> {
        tokio::time::sleep(Duration::from_millis(60)).await;
        std::mem::take(&mut *sent.lock().unwrap())
    }

    #[tokio::test]
    async fn a_song_is_shown_once() {
        let (mut p, sent, events) = player().await;
        assert!(p.event(&json!({ "type": "track_changed", "item": song("a") })).await);
        assert!(!p.event(&json!({ "type": "track_changed", "item": song("a") })).await, "the same song again is no change");
        let events = events.lock().unwrap();
        let Event::Track { title, artist, album, duration_ms, cover_url, .. } = &events[0] else { panic!() };
        assert_eq!((title.as_str(), artist.as_str(), album.as_str(), *duration_ms), ("Name a", "A, B", "Album", 210000));
        assert_eq!(cover_url.as_deref(), Some("x"), "the biggest cover");
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn another_device_is_left_alone() {
        let (mut p, sent, events) = player().await;
        p.event(&json!({ "type": "playback_state", "status": "playing", "item": song("a"), "is_active": true })).await;
        assert!(p.playing);
        p.event(&json!({ "type": "device_changed", "is_active": false, "device_name": "Phone" })).await;
        for uri in ["b", "c", "b", "a"] {
            p.event(&json!({ "type": "track_changed", "item": song(uri) })).await;
            p.event(&json!({ "type": "playback_changed", "status": "playing" })).await;
            p.event(&json!({ "type": "options_changed", "options": { "shuffle": true, "repeat": "context" } })).await;
            p.event(&json!({ "type": "queue_changed", "upcoming": [{ "source": "context", "item": { "uri": "spotify:track:1" } }] })).await;
            assert!(!p.playing, "the sound is elsewhere");
        }
        p.remember();
        p.event(&json!({ "type": "auth_state", "logged_in": true, "is_active": false })).await;
        assert_eq!(settled(&sent).await, ["set_volume 50"], "nothing is queued, skipped or started, also not after a restart");
        let events = events.lock().unwrap();
        assert!(events.iter().any(|e| matches!(e, Event::Repeat(1))) && events.iter().any(|e| matches!(e, Event::Shuffle(true))));
        drop(events);
        p.event(&json!({ "type": "playback_state", "status": "playing", "item": song("a"), "is_active": true })).await;
        assert!(p.playing, "and back again");
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
        p.command(Cmd::Repeat(1)).await;
        p.command(Cmd::Seek(1234)).await;
        assert_eq!(settled(&sent).await, ["set_volume 50", "play", "set_repeat_context false", "set_repeat_track true", "set_repeat_track false", "set_repeat_context true", "seek 1234"]);
    }

    #[tokio::test]
    async fn a_jam_keeps_its_own_order() {
        let (mut p, sent, events) = player().await;
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
        p.event(&json!({ "type": "playback_state", "status": "playing", "item": song("a"), "position": { "position_ms": 61000, "timestamp_ms": now_ms() } })).await;
        p.remember();
        p.event(&json!({ "type": "auth_state", "logged_in": false, "is_active": false })).await;
        assert_eq!(settled(&sent).await, Vec::<String>::new(), "not before it is signed in");
        p.event(&json!({ "type": "auth_state", "logged_in": true, "is_active": false })).await;
        assert_eq!(settled(&sent).await, ["set_volume 50", "activate", "play"]);
        p.event(&json!({ "type": "device_changed", "is_active": true })).await;
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
        assert_eq!(settled(&sent).await, ["activate"], "what was paused stays paused");
    }

    #[tokio::test]
    async fn the_volume_is_set_once_signed_in() {
        let (mut p, sent, _) = player().await;
        p.volume = 39068;
        p.event(&json!({ "type": "auth_state", "logged_in": false })).await;
        assert_eq!(settled(&sent).await, Vec::<String>::new());
        for _ in 0..2 {
            p.event(&json!({ "type": "auth_state", "logged_in": true })).await;
        }
        assert_eq!(settled(&sent).await, ["set_volume 60"]);
    }

    #[tokio::test]
    async fn the_whole_queue_is_shown() {
        let (mut p, _, _) = player().await;
        let uris = |n: usize| (0..n).map(|i| format!("spotify:track:{i}")).collect::<Vec<_>>();
        let queue = |uris: Vec<String>| json!({ "type": "queue_changed", "upcoming": uris.iter().map(|u| json!({ "item": { "uri": u } })).collect::<Vec<_>>() });
        for n in [0, 10, 30] {
            (p.upcoming, p.queue_asked) = (None, true);
            assert_eq!(p.queue_changed(&queue(uris(n))), Queue::New(uris(n)), "an answer is everything");
        }
        assert_eq!(p.queue_changed(&queue(uris(10))), Queue::Same, "the start of what is shown");
        assert_eq!(p.queue_changed(&queue(uris(11)[1..].to_vec())), Queue::Cut);
        assert_eq!(p.queue_changed(&queue(uris(2))), Queue::New(uris(2)));
        let mixed = queue(vec!["spotify:track:1".into(), "spotify:episode:2".into()]);
        assert_eq!(p.queue_changed(&mixed), Queue::New(vec!["spotify:track:1".into()]));
    }

    #[tokio::test]
    async fn pairing_is_announced_once() {
        let (mut p, _, events) = player().await;
        let said = || events.lock().unwrap().iter().filter(|e| matches!(e, Event::Player(_))).count();
        for _ in 0..2 {
            p.event(&json!({ "type": "auth_state", "logged_in": false })).await;
        }
        p.ask_to_pair(SIGN_IN);
        assert_eq!(said(), 0, "it may still be signing in");
        for _ in 0..2 {
            p.ask_to_pair(Duration::ZERO);
        }
        assert_eq!(said(), 1);
        p.event(&json!({ "type": "auth_state", "logged_in": true })).await;
        p.ask_to_pair(Duration::ZERO);
        assert_eq!(said(), 2, "connected, and nothing left to ask");
    }

    #[tokio::test]
    async fn a_soloist_that_ends_at_once_is_reported() {
        let dir = std::env::temp_dir().join(format!("rustbeat-launch-{}", std::process::id()));
        // Any program that doesn't know these options ends with an error.
        let exe = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("where.exe");
        let failed = launch(&Setup { exe, key: "k".into() }, &dir, Tuning::default()).await.err().expect("must fail");
        assert!(failed.to_string().contains("API key"), "{failed}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_equalizer_is_told_the_way_spotify_keeps_it() {
        // Values the Spotify app wrote for +8.5, +7 and +2.5 dB.
        assert_eq!([8.5, 7., 2.5].map(spotify_gain), [760567125, 626349397, 223696213]);
        assert_eq!(spotify_gain(-99.), -(1 << 30));
        let bass = [6., 5., 4., 2., 0., 0., 0., 0., 0., 0.];
        assert_eq!(gain_at(&bass, 62.5), 5.);
        assert!((gain_at(&bass, 60.) - 5.06).abs() < 0.01, "between the bands");
        assert_eq!(gain_at(&bass, 15000.), 0.);
        assert_eq!(gain_at(&[1.; BANDS], 20.), 1., "below the lowest band");
        let prefs = tuned("", Tuning { normalise: false, eq: bass });
        assert!(prefs.contains("audio.equalizer_v2=true\n") && prefs.contains("audio.equalizer.low_shelf_gain_v2="), "{prefs}");
    }

    #[test]
    fn its_settings_are_tuned_and_the_rest_kept() {
        let dir = std::env::temp_dir().join(format!("rustbeat-tune-{}", std::process::id()));
        tune(&dir, Tuning::default()).unwrap();
        let user = dir.join("settings").join("Users").join("someone-user");
        std::fs::create_dir_all(user.with_file_name("new-user")).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let prefs = user.join("prefs");
        std::fs::write(&prefs, "audio.gapless_migrated_to_ps=true\r\naudio.play_bitrate_enumeration=2\r\n").unwrap();
        for normalise in [true, false] {
            let tuning = Tuning { normalise, eq: [0.; BANDS] };
            tune(&dir, tuning).unwrap();
            let now = std::fs::read_to_string(&prefs).unwrap();
            assert!(now.starts_with("audio.gapless_migrated_to_ps=true\naudio.play_bitrate_enumeration=5\n"), "{now}");
            assert!(now.contains(&format!("audio.normalize_v2={normalise}\naudio.equalizer_v2=false\n")), "{now}");
            assert_eq!(tuned(&now, tuning), now);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pairing_port_is_found_in_any_language() {
        let list = "
Connessioni attive

  Proto  Indirizzo locale  Indirizzo esterno  Stato  PID
  TCP    127.0.0.1:56734  0.0.0.0:0  LISTENING  12
  TCP    0.0.0.0:80  0.0.0.0:0  LISTENING  4
  TCP    10.0.0.1:5000  1.2.3.4:443  ESTABLISHED  12
  TCP    0.0.0.0:65028  0.0.0.0:0  IN ASCOLTO  12
";
        assert_eq!(port_of(list, 12), Some(65028));
        assert_eq!(port_of(list, 13), None);
        assert_eq!(port_of("", 12), None);
    }

    #[tokio::test]
    async fn a_process_can_be_tied_to_this_one() {
        // The layout has to be right, or Windows refuses it.
        assert_eq!(size_of::<JobLimits>(), 144);
        let mut child = Command::new("ping.exe").args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).creation_flags(CREATE_NO_WINDOW).kill_on_drop(true).spawn().unwrap();
        end_with_us(&child);
        let in_job = {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn IsProcessInJob(process: *mut c_void, job: *mut c_void, result: *mut i32) -> i32;
            }
            let mut result = 0;
            unsafe { IsProcessInJob(child.raw_handle().unwrap(), std::ptr::null_mut(), &mut result) };
            result
        };
        child.kill().await.unwrap();
        assert_eq!(in_job, 1);
    }

    #[test]
    fn tools_report_why_they_failed() {
        assert!(run_tool("tar.exe", &["-xf"], &["no such file.zip".as_ref()]).unwrap_err().len() > 5);
        assert!(run_tool("no-such-tool.exe", &[], &[]).is_err());
        assert_eq!(run_tool("tar.exe", &["--version"], &[]), Ok(()));
    }

    #[test]
    fn soloist_is_found_where_the_zip_puts_it() {
        let dir = std::env::temp_dir().join(format!("rustbeat-test-{}", std::process::id()));
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
