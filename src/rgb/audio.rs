//! System audio capture (WASAPI loopback via cpal) and spectrum analysis for the
//! music visualizer effect.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::AudioState;
use super::effects::AudioLevels;

const FFT_N: usize = 1024;
const BANDS: usize = 16;

#[derive(Default)]
struct Ring {
    samples: VecDeque<f32>,
    rate: u32,
}

pub struct AudioCapture {
    enabled: bool,
    stream: Option<cpal::Stream>,
    ring: Arc<Mutex<Ring>>,
    state: AudioState,
    last_try: Option<Instant>,
    gain: f32,
    bass_avg: f32,
    beat: f32,
    smoothed: [f32; BANDS],
    last_levels: Option<Instant>,
    /// Set by the stream's error callback (device unplugged, exclusive mode, ...).
    errored: Arc<AtomicBool>,
    /// Name of the output device being captured, to follow a change of the default device.
    device: String,
    last_device_check: Option<Instant>,
}

/// How often the default output device is compared with the captured one.
const DEVICE_CHECK: Duration = Duration::from_secs(3);

impl Default for AudioCapture {
    fn default() -> Self {
        AudioCapture {
            enabled: false,
            stream: None,
            ring: Arc::new(Mutex::new(Ring::default())),
            state: AudioState::Off,
            last_try: None,
            gain: 1e-3,
            bass_avg: 0.0,
            beat: 0.0,
            smoothed: [0.0; BANDS],
            last_levels: None,
            errored: Arc::new(AtomicBool::new(false)),
            device: String::new(),
            last_device_check: None,
        }
    }
}

impl AudioCapture {
    pub fn state(&self) -> AudioState {
        self.state.clone()
    }

    /// The running stream failed or the default output device changed: start over.
    fn stream_stale(&mut self) -> bool {
        if self.errored.load(Ordering::SeqCst) {
            return true;
        }
        if self.last_device_check.is_some_and(|t| t.elapsed() < DEVICE_CHECK) {
            return false;
        }
        self.last_device_check = Some(Instant::now());
        let now = cpal::default_host().default_output_device().map(|d| device_name(&d));
        now.is_some_and(|n| n != self.device)
    }

    pub fn set_enabled(&mut self, on: bool) {
        if on && self.stream.is_some() && self.stream_stale() {
            log::info!("audio capture: stream failed or the default output device changed; restarting");
            self.stream = None;
            self.last_try = None;
        }
        if on == self.enabled && (self.stream.is_some() || !on) {
            return;
        }
        self.enabled = on;
        if !on {
            self.stream = None;
            self.state = AudioState::Off;
            return;
        }
        if self.last_try.is_some_and(|t| t.elapsed().as_secs() < 10) {
            return;
        }
        self.last_try = Some(Instant::now());
        match self.start() {
            Ok(name) => self.state = AudioState::Active(name),
            Err(e) => {
                log::warn!("audio capture unavailable: {e:#}");
                self.state = AudioState::Unavailable(format!("{e:#}"));
            }
        }
    }

