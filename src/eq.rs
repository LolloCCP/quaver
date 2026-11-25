//! Ten-band equalizer that sits between the decoder and the sound card.

use std::{
    f64::consts::TAU,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use librespot_playback::{
    NUM_CHANNELS, SAMPLE_RATE,
    audio_backend::{Sink, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};

use rodio::Source;

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

/// The filters as the settings want them.
struct Equalizer {
    shared: Arc<Shared>,
    rate: f64,
    seen: u32,
    chain: Option<Chain>,
}

impl Equalizer {
    fn new(shared: Arc<Shared>, rate: u32) -> Self {
        Self { shared, rate: rate.into(), seen: 0, chain: None }
    }

    /// What to filter with now, if at all.
    fn chain(&mut self) -> Option<&mut Chain> {
        let version = self.shared.eq_version.load(Ordering::Acquire);
        if version != self.seen {
            self.seen = version;
            let settings = self.shared.lock().clone();
            let gains = Some(settings.eq).filter(|eq| settings.eq_on && eq.iter().any(|g| *g != 0.));
            self.chain = gains.map(|gains| Chain::new(&gains, self.rate, self.chain.take()));
        }
        self.chain.as_mut()
    }
}

pub struct EqSink {
    inner: Box<dyn Sink>,
    equalizer: Equalizer,
}

impl EqSink {
    pub fn new(inner: Box<dyn Sink>, shared: Arc<Shared>) -> Self {
        Self { inner, equalizer: Equalizer::new(shared, SAMPLE_RATE) }
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
        if let (Some(chain), AudioPacket::Samples(samples)) = (self.equalizer.chain(), &mut packet) {
            for frame in samples.chunks_mut(CHANNELS) {
                for (channel, sample) in frame.iter_mut().enumerate() {
                    *sample = chain.filter(channel, *sample);
                }
            }
        }
        self.inner.write(packet, converter)
    }
}

/// The same for a file that rodio plays.
pub struct Equalized<S> {
    inner: S,
    equalizer: Equalizer,
    channel: usize,
}

impl<S: Source> Equalized<S> {
    pub fn new(inner: S, shared: Arc<Shared>) -> Self {
        let equalizer = Equalizer::new(shared, inner.sample_rate());
        Self { inner, equalizer, channel: 0 }
    }
}

impl<S: Source> Iterator for Equalized<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next()?;
        let channel = self.channel;
        self.channel = (channel + 1) % self.inner.channels().max(1) as usize;
        Some(match self.equalizer.chain() {
            Some(chain) => chain.filter(channel, sample.into()) as f32,
            None => sample,
        })
    }
}

impl<S: Source> Source for Equalized<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> u16 {
        self.inner.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
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
    fn new(gains: &[f32; BANDS], rate: f64, old: Option<Chain>) -> Self {
        let mut bands: [Band; BANDS] = std::array::from_fn(|i| Band::peaking(CENTERS[i], gains[i] as f64, rate));
        if let Some(old) = old {
            for (band, old) in bands.iter_mut().zip(old.bands) {
                band.z = old.z;
            }
        }
        let loudest = (0..400).map(|i| response(&bands, 20. * 1000f64.powf(i as f64 / 399.), rate)).fold(1., f64::max);
        Self { bands, level: 1. / loudest }
    }

    /// Channels beyond the first two are only turned down along with them.
    fn filter(&mut self, channel: usize, sample: f64) -> f64 {
        let mut x = sample * self.level;
        for Band { b, a, z } in &mut self.bands {
            let Some(z) = z.get_mut(channel) else { break };
            let y = b[0] * x + z[0];
            z[0] = b[1] * x - a[0] * y + z[1];
            z[1] = b[2] * x - a[1] * y;
            x = y;
        }
        x
    }
}

impl Band {
    /// Peaking filter from the RBJ Audio EQ Cookbook.
    fn peaking(center: f64, gain_db: f64, rate: f64) -> Self {
        // Too high for a low sample rate: there the filter would be unstable.
        if center > rate * 0.45 {
            return Self { b: [1., 0., 0.], a: [0.; 2], z: Default::default() };
        }
        let amp = 10f64.powf(gain_db / 40.);
        let w = TAU * center / rate;
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
fn response(bands: &[Band], frequency: f64, rate: f64) -> f64 {
    let w = TAU * frequency / rate;
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

    const RATE: f64 = SAMPLE_RATE as f64;

    fn process(gains: &[f32; BANDS], samples: &mut [f64]) {
        let mut chain = Chain::new(gains, RATE, None);
        for (i, sample) in samples.iter_mut().enumerate() {
            *sample = chain.filter(i % CHANNELS, *sample);
        }
    }

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
        process(&gains, &mut at);
        process(&gains, &mut far);
        assert!((20. * peak(&at).log10() + 6.).abs() < 0.1, "{}", peak(&at));
        assert!((peak(&far) - 1.).abs() < 0.02, "{}", peak(&far));
    }

    #[test]
    fn no_preset_or_extreme_can_boost_past_full_scale() {
        for gains in PRESETS.map(|p| p.1).into_iter().chain([[12.; BANDS], [-12.; BANDS]]) {
            let chain = Chain::new(&gains, RATE, None);
            for f in (20..20_000).step_by(7) {
                assert!(response(&chain.bands, f as f64, RATE) * chain.level <= 1.001, "{gains:?} at {f} Hz");
            }
        }
    }

    #[test]
    fn channels_stay_separate_and_long_runs_stay_finite() {
        let mut samples = sine(440.);
        process(&[12.; BANDS], &mut samples);
        assert!(samples.iter().skip(1).step_by(2).all(|right| *right == 0.));
        assert!(samples.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn files_of_any_rate_and_channels_are_equalized() {
        let shared = Arc::new(Shared::new(Settings { eq: [-12.; BANDS], eq_on: true, ..Default::default() }));
        for (rate, channels) in [(8000, 1), (22050, 2), (48000, 2), (192_000, 6)] {
            let frames = rate as usize;
            let tone: Vec<f32> = (0..frames).flat_map(|i| vec![(TAU * 440. * i as f64 / rate as f64).sin() as f32; channels as usize]).collect();
            let out: Vec<f32> = Equalized::new(rodio::buffer::SamplesBuffer::new(channels, rate, tone.clone()), shared.clone()).collect();
            assert_eq!(out.len(), tone.len());
            assert!(out.iter().all(|s| s.is_finite()));
            let left = out[out.len() / 2..].iter().step_by(channels as usize).fold(0f32, |m, v| m.max(v.abs()));
            assert!(left < 0.3, "{rate} Hz: {left}");
        }
        shared.lock().eq_on = false;
        shared.eq_version.fetch_add(1, Ordering::Release);
        let plain = vec![0.5, -0.25, 1., -1.];
        assert_eq!(Equalized::new(rodio::buffer::SamplesBuffer::new(2, 44100, plain.clone()), shared).collect::<Vec<_>>(), plain);
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
