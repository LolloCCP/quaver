use std::{
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError, atomic::AtomicU32},
};

pub const BANDS: usize = 10;
pub const MIN_TINT: f32 = 0.25;
/// Largest boost or cut of one equalizer band, in dB.
pub const MAX_GAIN: f32 = 12.;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub bitrate: u16,
    pub normalise: bool,
    pub tray: bool,
    /// How dark the glass is. Clearer than the minimum, text gets hard to read.
    pub tint: f32,
    pub eq_on: bool,
    pub eq: [f32; BANDS],
}

impl Default for Settings {
    fn default() -> Self {
        Self { bitrate: 320, normalise: false, tray: true, tint: 0.4, eq_on: false, eq: [0.; BANDS] }
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
        let Self { bitrate, normalise, tray, tint, eq_on, eq } = self;
        let eq = eq.map(|g| g.to_string()).join(",");
        format!("bitrate={bitrate}\nnormalise={normalise}\ntray={tray}\ntint={tint}\neq_on={eq_on}\neq={eq}\n")
    }

    fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
            let flag = |old| value.parse().unwrap_or(old);
            match key {
                "bitrate" => s.bitrate = value.parse().ok().filter(|b| [96, 160, 320].contains(b)).unwrap_or(s.bitrate),
                "normalise" => s.normalise = flag(s.normalise),
                "tray" => s.tray = flag(s.tray),
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
        let mut s = Settings { bitrate: 96, normalise: true, tray: false, tint: 0.5, eq_on: true, ..Default::default() };
        s.eq[3] = -4.5;
        assert_eq!(Settings::parse(&s.text()), s);
    }

    #[test]
    fn damaged_files_fall_back_to_defaults() {
        let text = "bitrate=9999\ntint=NaN\ntray=perhaps\neq=1,2,3\nunknown=1\nnonsense";
        assert_eq!(Settings::parse(text), Settings::default());
        assert_eq!(Settings::parse("eq=99,0,0,0,0,0,0,0,0,-99").eq[0], MAX_GAIN);
    }
}
