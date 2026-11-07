//! Song decoding (for the waveform) and playback from any position.

use rodio::{OutputStream, OutputStreamHandle, Sink, Source, buffer::SamplesBuffer};
use std::path::Path;
use std::time::Instant;

pub struct Song {
    pub samples: Vec<i16>,
    pub channels: u16,
    pub sample_rate: u32,
    /// Min/max of the mono signal per `PEAK_BLOCK` frames, normalized to -1..=1.
    pub peaks: Vec<(f32, f32)>,
}

pub const PEAK_BLOCK: usize = 256;

impl Song {
    pub fn load(path: &Path) -> Result<Self, String> {
        let file =
            std::fs::File::open(path).map_err(|e| format!("Can't open {}: {e}", path.display()))?;
        let decoder = rodio::Decoder::new(std::io::BufReader::new(file))
            .map_err(|e| format!("Can't decode {}: {e}", path.display()))?;
        let channels = decoder.channels();
        let sample_rate = decoder.sample_rate();
        let samples: Vec<i16> = decoder.collect();

        let ch = usize::from(channels.max(1));
        let peaks = samples
            .chunks(PEAK_BLOCK * ch)
            .map(|block| {
                let (mut lo, mut hi) = (0f32, 0f32);
                for frame in block.chunks(ch) {
                    let v = frame.iter().map(|&s| f32::from(s)).sum::<f32>()
                        / (ch as f32 * f32::from(i16::MAX));
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
                (lo, hi)
            })
            .collect();

        Ok(Self {
            samples,
            channels,
            sample_rate,
            peaks,
        })
    }

    pub fn duration_secs(&self) -> f64 {
        self.samples.len() as f64 / (f64::from(self.sample_rate) * f64::from(self.channels))
    }
}

pub struct Player {
    _stream: OutputStream,
    handle: OutputStreamHandle,
    sink: Option<Sink>,
    started: Option<(Instant, f64)>,
    pub volume: f32,
}

impl Player {
    pub fn new() -> Result<Self, String> {
        let (stream, handle) =
            OutputStream::try_default().map_err(|e| format!("No audio output: {e}"))?;
        Ok(Self {
            _stream: stream,
            handle,
            sink: None,
            started: None,
            volume: 0.6,
        })
    }

    pub fn play(&mut self, song: &Song, from_secs: f64) {
        self.stop();
        let ch = usize::from(song.channels);
        let frame = (from_secs.max(0.0) * f64::from(song.sample_rate)) as usize;
        let start = (frame * ch).min(song.samples.len());
        let Ok(sink) = Sink::try_new(&self.handle) else {
            return;
        };
        sink.set_volume(self.volume);
        sink.append(SamplesBuffer::new(
            song.channels,
            song.sample_rate,
            song.samples[start..].to_vec(),
        ));
        self.sink = Some(sink);
        self.started = Some((Instant::now(), from_secs));
    }

    pub fn stop(&mut self) {
        if let Some(s) = self.sink.take() {
            s.stop();
        }
        self.started = None;
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = v;
        if let Some(s) = &self.sink {
            s.set_volume(v);
        }
    }

    /// Current playback position in seconds, or `None` when stopped.
    pub fn position(&self) -> Option<f64> {
        let (t0, from) = self.started?;
        if self.sink.as_ref().is_some_and(Sink::empty) {
            return None;
        }
        Some(from + t0.elapsed().as_secs_f64())
    }
}
