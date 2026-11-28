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
    let (kind, id) = parts(text)?;
    // Spotify's ids are 22 letters and digits; it becomes part of an address.
    let valid = ["track", "album", "playlist", "artist"].contains(&kind) && id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric());
    valid.then(|| format!("spotify:{kind}:{id}"))
}

/// What a link is to, and its id.
fn parts(text: &str) -> Option<(&str, &str)> {
    let (kind, id) = match text.trim().strip_prefix("spotify:") {
        // Some pages write spotify://track/ID.
        Some(rest) => rest.trim_start_matches("//").split_once([':', '/'])?,
        None => {
            let text = text.trim();
            let path = text.strip_prefix("https://").or_else(|| text.strip_prefix("http://")).unwrap_or(text).strip_prefix("open.spotify.com/")?;
            let path = path.split_once('/').filter(|(first, _)| first.starts_with("intl-")).map_or(path, |(_, rest)| rest);
            path.split_once('/')?
        }
    };
    Some((kind, id.split(['?', '#', '/']).next()?))
}

/// A Jam invite, as the URI the Spotify app joins it by.
pub fn jam_of(text: &str) -> Option<String> {
    let (kind, id) = parts(text)?;
    // It goes into a command line: only letters, digits, - and _.
    let valid = kind == "socialsession" && (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then(|| format!("spotify:socialsession:{id}"))
}

/// Hands a link to the program Rustbeat took `spotify:` links from: Rustbeat can't join a Jam
/// itself, the Spotify app does, and can then pick Rustbeat as its speaker.
pub fn pass_on(uri: &str) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let theirs = get(COMMAND, KEPT).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no other program opened Spotify links before Rustbeat"))?;
    let (program, args) = split_command(&theirs);
    std::process::Command::new(program).raw_arg(args.replace("%1", uri)).spawn().map(drop)
}

/// The file of the program `pass_on` starts, such as Spotify.exe.
fn their_exe() -> Option<String> {
    let command = get(COMMAND, KEPT)?;
    Path::new(split_command(&command).0).file_name()?.to_str().map(str::to_owned)
}

fn system_tool(program: &str, args: &[&str]) -> Option<std::process::Output> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let system = Path::new(&std::env::var_os("SystemRoot")?).join("System32");
    std::process::Command::new(system.join(program)).args(args).creation_flags(CREATE_NO_WINDOW).output().ok()
}

/// Whether the program `pass_on` starts already runs.
pub fn theirs_running() -> bool {
    let Some(exe) = their_exe() else { return false };
    let listed = system_tool("tasklist.exe", &["/FI", &format!("IMAGENAME eq {exe}"), "/NH"]);
    listed.is_some_and(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains(&exe.to_lowercase()))
}

/// Closes the program `pass_on` started.
pub fn close_theirs() {
    if let Some(exe) = their_exe() {
        system_tool("taskkill.exe", &["/IM", &exe, "/F"]);
    }
}

/// A command as the registry keeps it: the program, maybe in quotes, then its arguments.
fn split_command(command: &str) -> (&str, &str) {
    let command = command.trim();
    match command.strip_prefix('"').and_then(|rest| rest.split_once('"')) {
        Some((program, args)) => (program, args.trim()),
        None => command.split_once(' ').map_or((command, ""), |(program, args)| (program, args.trim())),
    }
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

/// Whether Windows' "Transparency effects" are on (Settings, Personalization, Colors). Here
/// because the registry is read here; the glass theme turns opaque without them.
pub fn transparency_on() -> bool {
    const RRF_RT_REG_DWORD: u32 = 0x10;
    let mut on = 1u32;
    let mut size = 4u32;
    let (sub, value) = (r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "EnableTransparency");
    let code = unsafe { RegGetValueW(HKEY_CURRENT_USER, wide(sub).as_ptr(), wide(value).as_ptr(), RRF_RT_REG_DWORD, null_mut(), (&raw mut on).cast(), &mut size) };
    code != 0 || on != 0
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
    use super::{jam_of, split_command, uri_of};

    #[test]
    fn jam_invites_go_to_the_spotify_app() {
        for link in ["https://open.spotify.com/socialsession/Ab3_x-9?si=1", "spotify:socialsession:Ab3_x-9", "open.spotify.com/intl-it/socialsession/Ab3_x-9"] {
            assert_eq!(jam_of(link).as_deref(), Some("spotify:socialsession:Ab3_x-9"), "{link}");
        }
        for bad in ["spotify:socialsession:a\"b", "spotify:socialsession:", "spotify:socialsession:a b", "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"] {
            assert_eq!(jam_of(bad), None, "{bad}");
        }
        assert_eq!(uri_of("spotify:socialsession:Ab3"), None, "not something Rustbeat opens");
        assert_eq!(split_command(r#""C:\Spot ify\Spotify.exe" --protocol-uri="%1""#), (r"C:\Spot ify\Spotify.exe", r#"--protocol-uri="%1""#));
        assert_eq!(split_command(r"C:\Spotify.exe %1"), (r"C:\Spotify.exe", "%1"));
    }

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
