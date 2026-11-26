//! Plays the audio files in a folder, without Spotify or a network.

use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use librespot_core::cache::Cache;
use librespot_playback::{config::VolumeCtrl, mixer::mappings::MappedCtrl};
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use symphonia::core::{
    io::MediaSourceStream,
    meta::{MetadataRevision, StandardTagKey},
    probe::{Hint, ProbeResult},
};
use tokio::sync::{Semaphore, mpsc};

use crate::{
    backend::{Card, Cmd, Emit, End, Event, Row, Shelf},
    eq::Equalized,
    sequence::Sequence,
    settings::Shared,
    soloist::{OUTPUT_CHECK, RESTART_AFTER_MS, output},
};

const EXTENSIONS: [&str; 4] = ["flac", "mp3", "oga", "ogg"];
const ALL: &str = "local:all";
const ALBUM: &str = "local:album:";
const SEARCH: &str = "local:search:";
/// How soon the end of a song is noticed.
// shortcut: that is also the gap between two songs, hand the next one to rodio early if it bothers.
const END_CHECK: Duration = Duration::from_millis(30);
/// Albums on the home page; the library on the left has all of them.
const SHELF: usize = 40;
/// Files that can't be played in a row before giving up.
const FAILURES: usize = 5;
const PICTURES: usize = 4;

#[derive(Debug, Clone, Default, PartialEq)]
struct Song {
    path: String,
    title: String,
    artist: String,
    album: String,
    /// The context that plays its album: the folder and the album's name, as two albums may share one.
    album_uri: String,
    disc: u32,
    number: u32,
    duration_ms: u32,
}

impl Song {
    fn row(&self) -> Row {
        Row {
            uri: self.path.clone(),
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            album_uri: self.album_uri.clone(),
            duration_ms: self.duration_ms,
            cover: self.path.clone(),
            ..Default::default()
        }
    }
}

/// Every song in a folder and the folders in it, album by album.
fn scan(dir: &Path) -> Vec<Song> {
    let mut songs = Vec::new();
    let mut folders = vec![dir.to_owned()];
    while let Some(folder) = folders.pop() {
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(e) => {
                log::warn!("{}: {e}", folder.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            // A link doesn't count as a folder, so links can't lead in circles.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                folders.push(entry.path());
            } else if let Some(song) = read(&entry.path()) {
                songs.push(song);
            }
        }
    }
    songs.sort_by_cached_key(|s| (s.artist.to_lowercase(), s.album_uri.to_lowercase(), s.disc, s.number, s.title.to_lowercase()));
    songs
}

fn probe(path: &Path) -> Option<ProbeResult> {
    let extension = path.extension()?.to_str()?.to_lowercase();
    if !EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }
    let source = MediaSourceStream::new(Box::new(File::open(path).ok()?), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(&extension);
    symphonia::default::get_probe()
        .format(&hint, source, &Default::default(), &Default::default())
        .inspect_err(|e| log::warn!("{}: {e}", path.display()))
        .ok()
}

/// The tags and pictures, which are in the file's container or in front of it.
fn revision(probed: &mut ProbeResult) -> MetadataRevision {
    let inside = probed.format.metadata().skip_to_latest().cloned();
    inside.or_else(|| probed.metadata.get()?.skip_to_latest().cloned()).unwrap_or_default()
}

