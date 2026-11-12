//! Audio output ([MS-RDPEA]) for the web client.
//!
//! The RDPSND processor runs inside the session and must be `Send`, so it cannot call
//! JavaScript. [`WebAudioBackend`] passes each wave block to the session's event loop, where
//! [`WasmAudio`] hands it to the page's callbacks:
//!
//! - `wave(data: Uint8Array, sampleRate: number, channels: number): number` receives 16-bit
//!   little-endian PCM, interleaved, and returns how many milliseconds of audio the page
//!   has queued, which the server uses to keep audio and video in step;
//! - `encoded(codec: string, data: Uint8Array, sampleRate: number, channels: number): number`
//!   (optional) receives one frame of a compressed format the page decodes itself, listed in
//!   `codecs` (`"aac"`: AAC-LC, raw or ADTS, as Windows sends `WAVE_FORMAT_AAC_MS`), and
//!   returns the queued milliseconds like `wave`;
//! - `codecs: string[]` (optional) lists the compressed formats `encoded` can decode;
//! - `volume(left: number, right: number)` (optional) receives the volume, 0 to 65535;
//! - `close()` (optional) is called when the server stops the audio stream.
//!
//! Compressed sound is preferred, to save bandwidth: AAC when the page decodes it (about
//! 100 kbit/s), otherwise ADPCM, decoded here (a quarter of PCM). Uncompressed PCM is used
//! only when the server offers neither.
//!
//! [MS-RDPEA]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/bea2d5cf-e3b9-4419-92e5-0e074ff9bc5b

use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;
use std::borrow::Cow;
use std::sync::Arc;

use futures_channel::mpsc;
use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::codec::AudioDecoder;
use ironrdp::rdpsnd::pdu::{AudioFormat, AudioFormatFlags, PitchPdu, VolumePdu, WaveFormat};
use tracing::{debug, warn};
use wasm_bindgen::{JsCast as _, JsValue};

use crate::session::RdpInputEvent;

/// The compressed format the page can decode itself.
const AAC: &str = "aac";

/// What the backend sends to the event loop.
#[derive(Debug)]
pub(crate) enum AudioMessage {
    /// 16-bit little-endian PCM, interleaved.
    Wave {
        sample_rate: u32,
        channels: u16,
        data: Vec<u8>,
    },
    /// One frame for the page's decoder.
    Encoded {
        codec: &'static str,
        sample_rate: u32,
        channels: u16,
        data: Vec<u8>,
    },
    Volume {
        left: u16,
        right: u16,
    },
    Close,
}

