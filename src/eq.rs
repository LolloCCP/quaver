//! Ten-band equalizer that sits between the decoder and the sound card.

use std::{
    f64::consts::TAU,
    sync::{Arc, atomic::Ordering},
};

use librespot_playback::{
    NUM_CHANNELS, SAMPLE_RATE,
    audio_backend::{Sink, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};

use crate::settings::{BANDS, Shared};

const CENTERS: [f64; BANDS] = [31.25, 62.5, 125., 250., 500., 1000., 2000., 4000., 8000., 16000.];
/// One octave wide, the spacing of the bands.
const Q: f64 = std::f64::consts::SQRT_2;
const CHANNELS: usize = NUM_CHANNELS as usize;

pub const PRESETS: [(&str, [f32; BANDS]); 6] = [
    ("Flat", [0.; BANDS]),
    ("Bass", [6., 5., 4., 2., 0., 0., 0., 0., 0., 0.]),
    ("Vocal", [-2., -2., -1., 1., 3., 4., 4., 2., 0., -1.]),
    ("Treble", [0., 0., 0., 0., 0., 1., 2., 4., 5., 6.]),
    ("Loudness", [5., 4., 1., 0., -1., -1., 0., 2., 4., 5.]),
    ("Soft", [2., 1., 0., 0., 0., -1., -2., -3., -4., -5.]),
];

pub struct EqSink {
    inner: Box<dyn Sink>,
    shared: Arc<Shared>,
    seen: u32,
    chain: Option<Chain>,
}

impl EqSink {
    pub fn new(inner: Box<dyn Sink>, shared: Arc<Shared>) -> Self {
        Self { inner, shared, seen: 0, chain: None }
    }
}

impl Sink for EqSink {
    fn start(&mut self) -> SinkResult<()> {
        self.inner.start()
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.inner.stop()
    }

    fn write(&mut self, mut packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let version = self.shared.eq_version.load(Ordering::Acquire);
        if version != self.seen {
            self.seen = version;
            let settings = self.shared.lock().clone();
            let gains = Some(settings.eq).filter(|eq| settings.eq_on && eq.iter().any(|g| *g != 0.));
            self.chain = gains.map(|gains| Chain::new(&gains, self.chain.take()));
        }
        if let (Some(chain), AudioPacket::Samples(samples)) = (&mut self.chain, &mut packet) {
            chain.process(samples);
        }
        self.inner.write(packet, converter)
    }
}

struct Chain {
    bands: [Band; BANDS],
    /// Turned down by the loudest boost, so a boost can't clip.
    level: f64,
}

#[derive(Clone, Copy)]
struct Band {
    b: [f64; 3],
    a: [f64; 2],
    z: [[f64; 2]; CHANNELS],
}

impl Chain {
    /// Keeps the filters' memory from `old`, so moving a slider doesn't click.
    fn new(gains: &[f32; BANDS], old: Option<Chain>) -> Self {
        let mut bands: [Band; BANDS] = std::array::from_fn(|i| Band::peaking(CENTERS[i], gains[i] as f64));
        if let Some(old) = old {
            for (band, old) in bands.iter_mut().zip(old.bands) {
                band.z = old.z;
            }
        }
        let loudest = (0..400).map(|i| response(&bands, 20. * 1000f64.powf(i as f64 / 399.))).fold(1., f64::max);
        Self { bands, level: 1. / loudest }
    }

    fn process(&mut self, samples: &mut [f64]) {
        for frame in samples.chunks_mut(CHANNELS) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let mut x = *sample * self.level;
                for Band { b, a, z } in &mut self.bands {
                    let z = &mut z[channel];
                    let y = b[0] * x + z[0];
                    z[0] = b[1] * x - a[0] * y + z[1];
                    z[1] = b[2] * x - a[1] * y;
                    x = y;
                }
                *sample = x;
            }
        }
    }
}