fn read(path: &Path) -> Option<Song> {
    let mut probed = probe(path)?;
    let tags = revision(&mut probed);
    let tag = |key| tags.tags().iter().find(|t| t.std_key == Some(key)).map(|t| t.value.to_string().trim().to_owned()).filter(|t| !t.is_empty());
    // "3/12" is the third of twelve.
    let number = |key| tag(key).and_then(|t| t.split('/').next()?.trim().parse().ok()).unwrap_or(0);
    let params = &probed.format.default_track()?.codec_params;
    let duration = params.n_frames.zip(params.time_base).map(|(n, base)| base.calc_time(n));
    let folder = path.parent()?;
    let album = tag(StandardTagKey::Album).or_else(|| Some(folder.file_name()?.to_str()?.to_owned())).unwrap_or_default();
    Some(Song {
        path: path.to_str()?.to_owned(),
        title: tag(StandardTagKey::TrackTitle).or_else(|| Some(path.file_stem()?.to_str()?.to_owned()))?,
        artist: tag(StandardTagKey::Artist).or_else(|| tag(StandardTagKey::AlbumArtist)).unwrap_or_default(),
        // No folder's name has this character in it.
        album_uri: format!("{ALBUM}{}|{album}", folder.to_str()?),
        album,
        disc: number(StandardTagKey::DiscNumber),
        number: number(StandardTagKey::TrackNumber),
        duration_ms: duration.map_or(0, |t| (t.seconds * 1000 + (t.frac * 1000.) as u64) as u32),
    })
}

/// The picture in the file, or the one lying next to it.
fn cover(path: &Path) -> Option<Vec<u8>> {
    let inside = probe(path).and_then(|mut probed| Some(revision(&mut probed).visuals().first()?.data.to_vec()));
    inside.or_else(|| {
        ["cover.jpg", "folder.jpg", "cover.png"].iter().find_map(|name| std::fs::read(path.with_file_name(name)).ok())
    })
}

fn songs_of<'a>(songs: &'a [Song], context: &str) -> Vec<&'a Song> {
    if let Some(query) = context.strip_prefix(SEARCH) {
        let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
        let found = |s: &&Song| {
            let text = format!("{} {} {}", s.title, s.artist, s.album).to_lowercase();
            words.iter().all(|w| text.contains(w))
        };
        return songs.iter().filter(found).collect();
    }
    songs.iter().filter(|s| context == ALL || context == s.album_uri).collect()
}

/// One card for each album, in the order of the songs.
fn albums(songs: &[Song]) -> Vec<Card> {
    let mut seen = std::collections::HashSet::new();
    let first = songs.iter().filter(|s| seen.insert(&s.album_uri));
    first.map(|s| Card { title: s.album.clone(), subtitle: s.artist.clone(), uri: s.album_uri.clone(), picture: s.path.clone() }).collect()
}

pub async fn run(rx: &mut mpsc::UnboundedReceiver<Cmd>, emit: &Emit, cache: &Cache, shared: &Arc<Shared>) -> End {
    emit(Event::Connecting("Reading your music folder…".into()));
    let dir = PathBuf::from(&shared.lock().music);
    let scan = |dir: PathBuf| tokio::task::spawn_blocking(move || scan(&dir));
    let mut scanning = scan(dir.clone());
    let songs = loop {
        tokio::select! {
            songs = &mut scanning => break songs.unwrap_or_default(),
            cmd = rx.recv() => match cmd {
                Some(Cmd::Quit) | None => return End::Quit,
                Some(Cmd::Logout) => return End::Logout,
                Some(Cmd::Restart) => return End::Restart,
                _ => {}
            },
        }
    };
    let mut player = Player {
        songs,
        emit: emit.clone(),
        shared: shared.clone(),
        sequence: Sequence::default(),
        sink: None,
        stream: None,
        now: String::new(),
        from: Duration::ZERO,
        repeat_song: false,
        volume: cache.volume().unwrap_or(u16::MAX / 2),
        queue_shown: false,
    };
    let pictures = Arc::new(Semaphore::new(PICTURES));
    emit(Event::Player(String::new()));
    emit(Event::Jam(None));
    emit(Event::Volume(player.volume));
    emit(Event::Shuffle(false));
    emit(Event::Repeat(0));
    player.show_start(&dir);
    emit(Event::Ready);

    let mut playing_on = tokio::task::spawn_blocking(output).await.ok().flatten();
    let mut end_check = tokio::time::interval(END_CHECK);
    let mut output_check = tokio::time::interval(OUTPUT_CHECK);
    loop {
        tokio::select! {
            _ = end_check.tick() => player.follow_up(),
            _ = output_check.tick() => {
                let now = tokio::task::spawn_blocking(output).await.ok().flatten();
                if now.is_some() && now != playing_on {
                    playing_on = now;
                    player.reopen();
                }
            }
            cmd = rx.recv() => match cmd {
                Some(Cmd::Quit) | None => return End::Quit,
                Some(Cmd::Logout) => return End::Logout,
                Some(Cmd::Restart) => return End::Restart,
                Some(Cmd::Home { again: true }) => {
                    player.songs = scan(dir.clone()).await.unwrap_or_default();
                    player.show_start(&dir);
                }
                Some(Cmd::Picture { kind, id }) => {
                    let (emit, pictures) = (emit.clone(), pictures.clone());
                    tokio::spawn(async move {
                        let _turn = pictures.acquire().await;
                        let path = PathBuf::from(&id);
                        let image = tokio::task::spawn_blocking(move || cover(&path)).await.ok().flatten();
                        emit(Event::Picture { kind, id, images: image.into_iter().collect() });
                    });
                }
                Some(Cmd::Volume(v)) => {
                    cache.save_volume(v);
                    player.set_volume(v);
                }
                Some(cmd) => player.command(cmd),
            },
        }
    }
}