    fn start(&mut self) -> anyhow::Result<String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| anyhow::anyhow!("no audio output device"))?;
        let name = device_name(&device);
        let supported = device.default_output_config()?;
        let config = supported.config();
        let channels = config.channels.max(1) as usize;
        {
            let mut r = self.ring.lock().unwrap();
            r.rate = config.sample_rate;
            r.samples.clear();
        }
        let ring = self.ring.clone();
        let push = move |mono: &mut dyn Iterator<Item = f32>| {
            if let Ok(mut r) = ring.lock() {
                for s in mono {
                    if r.samples.len() >= FFT_N * 2 {
                        r.samples.pop_front();
                    }
                    r.samples.push_back(s);
                }
            }
        };
        self.errored.store(false, Ordering::SeqCst);
        let errored = self.errored.clone();
        let err = move |e: cpal::Error| {
            log::debug!("audio stream error: {e}");
            errored.store(true, Ordering::SeqCst);
        };
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
                config,
                move |data: &[f32], _| {
                    let mut it = data.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32);
                    push(&mut it);
                },
                err,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
                config,
                move |data: &[i16], _| {
                    let mut it = data
                        .chunks(channels)
                        .map(|f| f.iter().map(|s| *s as f32 / 32768.0).sum::<f32>() / channels as f32);
                    push(&mut it);
                },
                err,
                None,
            )?,
            other => anyhow::bail!("unsupported sample format {other:?}"),
        };
        stream.play()?;
        self.stream = Some(stream);
        self.device = name.clone();
        self.last_device_check = Some(Instant::now());
        log::info!("music visualizer listening to '{name}'");
        Ok(name)
    }

    pub fn levels(&mut self) -> Option<AudioLevels> {
        self.stream.as_ref()?;
        let dt = self.last_levels.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.03).clamp(0.001, 0.5);
        self.last_levels = Some(Instant::now());
        let (samples, rate) = {
            let r = self.ring.lock().ok()?;
            if r.samples.len() < FFT_N {
                return Some(AudioLevels::default());
            }
            let start = r.samples.len() - FFT_N;
            (r.samples.iter().skip(start).copied().collect::<Vec<f32>>(), r.rate.max(8000))
        };
        let mags = spectrum(&samples);
        // 16 log-spaced bands between 40 Hz and 16 kHz.
        let mut bands = [0.0f32; BANDS];
        let bin_hz = rate as f32 / FFT_N as f32;
        for (b, band) in bands.iter_mut().enumerate() {
            let lo = 40.0 * (16000.0f32 / 40.0).powf(b as f32 / BANDS as f32);
            let hi = 40.0 * (16000.0f32 / 40.0).powf((b + 1) as f32 / BANDS as f32);
            let (i0, i1) = ((lo / bin_hz) as usize, ((hi / bin_hz) as usize).max((lo / bin_hz) as usize + 1));
            let slice = &mags[i0.min(mags.len() - 1)..i1.min(mags.len())];
            *band = if slice.is_empty() { 0.0 } else { slice.iter().cloned().fold(0.0, f32::max) };
        }
        let peak = bands.iter().cloned().fold(0.0, f32::max);
        self.gain = (self.gain * (1.0 - 0.3 * dt)).max(peak).max(1e-4);
        let mut out = AudioLevels::default();
        for (i, band) in bands.iter().enumerate() {
            let v = (band / self.gain).sqrt().clamp(0.0, 1.0);
            // Fast attack, slower release.
            let k = if v > self.smoothed[i] { 0.6 } else { 1.0 - (-dt / 0.18).exp() };
            self.smoothed[i] += (v - self.smoothed[i]) * k;
            out.bands[i] = self.smoothed[i];
        }
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        out.level = (rms * 4.0).clamp(0.0, 1.0);
        let bass = (out.bands[0] + out.bands[1] + out.bands[2]) / 3.0;
        if bass > self.bass_avg * 1.45 && bass > 0.35 {
            self.beat = 1.0;
        }
        self.bass_avg += (bass - self.bass_avg) * (1.0 - (-dt / 0.6).exp());
        self.beat = (self.beat - dt * 3.0).max(0.0);
        out.beat = self.beat;
        Some(out)
    }
}

fn device_name(d: &cpal::Device) -> String {
    d.description().map(|d| d.to_string()).unwrap_or_else(|_| "Default output".into())
}

/// Magnitude spectrum (first N/2 bins) with a Hann window.
fn spectrum(x: &[f32]) -> Vec<f32> {
    let n = x.len().next_power_of_two().min(FFT_N);
    let mut re: Vec<f32> = (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (n - 1) as f32).cos();
            x.get(i).copied().unwrap_or(0.0) * w
        })
        .collect();
    let mut im = vec![0.0f32; n];
    // Bit-reversal permutation.
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f32::consts::TAU / len as f32;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (s, c) = (ang * k as f32).sin_cos();
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * c - im[b] * s;
                let ti = re[b] * s + im[b] * c;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
    (0..n / 2).map(|i| (re[i] * re[i] + im[i] * im[i]).sqrt()).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn fft_finds_a_tone() {
        let rate = 48000.0;
        let f = 1000.0;
        let x: Vec<f32> = (0..1024).map(|i| (std::f32::consts::TAU * f * i as f32 / rate).sin()).collect();
        let m = super::spectrum(&x);
        let peak = m.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        let hz = peak as f32 * rate / 1024.0;
        assert!((hz - f).abs() < 60.0, "{hz}");
    }
}