impl Band {
    /// Peaking filter from the RBJ Audio EQ Cookbook.
    fn peaking(center: f64, gain_db: f64) -> Self {
        let amp = 10f64.powf(gain_db / 40.);
        let w = TAU * center / SAMPLE_RATE as f64;
        let alpha = w.sin() / (2. * Q);
        let a0 = 1. + alpha / amp;
        Self {
            b: [(1. + alpha * amp) / a0, -2. * w.cos() / a0, (1. - alpha * amp) / a0],
            a: [-2. * w.cos() / a0, (1. - alpha / amp) / a0],
            z: Default::default(),
        }
    }
}

/// Gain of all bands together at one frequency.
fn response(bands: &[Band], frequency: f64) -> f64 {
    let w = TAU * frequency / SAMPLE_RATE as f64;
    let magnitude = |c: [f64; 3]| {
        let re = c[0] + c[1] * w.cos() + c[2] * (2. * w).cos();
        let im = c[1] * w.sin() + c[2] * (2. * w).sin();
        re.hypot(im)
    };
    bands.iter().map(|f| magnitude(f.b) / magnitude([1., f.a[0], f.a[1]])).product()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use std::sync::Mutex;

    fn sine(frequency: f64) -> Vec<f64> {
        (0..SAMPLE_RATE as usize)
            .flat_map(|i| [(TAU * frequency * i as f64 / SAMPLE_RATE as f64).sin(), 0.])
            .collect()
    }

    /// Peak of the left channel once the filters have settled.
    fn peak(samples: &[f64]) -> f64 {
        samples[samples.len() / 2..].iter().step_by(2).fold(0f64, |m, v| m.max(v.abs()))
    }

    #[test]
    fn a_band_changes_its_own_frequency_by_its_gain() {
        let mut gains = [0.; BANDS];
        gains[5] = -6.;
        let (mut at, mut far) = (sine(1000.), sine(60.));
        Chain::new(&gains, None).process(&mut at);
        Chain::new(&gains, None).process(&mut far);
        assert!((20. * peak(&at).log10() + 6.).abs() < 0.1, "{}", peak(&at));
        assert!((peak(&far) - 1.).abs() < 0.02, "{}", peak(&far));
    }

    #[test]
    fn no_preset_or_extreme_can_boost_past_full_scale() {
        for gains in PRESETS.map(|p| p.1).into_iter().chain([[12.; BANDS], [-12.; BANDS]]) {
            let chain = Chain::new(&gains, None);
            for f in (20..20_000).step_by(7) {
                assert!(response(&chain.bands, f as f64) * chain.level <= 1.001, "{gains:?} at {f} Hz");
            }
        }
    }

    #[test]
    fn channels_stay_separate_and_long_runs_stay_finite() {
        let mut samples = sine(440.);
        Chain::new(&[12.; BANDS], None).process(&mut samples);
        assert!(samples.iter().skip(1).step_by(2).all(|right| *right == 0.));
        assert!(samples.iter().all(|s| s.is_finite()));
    }

    struct Recorder(Arc<Mutex<Vec<f64>>>);
    impl Sink for Recorder {
        fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
            self.0.lock().unwrap().extend(packet.samples().unwrap());
            Ok(())
        }
    }

    #[test]
    fn the_switch_works_while_playing() {
        let shared = Arc::new(Shared::new(Settings { eq: [-12.; BANDS], ..Default::default() }));
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut sink = EqSink::new(Box::new(Recorder(out.clone())), shared.clone());
        let mut converter = Converter::new(None);
        let mut play = |sink: &mut EqSink| {
            sink.write(AudioPacket::Samples(sine(440.)), &mut converter).unwrap();
            std::mem::take(&mut *out.lock().unwrap())
        };
        assert_eq!(play(&mut sink), sine(440.), "off must not touch the sound");
        shared.lock().eq_on = true;
        shared.eq_version.fetch_add(1, Ordering::Release);
        assert!(peak(&play(&mut sink)) < 0.3);
        shared.lock().eq_on = false;
        shared.eq_version.fetch_add(1, Ordering::Release);
        assert_eq!(play(&mut sink), sine(440.));
    }
}
