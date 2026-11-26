//! Windowed-sinc sample rate conversion. Windows' shared mode only takes the device's own rate (mostly 48 kHz), and
//! rodio's linear conversion to it dulls the highs and folds noise back into the audible band.

use std::{f64::consts::PI, thread, time::Duration};

use librespot_playback::{
    NUM_CHANNELS, SAMPLE_RATE,
    audio_backend::{Sink, SinkError, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};
use rodio::{OutputStream, OutputStreamBuilder, Source, buffer::SamplesBuffer};

/// Input frames on each side of an output frame.
const HALF: usize = 32;
const TAPS: usize = 2 * HALF;
/// Kernels precomputed between two input frames; the ones in between are interpolated.
const PHASES: usize = 256;

pub struct Resampler {
    channels: usize,
    /// Input frames per output frame.
    step: f64,
    /// Where the next output frame falls in `buf`, in input frames.
    pos: f64,
    /// Interleaved input that later output still needs.
    buf: Vec<f32>,
    table: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: u16) -> Self {
        let step = from as f64 / to as f64;
        // A little below the lower of the two Nyquist frequencies, so nothing folds back.
        let cutoff = 0.95 * (1. / step).min(1.);
        let table = (0..=PHASES)
            .flat_map(|p| {
                (0..TAPS).map(move |k| {
                    let x = k as f64 - (HALF - 1) as f64 - p as f64 / PHASES as f64;
                    let sinc = if x == 0. { 1. } else { (PI * cutoff * x).sin() / (PI * cutoff * x) };
                    let u = x / HALF as f64;
                    let blackman = 0.42 + 0.5 * (PI * u).cos() + 0.08 * (2. * PI * u).cos();
                    (cutoff * sinc * blackman) as f32
                })
            })
            .collect();
        let channels = channels.max(1) as usize;
        // Silence before the start, so the first output frame lands on the first input frame.
        Self { channels, step, pos: (HALF - 1) as f64, buf: vec![0.; (HALF - 1) * channels], table }
    }

    /// Converts interleaved `input`, keeping what the next call still needs; a partial frame waits for the rest.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        let c = self.channels;
        let frames = self.buf.len() / c;
        let mut w = [0f32; TAPS];
        while self.pos as usize + HALF < frames {
            let i = self.pos as usize;
            let phase = (self.pos - i as f64) * PHASES as f64;
            let (p, mix) = (phase as usize, phase.fract() as f32);
            let (a, b) = (&self.table[p * TAPS..][..TAPS], &self.table[(p + 1) * TAPS..][..TAPS]);
            for k in 0..TAPS {
                w[k] = a[k] + (b[k] - a[k]) * mix;
            }
            let start = (i + 1 - HALF) * c;
            for ch in 0..c {
                out.push(w.iter().enumerate().map(|(k, w)| w * self.buf[start + k * c + ch]).sum());
            }
            self.pos += self.step;
        }
        let used = (self.pos as usize + 1 - HALF).min(frames);
        self.buf.drain(..used * c);
        self.pos -= used as f64;
    }
}

/// A rodio source played at another rate.
pub struct Resampled<S> {
    inner: S,
    rate: u32,
    resampler: Resampler,
    out: Vec<f32>,
    at: usize,
    done: bool,
}

impl<S: Source> Resampled<S> {
    // shortcut: assumes the source keeps one rate and channel count, true of one decoded file.
    pub fn new(inner: S, rate: u32) -> Self {
        let resampler = Resampler::new(inner.sample_rate(), rate, inner.channels());
        Self { inner, rate, resampler, out: Vec::new(), at: 0, done: false }
    }
}

impl<S: Source> Iterator for Resampled<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        while self.at == self.out.len() {
            if self.done {
                return None;
            }
            const CHUNK: usize = 4096;
            let mut input: Vec<f32> = self.inner.by_ref().take(CHUNK).collect();
            if input.len() < CHUNK {
                // Lets the last input frames through the filter.
                self.done = true;
                input.resize(input.len() + HALF * self.resampler.channels, 0.);
            }
            self.out.clear();
            self.at = 0;
            self.resampler.process(&input, &mut self.out);
        }
        self.at += 1;
        Some(self.out[self.at - 1])
    }
}

impl<S: Source> Source for Resampled<S> {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.inner.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

/// librespot's rodio sink, converting to the device's rate here instead of in rodio.
pub struct Out {
    sink: rodio::Sink,
    _stream: OutputStream,
    resampler: Option<Resampler>,
    rate: u32,
}

impl Out {
    pub fn open() -> Result<Self, rodio::StreamError> {
        let mut stream = OutputStreamBuilder::from_default_device()?.with_sample_rate(SAMPLE_RATE).open_stream_or_fallback()?;
        stream.log_on_drop(false);
        let rate = stream.config().sample_rate();
        let resampler = (rate != SAMPLE_RATE).then(|| Resampler::new(SAMPLE_RATE, rate, NUM_CHANNELS as u16));
        log::info!("Audio device runs at {rate} Hz");
        Ok(Self { sink: rodio::Sink::connect_new(stream.mixer()), _stream: stream, resampler, rate })
    }
}

impl Sink for Out {
    fn start(&mut self) -> SinkResult<()> {
        self.sink.play();
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.sink.sleep_until_end();
        self.sink.pause();
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let samples = packet.samples().map_err(|e| SinkError::OnWrite(e.to_string()))?;
        let mut samples = converter.f64_to_f32(samples);
        if let Some(resampler) = &mut self.resampler {
            let mut out = Vec::with_capacity(samples.len() * 2);
            resampler.process(&samples, &mut out);
            samples = out;
        }
        self.sink.append(SamplesBuffer::new(NUM_CHANNELS as u16, self.rate, samples));
        // About half a second queued, as librespot's own sink keeps.
        while self.sink.len() > 26 {
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f64, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames).flat_map(|i| [(2. * PI * frequency * i as f64 / rate as f64).sin() as f32; 2]).collect()
    }

    #[test]
    fn a_tone_keeps_its_pitch_and_level() {
        let mut resampler = Resampler::new(44100, 48000, 2);
        let mut out = Vec::new();
        // In uneven pieces, as packets come.
        for piece in sine(1000., 44100, 44100).chunks(2 * 333 + 2) {
            resampler.process(piece, &mut out);
        }
        assert!((out.len() as i64 / 2 - 48000).abs() < HALF as i64 * 2);
        let expected = sine(1000., 48000, out.len() / 2);
        let worst = out[2000..out.len() - 2000].iter().zip(&expected[2000..]).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(worst < 1e-3, "off by {worst}");
    }

    #[test]
    fn what_the_device_cannot_play_is_filtered_out() {
        // 30 kHz can't exist at 48 kHz: linear conversion would fold it back to an audible 18 kHz.
        let mut resampler = Resampler::new(96000, 48000, 2);
        let mut out = Vec::new();
        resampler.process(&sine(30000., 96000, 96000), &mut out);
        let peak = out[2000..out.len() - 2000].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.01, "left {peak}");
    }

    #[test]
    fn a_file_plays_to_its_end() {
        let input = sine(440., 44100, 4410);
        let out: Vec<f32> = Resampled::new(SamplesBuffer::new(2, 44100, input), 48000).collect();
        assert_eq!(out.len() % 2, 0);
        assert!((out.len() as i64 / 2 - 4800).abs() <= HALF as i64 + 2, "{} frames", out.len() / 2);
    }
}