struct Player {
    songs: Vec<Song>,
    emit: Emit,
    shared: Arc<Shared>,
    sequence: Sequence,
    sink: Option<Sink>,
    /// Kept from song to song while they have the same sample rate.
    stream: Option<(u32, OutputStream)>,
    /// The song that is loaded, and where in it the sink started.
    now: String,
    from: Duration,
    repeat_song: bool,
    volume: u16,
    queue_shown: bool,
}

impl Player {
    fn show_start(&self, dir: &Path) {
        let albums = albums(&self.songs);
        if albums.is_empty() {
            (self.emit)(Event::Playlists(vec![]));
            return (self.emit)(Event::NoHome(format!("No MP3, FLAC or Ogg Vorbis files in {}. Choose another folder in Settings.", dir.display())));
        }
        let shelf = |title: &str, count| Shelf { title: title.into(), cards: albums.iter().take(count).cloned().collect() };
        (self.emit)(Event::Home(vec![shelf("", 8), shelf("Albums", SHELF)]));
        (self.emit)(Event::Playlists(albums));
    }

    fn show(&mut self, title: String, context: &str) {
        self.queue_shown = false;
        (self.emit)(Event::List { title, context: context.to_owned() });
        (self.emit)(Event::Rows(songs_of(&self.songs, context).into_iter().map(Song::row).collect()));
        (self.emit)(Event::ListDone);
    }

    fn show_queue(&self) {
        (self.emit)(Event::List { title: "Up next".into(), context: String::new() });
        let upcoming = self.sequence.upcoming(80);
        (self.emit)(Event::Rows(upcoming.iter().filter_map(|uri| self.song(uri)).map(Song::row).collect()));
        (self.emit)(Event::ListDone);
    }

    fn song(&self, uri: &str) -> Option<&Song> {
        self.songs.iter().find(|s| s.path == uri)
    }

    fn position(&self) -> Duration {
        self.from + self.sink.as_ref().map(Sink::get_pos).unwrap_or_default()
    }

    fn playing(&self) -> bool {
        self.sink.as_ref().is_some_and(|s| !s.is_paused())
    }

    fn show_position(&self) {
        (self.emit)(Event::Position { ms: self.position().as_millis() as u32, playing: self.playing() });
    }

    fn set_volume(&mut self, volume: u16) {
        self.volume = volume;
        (self.emit)(Event::Volume(volume));
        if let Some(sink) = &self.sink {
            sink.set_volume(VolumeCtrl::default().to_mapped(volume) as f32);
        }
    }

