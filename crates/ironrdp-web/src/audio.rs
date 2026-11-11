//! Audio output ([MS-RDPEA]) for the web client.
//!
//! The RDPSND processor runs inside the session and must be `Send`, so it cannot call
//! JavaScript. [`WebAudioBackend`] passes each wave block to the session's event loop, where
//! [`WasmAudio`] hands it to the page's callbacks:
//!
//! - `wave(data: Uint8Array, sampleRate: number, channels: number): number` receives 16-bit
//!   little-endian PCM, interleaved, and returns how many milliseconds of audio the page
//!   has queued, which the server uses to keep audio and video in step;
//! - `volume(left: number, right: number)` (optional) receives the volume, 0 to 65535;
//! - `close()` (optional) is called when the server stops the audio stream.
//!
//! [MS-RDPEA]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/bea2d5cf-e3b9-4419-92e5-0e074ff9bc5b

use core::time::Duration;
use std::borrow::Cow;
use std::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use futures_channel::mpsc;
use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{AudioFormat, AudioFormatFlags, PitchPdu, VolumePdu, WaveFormat};
use tracing::{debug, warn};
use wasm_bindgen::{JsCast as _, JsValue};

use crate::session::RdpInputEvent;

/// What the backend sends to the event loop.
#[derive(Debug)]
pub(crate) enum AudioMessage {
    Wave { sample_rate: u32, channels: u16, data: Vec<u8> },
    Volume { left: u16, right: u16 },
    Close,
}

/// Creates the RDPSND backend and the part that talks to the page.
///
/// Returns `None` when `callbacks` has no `wave` function.
pub(crate) fn web_audio(
    callbacks: &JsValue,
    events: mpsc::UnboundedSender<RdpInputEvent>,
) -> Option<(WebAudioBackend, WasmAudio)> {
    let function = |name: &str| {
        js_sys::Reflect::get(callbacks, &JsValue::from_str(name))
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
    };
    let wave = function("wave")?;
    let latency_ms = Arc::new(AtomicU32::new(0));
    let backend = WebAudioBackend {
        events,
        latency_ms: Arc::clone(&latency_ms),
        formats: pcm_formats(),
    };
    let audio = WasmAudio {
        this: callbacks.clone(),
        wave,
        volume: function("volume"),
        close: function("close"),
        latency_ms,
    };
    Some((backend, audio))
}

fn pcm(channels: u16, rate: u32) -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: channels,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * u32::from(channels) * 2,
        n_block_align: channels * 2,
        bits_per_sample: 16,
        data: None,
    }
}

/// Uncompressed 16-bit PCM, best first. Web Audio resamples whatever rate arrives.
fn pcm_formats() -> Vec<AudioFormat> {
    vec![
        pcm(2, 48_000),
        pcm(2, 44_100),
        pcm(2, 22_050),
        pcm(1, 44_100),
        pcm(1, 22_050),
        pcm(2, 16_000),
        pcm(1, 16_000),
        pcm(1, 8_000),
    ]
}

fn playable(format: &AudioFormat) -> bool {
    format.format == WaveFormat::PCM
        && format.bits_per_sample == 16
        && (1..=2).contains(&format.n_channels)
        && (8_000..=96_000).contains(&format.n_samples_per_sec)
}

/// The RDPSND backend: hands wave blocks to the event loop.
#[derive(Debug)]
pub(crate) struct WebAudioBackend {
    events: mpsc::UnboundedSender<RdpInputEvent>,
    latency_ms: Arc<AtomicU32>,
    formats: Vec<AudioFormat>,
}

impl WebAudioBackend {
    fn send(&self, message: AudioMessage) {
        if self.events.unbounded_send(RdpInputEvent::Audio(message)).is_err() {
            debug!("Audio after the session ended");
        }
    }
}

impl RdpsndClientHandler for WebAudioBackend {
    fn get_flags(&self) -> AudioFormatFlags {
        AudioFormatFlags::VOLUME
    }

    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_formats(&self, server_formats: &[AudioFormat]) -> Vec<AudioFormat> {
        // Windows lists its formats best first and picks among the client's by quality mode
        // and bandwidth, so advertise every playable one in the server's order.
        let chosen: Vec<AudioFormat> = server_formats.iter().filter(|format| playable(format)).cloned().collect();
        if chosen.is_empty() {
            warn!("The server offers no uncompressed 16-bit audio format");
        }
        chosen
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        if !playable(format) {
            warn!(?format, "Skipping audio in a format the page cannot play");
            return;
        }
        self.send(AudioMessage::Wave {
            sample_rate: format.n_samples_per_sec,
            channels: format.n_channels,
            data: data.into_owned(),
        });
    }

    fn playback_latency(&self) -> Duration {
        Duration::from_millis(u64::from(self.latency_ms.load(Ordering::Relaxed)))
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        self.send(AudioMessage::Volume {
            left: volume.volume_left,
            right: volume.volume_right,
        });
    }

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {
        self.send(AudioMessage::Close);
    }
}

/// The page's audio callbacks.
pub(crate) struct WasmAudio {
    this: JsValue,
    wave: js_sys::Function,
    volume: Option<js_sys::Function>,
    close: Option<js_sys::Function>,
    latency_ms: Arc<AtomicU32>,
}

impl WasmAudio {
    pub(crate) fn process(&self, message: AudioMessage) {
        let result = match message {
            AudioMessage::Wave {
                sample_rate,
                channels,
                data,
            } => self
                .wave
                .call3(
                    &self.this,
                    &js_sys::Uint8Array::from(data.as_slice()),
                    &JsValue::from(sample_rate),
                    &JsValue::from(channels),
                )
                .map(|queued| {
                    if let Some(ms) = queued.as_f64().filter(|ms| ms.is_finite()) {
                        #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let ms = ms.clamp(0.0, 10_000.0) as u32;
                        self.latency_ms.store(ms, Ordering::Relaxed);
                    }
                }),
            AudioMessage::Volume { left, right } => match &self.volume {
                Some(volume) => volume
                    .call2(&self.this, &JsValue::from(left), &JsValue::from(right))
                    .map(drop),
                None => Ok(()),
            },
            AudioMessage::Close => match &self.close {
                Some(close) => close.call0(&self.this).map(drop),
                None => Ok(()),
            },
        };
        if let Err(error) = result {
            warn!(?error, "Audio callback failed");
        }
    }
}

/// Wall-clock time for the RDPSND processor, which reports how long it held each block.
pub(crate) fn clock() -> ironrdp::rdpsnd::client::Clock {
    Box::new(|| Duration::from_secs_f64(js_sys::Date::now() / 1000.0))
}
