//! Spotify links (spotify:track:… and open.spotify.com/…) opening in Rustbeat. Windows asks the
//! handler of `spotify:` in the user's own classes, which is normally the Spotify app; Rustbeat
//! keeps that app's command aside while it handles them, and puts it back when it stops.

use std::{ffi::c_void, io, path::Path, ptr::null_mut};

/// The URI a link stands for: a song, album, playlist, artist or person.
pub fn uri_of(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(user) = crate::people::linked(text) {
        return Some(user);
    }
    let (kind, id) = match text.strip_prefix("spotify:") {
        // Some pages write spotify://track/ID.
        Some(rest) => rest.trim_start_matches("//").split_once([':', '/'])?,
        None => {
            let path = text.strip_prefix("https://").or_else(|| text.strip_prefix("http://")).unwrap_or(text).strip_prefix("open.spotify.com/")?;
            let path = path.split_once('/').filter(|(first, _)| first.starts_with("intl-")).map_or(path, |(_, rest)| rest);
            path.split_once('/')?
        }
    };
    let id = id.split(['?', '#', '/']).next()?;
    // Spotify's ids are 22 letters and digits; it becomes part of an address.
    let valid = ["track", "album", "playlist", "artist"].contains(&kind) && id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric());
    valid.then(|| format!("spotify:{kind}:{id}"))
}

const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
const REG_SZ: u32 = 1;
const RRF_RT_REG_SZ: u32 = 2;
const KEY: &str = r"Software\Classes\spotify";
const COMMAND: &str = r"Software\Classes\spotify\shell\open\command";
/// Where the command Rustbeat replaced is kept, next to it.
const KEPT: &str = "Rustbeat kept";

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(key: isize, sub: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, size: *mut u32) -> i32;
    fn RegSetKeyValueW(key: isize, sub: *const u16, value: *const u16, kind: u32, data: *const c_void, size: u32) -> i32;
    fn RegDeleteKeyValueW(key: isize, sub: *const u16, value: *const u16) -> i32;
    fn RegDeleteTreeW(key: isize, sub: *const u16) -> i32;
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn checked(code: i32) -> io::Result<()> {
    if code == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(code)) }
}

fn get(sub: &str, value: &str) -> Option<String> {
    let mut data = vec![0u16; 2048];
    let mut size = (data.len() * 2) as u32;
    let code = unsafe { RegGetValueW(HKEY_CURRENT_USER, wide(sub).as_ptr(), wide(value).as_ptr(), RRF_RT_REG_SZ, null_mut(), data.as_mut_ptr().cast(), &mut size) };
    (code == 0).then(|| String::from_utf16_lossy(&data[..(size as usize / 2).saturating_sub(1)]))
}

fn set(sub: &str, value: &str, text: &str) -> io::Result<()> {
    let data = wide(text);
    checked(unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, wide(sub).as_ptr(), wide(value).as_ptr(), REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) })
}

fn is_ours(command: &str) -> bool {
    command.to_lowercase().contains("rustbeat")
}

fn command_for(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

/// Makes Rustbeat (at `exe`) open `spotify:` links, or gives them back. Safe to repeat, which is
/// needed: the Spotify app takes them back each time it starts.
pub fn handle(exe: &Path, on: bool) -> io::Result<()> {
    let ours = command_for(exe);
    let now = get(COMMAND, "");
    let kept = get(COMMAND, KEPT);
    if on {
        if now.as_deref() == Some(ours.as_str()) {
            return Ok(());
        }
        // What was there before, unless it is an older Rustbeat's.
        if let Some(theirs) = now.filter(|c| !is_ours(c)) {
            set(COMMAND, KEPT, &theirs)?;
        }
        set(KEY, "", "URL:Spotify Protocol")?;
        set(KEY, "URL Protocol", "")?;
        return set(COMMAND, "", &ours);
    }
    if !now.is_some_and(|c| is_ours(&c)) {
        return Ok(());
    }
    match kept {
        Some(theirs) => {
            set(COMMAND, "", &theirs)?;
            checked(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(COMMAND).as_ptr(), wide(KEPT).as_ptr()) })
        }
        // Nobody had them before Rustbeat.
        None => checked(unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(KEY).as_ptr()) }),
    }
}

#[cfg(test)]
mod tests {
    use super::uri_of;

    #[test]
    fn links_become_uris() {
        let id = "4uLU6hMCjMI75M1A2tKUQC";
        for link in [
            format!("spotify:track:{id}"),
            format!("spotify://track/{id}"),
            format!("https://open.spotify.com/track/{id}?si=abc"),
            format!("open.spotify.com/intl-it/track/{id}"),
            format!(" spotify:track:{id}\n"),
        ] {
            assert_eq!(uri_of(&link).as_deref(), Some(format!("spotify:track:{id}").as_str()), "{link}");
        }
        assert_eq!(uri_of(&format!("https://open.spotify.com/album/{id}")).as_deref(), Some(format!("spotify:album:{id}").as_str()));
        assert_eq!(uri_of("https://open.spotify.com/user/some.one").as_deref(), Some("spotify:user:some.one"));
        for bad in [
            "spotify:track:short".to_owned(),
            format!("spotify:episode:{id}"),
            format!("https://evil.example/track/{id}"),
            format!("spotify:track:{}%2F", &id[..21]),
            "spotify:".to_owned(),
            String::new(),
        ] {
            assert_eq!(uri_of(&bad), None, "{bad}");
        }
    }
}