    /// Moving within a song loads it again: a sink whose output is gone would never answer.
    fn load(&mut self, uri: &str, from: Duration, playing: bool) -> Result<(), Box<dyn std::error::Error>> {
        self.sink = None;
        let mut source = Decoder::try_from(File::open(uri)?)?;
        if !from.is_zero() {
            source.try_seek(from)?;
        }
        // The song's own rate leaves any conversion to Windows, which does it better than rodio.
        let rate = source.sample_rate();
        if self.stream.as_ref().is_none_or(|(old, _)| *old != rate) {
            self.stream = None;
            let mut stream = OutputStreamBuilder::from_default_device()?.with_sample_rate(rate).open_stream_or_fallback()?;
            stream.log_on_drop(false);
            self.stream = Some((rate, stream));
        }
        let (_, stream) = self.stream.as_ref().expect("opened above");
        let sink = Sink::connect_new(stream.mixer());
        sink.set_volume(VolumeCtrl::default().to_mapped(self.volume) as f32);
        if !playing {
            sink.pause();
        }
        sink.append(Equalized::new(source, self.shared.clone()));
        self.sink = Some(sink);
        self.from = from;
        Ok(())
    }

    /// Plays a song, or the first one after it that can be played.
    fn play(&mut self, mut next: Option<String>) {
        for _ in 0..FAILURES {
            let Some(uri) = next else { return self.stop() };
            match self.load(&uri, Duration::ZERO, true) {
                Ok(()) => {
                    self.announce(uri);
                    return self.show_position();
                }
                Err(e) => {
                    log::warn!("{uri}: {e}");
                    let name = Path::new(&uri).file_name().unwrap_or_default().to_string_lossy();
                    (self.emit)(Event::Status(format!("“{name}” could not be played: {e}")));
                }
            }
            next = self.sequence.ran_out();
        }
        self.stop();
    }

