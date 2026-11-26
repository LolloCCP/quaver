use std::{
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError, atomic::AtomicU32},
};

pub const BANDS: usize = 10;
pub const MIN_TINT: f32 = 0.4;
/// Largest boost or cut of one equalizer band, in dB.
pub const MAX_GAIN: f32 = 12.;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub bitrate: u16,
    pub normalise: bool,
    pub tray: bool,
    /// How dark the glass is. Clearer than the minimum, text gets hard to read.
    pub tint: f32,
    /// Animations, the pointer's light and gliding scrolling.
    pub motion: bool,
    pub theme: String,
    /// The custom theme's background and highlight.
    pub custom: [[u8; 3]; 2],
    /// Play through Spotify Soloist instead of librespot.
    pub soloist: bool,
    /// Play the files in `music`, without Spotify.
    pub offline: bool,
    pub music: String,
    pub eq_on: bool,
    pub eq: [f32; BANDS],
    /// The release to stay on; empty for always the newest.
    pub version: String,
    /// Ask GitHub for new versions at start.
    pub updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { bitrate: 320, normalise: false, tray: true, tint: 0.45, motion: true, theme: "glass".into(), custom: [[30, 34, 54], [255, 150, 80]], soloist: false, offline: false, music: music_folder(), eq_on: false, eq: [0.; BANDS], version: String::new(), updates: true }
    }
}

impl Settings {
    /// A missing file or a bad line just means the default.
    pub fn load(path: &Path) -> Self {
        Self::parse(&std::fs::read_to_string(path).unwrap_or_default())
    }

    /// Writes a new file and swaps it in, so a crash can't leave half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let new = path.with_extension("new");
        std::fs::write(&new, self.text())?;
        std::fs::rename(new, path)
    }

    fn text(&self) -> String {
        let Self { bitrate, normalise, tray, tint, motion, theme, custom, soloist, offline, music, eq_on, eq, version, updates } = self;
        let custom = custom.map(hex).join(",");
        let eq = eq.map(|g| g.to_string()).join(",");
        format!("bitrate={bitrate}\nnormalise={normalise}\ntray={tray}\ntint={tint}\nmotion={motion}\ntheme={theme}\ncustom={custom}\nsoloist={soloist}\noffline={offline}\nmusic={music}\neq_on={eq_on}\neq={eq}\nversion={version}\nupdates={updates}\n")
    }

    fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
            let flag = |old| value.parse().unwrap_or(old);
            match key {
                "bitrate" => s.bitrate = value.parse().ok().filter(|b| [96, 160, 320].contains(b)).unwrap_or(s.bitrate),
                "normalise" => s.normalise = flag(s.normalise),
                "tray" => s.tray = flag(s.tray),
                "motion" => s.motion = flag(s.motion),
                // An unknown theme is shown as the first one.
                "theme" => s.theme = value.trim().to_owned(),
                "custom" => {
                    let colors: Vec<_> = value.split(',').filter_map(parse_hex).collect();
                    s.custom = colors.try_into().unwrap_or(s.custom);
                }
                "soloist" => s.soloist = flag(s.soloist),
                "offline" => s.offline = flag(s.offline),
                "music" => s.music = value.to_owned(),
                "version" => s.version = value.trim().to_owned(),
                "updates" => s.updates = flag(s.updates),
                "eq_on" => s.eq_on = flag(s.eq_on),
                "tint" => s.tint = value.parse().ok().filter(|t: &f32| t.is_finite()).unwrap_or(s.tint).clamp(MIN_TINT, 0.85),
                "eq" => {
                    let gains: Vec<f32> = value.split(',').filter_map(|g| g.parse().ok()).filter(|g: &f32| g.is_finite()).collect();
                    if let Ok(gains) = <[f32; BANDS]>::try_from(gains) {
                        s.eq = gains.map(|g| g.clamp(-MAX_GAIN, MAX_GAIN));
                    }
                }
                _ => {}
            }
        }
        s
    }
}

fn music_folder() -> String {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    format!(r"{home}\Music")
}

pub fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

pub fn parse_hex(text: &str) -> Option<[u8; 3]> {
    let digits = text.trim().trim_start_matches('#');
    let valid = digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit());
    let [_, r, g, b] = u32::from_str_radix(digits, 16).ok().filter(|_| valid)?.to_be_bytes();
    Some([r, g, b])
}

/// Settings as seen by the UI, the connection and the audio thread.
pub struct Shared {
    settings: Mutex<Settings>,
    /// Changes whenever the equalizer does, so the audio thread rarely locks.
    pub eq_version: AtomicU32,
}

impl Shared {
    pub fn new(settings: Settings) -> Self {
        Self { settings: Mutex::new(settings), eq_version: AtomicU32::new(1) }
    }

    pub fn lock(&self) -> MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_saved_is_what_loads() {
        let mut s = Settings { bitrate: 96, normalise: true, tray: false, tint: 0.5, motion: false, theme: "paper".into(), custom: [[0, 1, 2], [253, 254, 255]], soloist: true, offline: true, music: r"D:\My music=loud, été".into(), eq_on: true, version: "v0.1.2".into(), updates: false, ..Default::default() };
        s.eq[3] = -4.5;
        assert_eq!(Settings::parse(&s.text()), s);
    }

    #[test]
    fn damaged_files_fall_back_to_defaults() {
        let text = "bitrate=9999\ntint=NaN\ntray=perhaps\ncustom=#12345g,#ffffff\neq=1,2,3\nunknown=1\nnonsense";
        assert_eq!(Settings::parse(text), Settings::default());
        assert_eq!(parse_hex(" 0A0b0C "), Some([10, 11, 12]));
        for bad in ["", "#", "#fff", "+12345", "#1234567", "ééé"] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
        assert_eq!(Settings::parse("eq=99,0,0,0,0,0,0,0,0,-99").eq[0], MAX_GAIN);
    }
}
