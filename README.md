# Quaver

A small native Spotify client for Windows: Rust, Slint (software rendered, no browser engine) and librespot.

> **Unofficial.** Quaver is not affiliated with, endorsed by or approved by Spotify. It reads your library through
> [librespot](https://github.com/librespot-org/librespot) and undocumented Spotify addresses, which Spotify may change,
> block or object to at any time, and using it may be against its terms. Use it at your own risk and with your own
> account. Spotify is a trademark of Spotify AB.

## Use

Needs Windows, Spotify Premium and [Rust](https://rustup.rs) with the Visual C++ build tools.

- Build: `cargo build --release`, then run `target\release\quaver.exe`
- Test: `cargo test`
- Logs: set `RUST_LOG=info` and run a debug build (`cargo run`)

Closing the window keeps it in the tray; quit from the tray icon.

Shortcuts: Space play/pause, Ctrl+Left/Right previous/next, Alt+Left/Right back/forward, Ctrl+F search, Ctrl+H home,
Ctrl+, settings, F11 full screen, Esc leave.

## Playback

Spotify doesn't let librespot play on newer accounts ("audio key error"). For those, choose Spotify Soloist in Settings:
Spotify's own player, which Quaver downloads from Spotify when you ask it to. It needs your own API key, and each build
lasts 90 days. Soloist is Spotify's software under Spotify's terms; it is not part of this repository.

Can't connect? `cargo test probe -- --ignored --nocapture` prints why this network can't reach Spotify.

## Your data

Everything is in `%LOCALAPPDATA%\Quaver`: the login in `credentials.json` ("Sign out" deletes it), the Soloist key in
`soloist-key.txt` and the settings in `settings.txt`. The login and the key are plain files; don't share that folder.

## License

[MIT](LICENSE). Dependencies have their own licenses; Slint is used under its
[royalty-free license](https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md).

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)