    fn announce(&mut self, uri: String) {
        self.now = uri.clone();
        if let Some(song) = self.song(&uri).cloned() {
            (self.emit)(Event::Track { uri: uri.clone(), title: song.title, artist: song.artist, album: song.album, duration_ms: song.duration_ms, cover_url: None });
        }
        let emit = self.emit.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(bytes) = cover(Path::new(&uri)) {
                emit(Event::Cover { uri, bytes });
            }
        });
        if self.queue_shown {
            self.show_queue();
        }
    }

    fn stop(&mut self) {
        self.sink = None;
        (self.emit)(Event::Stopped);
    }

    fn follow_up(&mut self) {
        if !self.sink.as_ref().is_some_and(Sink::empty) {
            return;
        }
        let next = if self.repeat_song { Some(self.now.clone()) } else { self.sequence.ran_out() };
        self.play(next);
    }

    fn seek(&mut self, to: Duration, playing: bool) {
        let uri = self.now.clone();
        if uri.is_empty() {
            return;
        }
        if let Err(e) = self.load(&uri, to, playing) {
            (self.emit)(Event::Status(format!("This song could not go on: {e}")));
            return self.stop();
        }
        self.show_position();
    }

    /// Goes on where Windows plays now.
    fn reopen(&mut self) {
        self.stream = None;
        if self.sink.is_some() {
            self.seek(self.position(), self.playing());
        }
    }

    fn set_playing(&mut self, playing: bool) {
        match &self.sink {
            Some(sink) if playing => sink.play(),
            Some(sink) => sink.pause(),
            // After the end of the list, the last song starts over.
            None if playing => return self.seek(Duration::ZERO, true),
            None => return,
        }
        self.show_position();
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Browse { title, uri } => self.show(title, &uri),
            Cmd::Liked => self.show("My files".into(), ALL),
            Cmd::Search(q) => self.show(format!("Results for “{}”", q.trim()), &format!("{SEARCH}{q}")),
            Cmd::Queue => {
                self.queue_shown = true;
                self.show_queue();
            }
            Cmd::Lyrics(uri) => (self.emit)(Event::Lyrics { uri, synced: false, lines: vec![], note: "No lyrics for your own files" }),
            Cmd::Play { context, track } => {
                let next = if context.is_empty() {
                    self.sequence.jump(&track)
                } else {
                    let list = songs_of(&self.songs, &context).into_iter().map(|s| s.path.clone()).collect();
                    self.sequence.start(list, &track)
                };
                if next.is_some() {
                    self.play(next);
                }
            }
            Cmd::Enqueue(uri) if self.sequence.active() => {
                self.sequence.enqueue(uri);
                if self.queue_shown {
                    self.show_queue();
                }
            }
            Cmd::Enqueue(uri) => {
                let first = self.sequence.start(vec![uri], "");
                self.play(first);
            }
            Cmd::ClearQueue => {
                self.sequence.clear_queue();
                if self.queue_shown {
                    self.show_queue();
                }
            }
            Cmd::Toggle => self.set_playing(!self.playing()),
            Cmd::Resume => self.set_playing(true),
            Cmd::Pause => self.set_playing(false),
            Cmd::Next => {
                let next = self.sequence.ran_out();
                self.play(next);
            }
            Cmd::Prev => {
                let early = self.position().as_millis() < RESTART_AFTER_MS.into();
                match if early { self.sequence.previous() } else { None } {
                    Some(uri) => self.play(Some(uri)),
                    None => self.seek(Duration::ZERO, true),
                }
            }
            Cmd::Seek(ms) => self.seek(Duration::from_millis(ms.into()), self.playing()),
            Cmd::Shuffle(on) => {
                self.sequence.set_shuffle(on);
                (self.emit)(Event::Shuffle(on));
                if self.queue_shown {
                    self.show_queue();
                }
            }
            Cmd::Repeat(mode) => {
                self.sequence.repeat = mode == 1;
                self.repeat_song = mode == 2;
                (self.emit)(Event::Repeat(mode));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use std::sync::Mutex;

    fn crc(bytes: &[u8], poly: u32, bits: u32) -> u32 {
        bytes.iter().fold(0, |crc, b| {
            (0..8).fold(crc ^ ((*b as u32) << (bits - 8)), |c, _| if c >> (bits - 1) & 1 == 1 { c << 1 ^ poly } else { c << 1 }) & ((1 << bits) - 1)
        })
    }

    /// A silent FLAC file of `frames` times 4096 samples in stereo.
    fn flac(tags: &[&str], frames: u8) -> Vec<u8> {
        let rate_channels_bits = 44100u64 << 44 | 1 << 41 | 15 << 36 | frames as u64 * 4096;
        let mut out = b"fLaC\x00\x00\x00\x22\x10\x00\x10\x00\x00\x00\x00\x00\x00\x00".to_vec();
        out.extend(rate_channels_bits.to_be_bytes());
        out.extend([0; 16]);
        let mut comment = vec![0; 4];
        comment.extend((tags.len() as u32).to_le_bytes());
        for tag in tags {
            comment.extend((tag.len() as u32).to_le_bytes());
            comment.extend(tag.bytes());
        }
        out.push(0x84);
        out.extend(&(comment.len() as u32).to_be_bytes()[1..]);
        out.extend(comment);
        for n in 0..frames {
            let mut frame = vec![0xff, 0xf8, 0xc9, 0x18, n];
            frame.push(crc(&frame, 0x07, 8) as u8);
            frame.extend([0; 6]);
            frame.extend((crc(&frame, 0x8005, 16) as u16).to_be_bytes());
            out.extend(frame);
        }
        out
    }

    /// Two albums of the same name, an untagged song, and things that aren't songs.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustbeat-{name}-{}", std::process::id()));
        for (path, bytes) in [
            ("one/b.flac", flac(&["TITLE=Second", "ARTIST=Ann", "ALBUM=Hits", "TRACKNUMBER=2/2"], 11)),
            ("one/a.FLAC", flac(&["TITLE=First", "ARTIST=Ann", "ALBUM=Hits", "TRACKNUMBER=1/2"], 11)),
            ("two/c.flac", flac(&["TITLE=Été", "ARTIST=Bob", "ALBUM=Hits"], 11)),
            ("loose/No tags.flac", flac(&[], 11)),
            ("one/cover.jpg", b"picture".to_vec()),
            ("one/broken.mp3", b"not music".to_vec()),
            ("one/notes.txt", b"text".to_vec()),
        ] {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        dir
    }

    #[test]
    fn a_folder_becomes_albums() {
        let dir = folder("scan");
        let songs = scan(&dir);
        let titles: Vec<_> = songs.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["No tags", "First", "Second", "Été"]);
        assert_eq!((songs[0].album.as_str(), songs[1].duration_ms), ("loose", 1021));
        let albums = albums(&songs);
        assert_eq!(albums.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(), ["loose", "Hits", "Hits"]);
        assert_eq!(songs_of(&songs, &albums[1].uri).len(), 2);
        assert_eq!(songs_of(&songs, ALL).len(), 4);
        assert_eq!(songs_of(&songs, "local:search: hits BOB").len(), 1);
        assert_eq!(songs_of(&songs, "local:search:été").len(), 1);
        assert!(songs_of(&songs, "spotify:playlist:x").is_empty());
        assert_eq!(cover(Path::new(&songs[1].path)), Some(b"picture".to_vec()));
        assert_eq!(cover(Path::new(&songs[0].path)), None);
        assert!(scan(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn player(songs: Vec<Song>) -> (Player, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let emit: Emit = { let events = events.clone(); Arc::new(move |e| events.lock().unwrap().push(e)) };
        let shared = Arc::new(Shared::new(Settings::default()));
        let player = Player { songs, emit, shared, sequence: Sequence::default(), sink: None, stream: None, now: String::new(), from: Duration::ZERO, repeat_song: false, volume: u16::MAX / 2, queue_shown: false };
        (player, events)
    }

    #[tokio::test]
    async fn files_that_are_gone_end_playback_instead_of_looping() {
        let (mut p, events) = player(vec![Song { path: "gone.flac".into(), ..Default::default() }]);
        p.command(Cmd::Repeat(1));
        p.command(Cmd::Toggle);
        p.command(Cmd::Seek(5));
        p.command(Cmd::Play { context: ALL.into(), track: String::new() });
        assert!(!p.playing());
        assert!(matches!(events.lock().unwrap().last(), Some(Event::Stopped)));
    }

    #[tokio::test]
    #[ignore = "needs a sound card; plays silence"]
    async fn songs_play_one_after_another() {
        let dir = folder("play");
        let songs = scan(&dir);
        let paths: Vec<_> = songs.iter().map(|s| s.path.clone()).collect();
        let (mut p, events) = player(songs);
        p.command(Cmd::Play { context: ALL.into(), track: paths[1].clone() });
        assert!(p.playing() && p.now == paths[1]);
        std::thread::sleep(Duration::from_millis(400));
        let at = p.position().as_millis();
        assert!((150..700).contains(&at), "{at}");
        p.command(Cmd::Pause);
        p.reopen();
        assert!(!p.playing() && p.position().as_millis() >= at);
        p.command(Cmd::Seek(800));
        p.command(Cmd::Enqueue(paths[0].clone()));
        p.command(Cmd::Resume);
        let mut heard = vec![p.now.clone()];
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(30));
            p.follow_up();
            if heard.last() != Some(&p.now) {
                heard.push(p.now.clone());
            }
        }
        assert_eq!(heard, [&paths[1], &paths[0], &paths[2], &paths[3]].map(String::clone));
        assert!(!p.playing() && matches!(events.lock().unwrap().last(), Some(Event::Stopped)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
