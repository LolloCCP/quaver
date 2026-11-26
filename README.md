# Rustbeat

A small native Spotify client for Windows: Rust, Slint (software rendered, no browser engine) and librespot.

> **Unofficial.** Rustbeat is not affiliated with, endorsed by or approved by Spotify. It reads your library through
> [librespot](https://github.com/librespot-org/librespot) and undocumented Spotify addresses, which Spotify may change,
> block or object to at any time, and using it may be against its terms. Use it at your own risk and with your own
> account. Spotify is a trademark of Spotify AB.

## Use

Needs Windows and Spotify Premium. Get `rustbeatinstaller.exe` from the
[latest release](https://github.com/LolloCCP/rustbeat/releases/latest); it installs for you alone, without admin rights. The
installer isn't signed, so Windows may warn about it: "More info", then "Run anyway".

Rustbeat looks for a new version on GitHub when it starts and uses it from the next start. Settings > Updates goes back to
an older one and stays there, or turns looking off.

To build it yourself you need [Rust](https://rustup.rs) with the Visual C++ build tools:

- Build: `cargo build --release`, then run `target\release\rustbeat.exe`
- Test: `cargo test`
- Logs: set `RUST_LOG=info` and run a debug build (`cargo run`)

Every push to `main` becomes a release (`.github/workflows/release.yml`).

Closing the window keeps it in the tray; quit from the tray icon.

Shortcuts: Space play/pause, Ctrl+Left/Right previous/next, Alt+Left/Right back/forward, Ctrl+F search, Ctrl+H home,
Ctrl+, settings, F11 full screen, Esc leave.

## Playback

Spotify doesn't let librespot play on newer accounts ("audio key error"). For those, choose Spotify Soloist in Settings:
Spotify's own player, which Rustbeat downloads from Spotify when you ask it to. It needs your own API key, and each build
lasts 90 days. Soloist is Spotify's software under Spotify's terms; it is not part of this repository.

Can't connect? `cargo test probe -- --ignored --nocapture` prints why this network can't reach Spotify.

The built-in player and Offline convert songs to the rate your sound device runs at (mostly 48 kHz) with a windowed-sinc
filter, instead of rodio's linear one, which dulls the highs.

Through Soloist, Rustbeat asks for lossless and turns Spotify's loudness normalization off (or on, with "Same volume for
every song"). That uses undocumented lines of Soloist's settings, which an update may change.

Lists start through Spotify Connect's own, undocumented, commands, so the Spotify app sees the same list and can go on
with it; without them only the chosen song plays. Pressing play in Rustbeat takes over from another device.

## Karaoke

The microphone button in Now Playing turns on karaoke, made to be left on a second screen: the cover on one side, in the
style you pick (cover, a spinning vinyl record, a spinning CD or a mirrored cover), and timed lyrics gliding by on the
other, the line being sung large and the others small and faded. Click a line to go to it.

## Friends

The people button shows what friends listen to, your blends and group playlists, and whom you follow; paste the link to a
profile into the search box to open it, see its public playlists and follow it. Blends are made in the Spotify app.

## Offline

Settings > Offline plays the MP3, FLAC and Ogg Vorbis files in a folder of yours, without Spotify, a login or a network;
the equalizer works there. Songs downloaded in the Spotify app are encrypted for it and can't be played.

## Your data

Everything is in `%LOCALAPPDATA%\Rustbeat`: the login in `credentials.json` ("Sign out" deletes it), the Soloist key in
`soloist-key.txt` and the settings in `settings.txt`. The login and the key are plain files; don't share that folder.

## Privacy

Rustbeat has no analytics or tracking. It talks to Spotify, and to GitHub when it starts to look for new versions (off in
Settings > Updates); [PRIVACY.txt](PRIVACY.txt) has the details, and the installer shows it.

## License

[MIT](LICENSE). Dependencies have their own licenses; Slint is used under its
[royalty-free license](https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md).

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)
