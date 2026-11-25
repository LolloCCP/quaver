# Quaver

A small native Spotify client for Windows: Rust, Slint (software rendered, no browser engine) and librespot.

- Build: `cargo build --release`, then run `target\release\quaver.exe`
- Test: `cargo test`
- Logs: set `RUST_LOG=info` and run a debug build (`cargo run`)

Needs Spotify Premium. Closing the window keeps it in the tray; quit from the tray icon.
The saved login is `%LOCALAPPDATA%\Quaver\credentials.json`; "Sign out" deletes it.

librespot is unofficial and not approved by Spotify. Use it at your own risk.

Shortcuts: Space play/pause, Ctrl+Left/Right previous/next, Alt+Left/Right back/forward, Ctrl+F search, Ctrl+H home, Ctrl+, settings, F11 full screen, Esc leave.

Settings are in `%LOCALAPPDATA%Quaversettings.txt`.

Can't connect? `cargo test probe -- --ignored --nocapture` prints why this network can't reach Spotify.

Songs refused ("audio key error")? Spotify doesn't let librespot play on newer accounts. Choose Spotify Soloist
in Settings; it needs your own API key and a build from Spotify that lasts 90 days.