/// Creates the RDPSND backend and the part that talks to the page.
///
/// Returns `None` when `callbacks` has no `wave` function.
pub(crate) fn web_audio(
    callbacks: &JsValue,
    events: mpsc::UnboundedSender<RdpInputEvent>,
) -> Option<(WebAudioBackend, WasmAudio)> {
    let property = |name: &str| js_sys::Reflect::get(callbacks, &JsValue::from_str(name)).ok();
    let function = |name: &str| property(name).and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    let wave = function("wave")?;
    let encoded = function("encoded");
    let aac = encoded.is_some()
        && property("codecs")
            .and_then(|codecs| codecs.dyn_into::<js_sys::Array>().ok())
            .is_some_and(|codecs| codecs.iter().any(|codec| codec.as_string().as_deref() == Some(AAC)));
    let latency_ms = Arc::new(AtomicU32::new(0));
    let backend = WebAudioBackend {
        events,
        latency_ms: Arc::clone(&latency_ms),
        formats: pcm_formats(),
        aac,
        decoder: None,
    };
    let audio = WasmAudio {
        this: callbacks.clone(),
        wave,
        encoded,
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

/// A channel count and rate the page plays (and that sizes no buffer from hostile values).
fn sane_layout(format: &AudioFormat) -> bool {
    (1..=2).contains(&format.n_channels) && (8_000..=96_000).contains(&format.n_samples_per_sec)
}

fn is_pcm16(format: &AudioFormat) -> bool {
    format.format == WaveFormat::PCM && format.bits_per_sample == 16
}

/// How the page gets a format's audio, if it can play it at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Route {
    /// Decoded by the page (AAC). Preferred: the smallest.
    Page,
    /// Decoded here (ADPCM), good quality: stereo at 32 kHz or more.
    Decoded,
    /// Uncompressed 16-bit PCM.
    Pcm,
    /// Decoded here, but low quality (A-law, mu-law, low rates): only when nothing else is offered.
    LowQuality,
}

/// The RDPSND backend: hands wave blocks to the event loop.
#[derive(Debug)]
pub(crate) struct WebAudioBackend {
    events: mpsc::UnboundedSender<RdpInputEvent>,
    latency_ms: Arc<AtomicU32>,
    formats: Vec<AudioFormat>,
    /// Whether the page decodes AAC.
    aac: bool,
    /// The decoder of the last format decoded here.
    decoder: Option<(AudioFormat, AudioDecoder)>,
}

impl WebAudioBackend {
    fn send(&self, message: AudioMessage) {
        if self.events.unbounded_send(RdpInputEvent::Audio(message)).is_err() {
            debug!("Audio after the session ended");
        }
    }

    fn route(&self, format: &AudioFormat) -> Option<Route> {
        if !sane_layout(format) {
            return None;
        }
        if format.format == WaveFormat::AAC_MS {
            // The page's decoder (WebCodecs) outputs at most 48 kHz.
            return (self.aac && format.n_samples_per_sec <= 48_000).then_some(Route::Page);
        }
        if is_pcm16(format) {
            return Some(Route::Pcm);
        }
        if !AudioDecoder::supports(format) {
            return None;
        }
        let adpcm = matches!(format.format, WaveFormat::ADPCM | WaveFormat::DVI_ADPCM);
        Some(if adpcm && format.n_channels == 2 && format.n_samples_per_sec >= 32_000 {
            Route::Decoded
        } else {
            Route::LowQuality
        })
    }

    /// Decodes a format the page cannot play into 16-bit PCM.
    fn decode(&mut self, format: &AudioFormat, data: &[u8]) -> Option<Vec<u8>> {
        if self.decoder.as_ref().is_none_or(|(current, _)| current != format) {
            match AudioDecoder::new(format) {
                Ok(decoder) => self.decoder = Some((format.clone(), decoder)),
                Err(error) => {
                    warn!(%error, ?format, "No decoder for the audio format");
                    self.decoder = None;
                    return None;
                }
            }
        }
        let (_, decoder) = self.decoder.as_mut()?;
        let mut samples = Vec::new();
        if let Err(error) = decoder.decode(data, &mut samples) {
            // The blocks decoded before the error still play.
            debug!(%error, "Audio decoding failed");
        }
        Some(samples.iter().flat_map(|sample| sample.to_le_bytes()).collect())
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
        // Advertise only the best kind of format the server offers, in the server's order
        // (Windows lists its formats best first): what is advertised is what the server
        // chooses from, so this is how compressed sound is preferred.
        let routed: Vec<(Route, &AudioFormat)> = server_formats
            .iter()
            .filter_map(|format| self.route(format).map(|route| (route, format)))
            .collect();
        let Some(best) = routed.iter().map(|(route, _)| *route).min() else {
            warn!("The server offers no audio format the page can play");
            return Vec::new();
        };
        let chosen: Vec<AudioFormat> = routed
            .into_iter()
            .filter(|(route, _)| *route == best)
            .map(|(_, format)| format.clone())
            .collect();
        debug!(?best, formats = chosen.len(), "Chose audio formats");
        chosen
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        let message = match self.route(format) {
            Some(Route::Page) => AudioMessage::Encoded {
                codec: AAC,
                sample_rate: format.n_samples_per_sec,
                channels: format.n_channels,
                data: data.into_owned(),
            },
            Some(Route::Pcm) => AudioMessage::Wave {
                sample_rate: format.n_samples_per_sec,
                channels: format.n_channels,
                data: data.into_owned(),
            },
            Some(Route::Decoded | Route::LowQuality) => match self.decode(format, &data) {
                Some(pcm) => AudioMessage::Wave {
                    sample_rate: format.n_samples_per_sec,
                    channels: format.n_channels,
                    data: pcm,
                },
                None => return,
            },
            None => {
                warn!(?format, "Skipping audio in a format the page cannot play");
                return;
            }
        };
        self.send(message);
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
        self.decoder = None;
        self.send(AudioMessage::Close);
    }
}

/// The page's audio callbacks.
pub(crate) struct WasmAudio {
    this: JsValue,
    wave: js_sys::Function,
    encoded: Option<js_sys::Function>,
    volume: Option<js_sys::Function>,
    close: Option<js_sys::Function>,
    latency_ms: Arc<AtomicU32>,
}

impl WasmAudio {
    fn store_latency(&self, queued: &JsValue) {
        if let Some(ms) = queued.as_f64().filter(|ms| ms.is_finite()) {
            #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ms = ms.clamp(0.0, 10_000.0) as u32;
            self.latency_ms.store(ms, Ordering::Relaxed);
        }
    }

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
                .map(|queued| self.store_latency(&queued)),
            AudioMessage::Encoded {
                codec,
                sample_rate,
                channels,
                data,
            } => match &self.encoded {
                Some(encoded) => {
                    let args = js_sys::Array::of4(
                        &JsValue::from_str(codec),
                        &js_sys::Uint8Array::from(data.as_slice()),
                        &JsValue::from(sample_rate),
                        &JsValue::from(channels),
                    );
                    encoded
                        .apply(&self.this, &args)
                        .map(|queued| self.store_latency(&queued))
                }
                None => Ok(()),
            },
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
