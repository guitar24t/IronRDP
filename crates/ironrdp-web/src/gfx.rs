//! Graphics Pipeline ([MS-RDPEGFX]) for the web client, with H.264 decoded by the browser.
//!
//! Browsers decode H.264 asynchronously (WebCodecs), but the graphics pipeline expects an
//! [`H264Decoder`] that returns each picture straight away. So the pipeline runs outside the
//! session, as [`ironrdp::session::composite_graphics_pipeline`] describes:
//!
//! - [`GfxForwarder`], registered on the dynamic channel in place of the pipeline, queues
//!   each message;
//! - [`GraphicsPipelinePump`] takes the messages in order, decodes the H.264 bitstreams each
//!   one carries with the JavaScript decoder, and only then lets the pipeline process the
//!   message. The pipeline's decoder ([`PrefetchedDecoder`]) hands back those pictures.
//!
//! One message is processed at a time, so the surfaces see the commands in the server's
//! order. The session sends the replies the pump returns and composites the result.
//!
//! The JavaScript decoder factory is called with a surface ID and returns an object with
//! `decode(data: Uint8Array): Promise<Picture>` and `close(): void`, where a picture is the
//! YUV 4:2:0 planes of the picture `data` carries:
//! `{ width, height, y: Uint8Array, u: Uint8Array, v: Uint8Array, yStride, uvStride }`.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_channel::mpsc;
use futures_util::StreamExt as _;
use ironrdp::dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp_core::{Decode as _, ReadCursor, impl_as_any};
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decoder, Yuv420Frame};
use ironrdp_egfx::pdu::{
    Avc420BitmapStream, Avc444BitmapStream, CapabilitiesV8Flags, CapabilitiesV10Flags, CapabilitiesV103Flags,
    CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitySet, Codec1Type, GfxPdu,
};
use ironrdp_pdu::{PduError, PduResult, pdu_other_err};
use tracing::{debug, warn};
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::session::RdpInputEvent;

/// Most H.264 streams decoded at once: the limit `GraphicsPipelineClient` applies to its own
/// decoders (twice the 16 monitors RDP allows), since a server can create surfaces freely.
const MAX_DECODERS: usize = 32;

/// Most bytes of graphics messages waiting for the pump. A server that ignores frame
/// acknowledgements could otherwise grow the queue without limit while pictures decode;
/// past this, the session ends.
const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;

/// After H.264 has failed this long without a picture decoding, ask for a protocol reset.
const RECOVER_AFTER_MS: f64 = 2_000.0;
/// Minimum time between two protocol resets.
const RESET_INTERVAL_MS: f64 = 10_000.0;

/// The pipeline, shared by the forwarder, the pump and the session (which composites it).
#[derive(Clone)]
pub(crate) struct SharedGraphicsPipeline(Arc<Mutex<GraphicsPipelineClient>>);

impl SharedGraphicsPipeline {
    pub(crate) fn lock(&self) -> MutexGuard<'_, GraphicsPipelineClient> {
        // The web client runs on one thread, so nothing can hold the lock across a panic.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The session composites the surfaces itself, so the per-command callbacks are unused.
struct EgfxHandler {
    h264: bool,
}

impl GraphicsPipelineHandler for EgfxHandler {
    fn capabilities(&self) -> Vec<CapabilitySet> {
        if self.h264 {
            return ironrdp_egfx::client::default_capabilities();
        }
        // Without H.264, the version 10 sets with AVC disabled still let the server use
        // RemoteFX Progressive. (The default list only has version 8 left once the AVC
        // sets are filtered out.)
        vec![
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE | CapabilitiesV107Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_6 {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_5 {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_4 {
                flags: CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_3 {
                flags: CapabilitiesV103Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_2 {
                flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
            },
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
            },
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE,
            },
        ]
    }
}

/// Messages queued for [`GraphicsPipelinePump`].
struct QueuedMessage {
    channel_id: u32,
    /// Messages of an earlier channel (before a close or restart) are dropped.
    generation: u32,
    payload: Vec<u8>,
}

/// What [`GraphicsPipelinePump`] hands back to the session after each message.
pub(crate) struct GraphicsPipelineOutput {
    pub(crate) channel_id: u32,
    /// Replies to send on the channel. (In a mutex only because the session's event
    /// channel requires `Sync`.)
    messages: Mutex<Vec<DvcMessage>>,
}

impl GraphicsPipelineOutput {
    pub(crate) fn into_messages(self) -> Vec<DvcMessage> {
        self.messages.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for GraphicsPipelineOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GraphicsPipelineOutput")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

/// Creates the pipeline, the processor to register on the dynamic channel and the pump.
///
/// Without `h264_decoder_factory`, the pipeline advertises no AVC capability set, and
/// the server uses the other codecs (RemoteFX Progressive, ClearCodec, planar).
pub(crate) fn graphics_pipeline(
    h264_decoder_factory: Option<js_sys::Function>,
    events: mpsc::UnboundedSender<RdpInputEvent>,
) -> (SharedGraphicsPipeline, GfxForwarder, GraphicsPipelinePump) {
    let prefetched = PrefetchQueue::default();
    let h264 = h264_decoder_factory.is_some();
    let mut client = GraphicsPipelineClient::new(Box::new(EgfxHandler { h264 }), None);
    if h264 {
        let decoder: Box<dyn H264Decoder> = Box::new(PrefetchedDecoder(prefetched.clone()));
        client = GraphicsPipelineClient::new(Box::new(EgfxHandler { h264 }), Some(decoder));
        // Every surface draws from the same queue, so one decoder per surface is just a view of it.
        let queue = prefetched.clone();
        client = client.with_h264_decoder_factory(Box::new(move || {
            let decoder: Box<dyn H264Decoder> = Box::new(PrefetchedDecoder(queue.clone()));
            Some(decoder)
        }));
    }

    let pipeline = SharedGraphicsPipeline(Arc::new(Mutex::new(client)));
    let generation = Arc::new(AtomicU32::new(0));
    let (tx, rx) = mpsc::unbounded();
    let queued_bytes = Arc::new(AtomicUsize::new(0));

    let forwarder = GfxForwarder {
        pipeline: pipeline.clone(),
        generation: Arc::clone(&generation),
        tx,
        queued_bytes: Arc::clone(&queued_bytes),
    };
    let pump = GraphicsPipelinePump {
        pipeline: pipeline.clone(),
        generation,
        rx,
        queued_bytes,
        prefetched,
        decoder_factory: h264_decoder_factory,
        decoders: BTreeMap::new(),
        events,
        failing_since: None,
        last_reset_request: None,
    };
    (pipeline, forwarder, pump)
}

// ============================================================================
// Forwarder
// ============================================================================

/// Stands in for the pipeline on the dynamic channel and queues its messages.
pub(crate) struct GfxForwarder {
    pipeline: SharedGraphicsPipeline,
    generation: Arc<AtomicU32>,
    tx: mpsc::UnboundedSender<QueuedMessage>,
    /// Bytes queued for the pump and not yet processed.
    queued_bytes: Arc<AtomicUsize>,
}

impl_as_any!(GfxForwarder);

impl DvcProcessor for GfxForwarder {
    fn channel_name(&self) -> &str {
        ironrdp_egfx::CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // Anything still queued belongs to the previous channel.
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.pipeline.lock().start(channel_id)
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let queued = self.queued_bytes.fetch_add(payload.len(), Ordering::Relaxed) + payload.len();
        if queued > MAX_QUEUED_BYTES {
            return Err(pdu_other_err!("the server sends graphics faster than they can be decoded"));
        }
        let message = QueuedMessage {
            channel_id,
            generation: self.generation.load(Ordering::Relaxed),
            payload: payload.to_vec(),
        };
        self.tx
            .unbounded_send(message)
            .map_err(|_| pdu_other_err!("the graphics pipeline has stopped"))?;
        Ok(Vec::new())
    }

    fn close(&mut self, channel_id: u32) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.pipeline.lock().close(channel_id);
    }
}

impl DvcClientProcessor for GfxForwarder {}

// ============================================================================
// Decoder side of the pipeline
// ============================================================================

/// A picture decoded ahead of the PDU that needs it.
struct Prefetched {
    /// Identifies the bitstream the picture came from.
    fingerprint: u64,
    picture: Result<Yuv420Frame, String>,
}

#[derive(Clone, Default)]
struct PrefetchQueue(Arc<Mutex<VecDeque<Prefetched>>>);

impl PrefetchQueue {
    fn lock(&self) -> MutexGuard<'_, VecDeque<Prefetched>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// FNV-1a over the bitstream: tells apart the bitstreams of consecutive updates.
fn fingerprint(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash ^ u64::try_from(data.len()).unwrap_or(u64::MAX)
}

/// The pipeline's H.264 decoder: returns the pictures [`GraphicsPipelinePump`] decoded for
/// the PDU being processed, in the order the pipeline asks for them.
struct PrefetchedDecoder(PrefetchQueue);

impl H264Decoder for PrefetchedDecoder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.decode_yuv420(data).map(|picture| picture.to_rgba())
    }

    fn decode_yuv420(&mut self, data: &[u8]) -> DecoderResult<Yuv420Frame> {
        let mut queue = self.0.lock();
        match queue.pop_front() {
            Some(prefetched) if prefetched.fingerprint == fingerprint(data) => {
                prefetched.picture.map_err(DecoderError::msg)
            }
            Some(_) => {
                queue.clear();
                Err(DecoderError::msg("the decoded picture is not the one requested"))
            }
            None => Err(DecoderError::msg("no decoded picture for this bitstream")),
        }
    }

    fn supports_yuv420(&self) -> bool {
        true
    }
}

// ============================================================================
// Pump
// ============================================================================

/// Processes the queued messages, decoding H.264 first. See the module documentation.
pub(crate) struct GraphicsPipelinePump {
    pipeline: SharedGraphicsPipeline,
    generation: Arc<AtomicU32>,
    rx: mpsc::UnboundedReceiver<QueuedMessage>,
    queued_bytes: Arc<AtomicUsize>,
    prefetched: PrefetchQueue,
    decoder_factory: Option<js_sys::Function>,
    /// The JavaScript decoder of each surface's H.264 stream.
    decoders: BTreeMap<u16, JsValue>,
    events: mpsc::UnboundedSender<RdpInputEvent>,
    /// When H.264 started failing, if it has not decoded a picture since.
    failing_since: Option<f64>,
    last_reset_request: Option<f64>,
}

impl GraphicsPipelinePump {
    pub(crate) async fn run(mut self) {
        while let Some(message) = self.rx.next().await {
            self.queued_bytes.fetch_sub(message.payload.len(), Ordering::Relaxed);
            if message.generation != self.generation.load(Ordering::Relaxed) {
                continue;
            }
            let event = match self.process(message.generation, &message.payload).await {
                Ok(messages) => RdpInputEvent::GraphicsPipeline(GraphicsPipelineOutput {
                    channel_id: message.channel_id,
                    messages: Mutex::new(messages),
                }),
                Err(error) => RdpInputEvent::GraphicsPipelineError(error),
            };
            if self.events.unbounded_send(event).is_err() {
                break;
            }
        }
        self.close_decoders();
        debug!("Graphics pipeline stopped");
    }

    async fn process(&mut self, generation: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let pdus = self.pipeline.lock().decode_messages(payload)?;
        let mut messages = Vec::new();
        for pdu in pdus {
            self.prepare(&pdu).await;
            if generation != self.generation.load(Ordering::Relaxed) {
                // The channel closed or restarted while a picture was decoding.
                return Ok(Vec::new());
            }
            let result = self.pipeline.lock().process_pdu(pdu);
            // Pictures the PDU did not use (for example, for an unknown surface) are stale.
            self.prefetched.lock().clear();
            messages.extend(result?);
        }
        messages.extend(self.recover());
        Ok(messages)
    }

    /// Ends the H.264 streams the PDU ends and decodes the bitstreams it carries.
    async fn prepare(&mut self, pdu: &GfxPdu) {
        match pdu {
            GfxPdu::CapabilitiesConfirm(_) | GfxPdu::ResetGraphics(_) => self.close_decoders(),
            GfxPdu::CreateSurface(pdu) => self.close_decoder(pdu.surface_id),
            GfxPdu::DeleteSurface(pdu) => self.close_decoder(pdu.surface_id),
            GfxPdu::WireToSurface1(pdu) => match pdu.codec_id {
                Codec1Type::Avc420 => {
                    if let Ok(stream) = Avc420BitmapStream::decode(&mut ReadCursor::new(&pdu.bitmap_data)) {
                        self.prefetch(pdu.surface_id, stream.data).await;
                    }
                }
                Codec1Type::Avc444 | Codec1Type::Avc444v2 => {
                    // The pipeline decodes the first sub-stream, then the second if there is one.
                    if let Ok(stream) = Avc444BitmapStream::decode(&mut ReadCursor::new(&pdu.bitmap_data)) {
                        self.prefetch(pdu.surface_id, stream.stream1.data).await;
                        if let Some(stream2) = &stream.stream2 {
                            self.prefetch(pdu.surface_id, stream2.data).await;
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    async fn prefetch(&mut self, surface_id: u16, data: &[u8]) {
        // The pipeline drops updates for surfaces it does not know: decode nothing for them.
        if self.pipeline.lock().get_surface(surface_id).is_none() {
            return;
        }
        let picture = self.decode(surface_id, data).await;
        match &picture {
            Ok(_) => self.failing_since = None,
            // Warn once per run of failures: they can come with every picture until a key frame.
            Err(error) if self.failing_since.is_none() => {
                warn!(surface_id, %error, "H.264 decoding failed");
                self.failing_since = Some(js_sys::Date::now());
            }
            Err(error) => debug!(surface_id, %error, "H.264 decoding failed"),
        }
        self.prefetched.lock().push_back(Prefetched {
            fingerprint: fingerprint(data),
            picture,
        });
    }

    async fn decode(&mut self, surface_id: u16, data: &[u8]) -> Result<Yuv420Frame, String> {
        let decoder = self.decoder(surface_id)?;
        let decode = js_sys::Reflect::get(&decoder, &JsValue::from_str("decode"))
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
            .ok_or("the H.264 decoder has no decode function")?;
        let returned = decode
            .call1(&decoder, &js_sys::Uint8Array::from(data))
            .map_err(js_error)?;
        let picture = JsFuture::from(js_sys::Promise::resolve(&returned))
            .await
            .map_err(js_error)?;
        picture_from_js(&picture)
    }

    fn decoder(&mut self, surface_id: u16) -> Result<JsValue, String> {
        if let Some(decoder) = self.decoders.get(&surface_id) {
            return Ok(decoder.clone());
        }
        if MAX_DECODERS <= self.decoders.len() {
            return Err("too many surfaces sending H.264".to_owned());
        }
        let factory = self.decoder_factory.as_ref().ok_or("no H.264 decoder")?;
        let decoder = factory
            .call1(&JsValue::NULL, &JsValue::from(surface_id))
            .map_err(js_error)?;
        if !decoder.is_object() {
            return Err("the H.264 decoder factory returned no decoder".to_owned());
        }
        self.decoders.insert(surface_id, decoder.clone());
        Ok(decoder)
    }

    fn close_decoder(&mut self, surface_id: u16) {
        if let Some(decoder) = self.decoders.remove(&surface_id) {
            close_js_decoder(&decoder);
        }
    }

    fn close_decoders(&mut self) {
        for decoder in core::mem::take(&mut self.decoders).values() {
            close_js_decoder(decoder);
        }
    }

    /// When H.264 keeps failing, a decoder may be waiting for a key frame the server does
    /// not plan to send. A protocol reset (graphics versions 10.3 to 10.7) makes the server
    /// recreate its surfaces, which starts new streams.
    fn recover(&mut self) -> Option<DvcMessage> {
        let since = self.failing_since?;
        let now = js_sys::Date::now();
        if now - since < RECOVER_AFTER_MS
            || self
                .last_reset_request
                .is_some_and(|last| now - last < RESET_INTERVAL_MS)
        {
            return None;
        }
        self.last_reset_request = Some(now);
        self.failing_since = None;
        let reset = self.pipeline.lock().request_protocol_reset();
        match reset {
            Some(pdu) => {
                warn!("H.264 keeps failing; asking the server to reset the graphics pipeline");
                self.close_decoders();
                let message: DvcMessage = Box::new(pdu);
                Some(message)
            }
            None => {
                warn!("H.264 keeps failing, and the negotiated graphics version cannot reset");
                None
            }
        }
    }
}

fn close_js_decoder(decoder: &JsValue) {
    if let Some(close) = js_sys::Reflect::get(decoder, &JsValue::from_str("close"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
    {
        if let Err(error) = close.call0(decoder) {
            debug!(error = js_error(error), "Closing an H.264 decoder failed");
        }
    }
}

fn js_error(value: JsValue) -> String {
    value
        .dyn_ref::<js_sys::Error>()
        .map(|error| String::from(error.message()))
        .or_else(|| value.as_string())
        .unwrap_or_else(|| format!("{value:?}"))
}

fn picture_from_js(picture: &JsValue) -> Result<Yuv420Frame, String> {
    let number = |key: &str| -> Result<f64, String> {
        js_sys::Reflect::get(picture, &JsValue::from_str(key))
            .ok()
            .and_then(|value| value.as_f64())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .ok_or_else(|| format!("picture.{key} is not a number"))
    };
    let plane = |key: &str| -> Result<Vec<u8>, String> {
        js_sys::Reflect::get(picture, &JsValue::from_str(key))
            .ok()
            .and_then(|value| value.dyn_into::<js_sys::Uint8Array>().ok())
            .map(|array| array.to_vec())
            .ok_or_else(|| format!("picture.{key} is not a Uint8Array"))
    };
    let dimension = |key: &str| -> Result<u16, String> {
        let value = number(key)?;
        u16::try_from(f64_to_u32(value)).map_err(|_| format!("picture.{key} is too large"))
    };
    let stride = |key: &str| -> Result<usize, String> {
        usize::try_from(f64_to_u32(number(key)?)).map_err(|_| format!("picture.{key} is too large"))
    };

    Yuv420Frame::new(
        dimension("width")?,
        dimension("height")?,
        plane("y")?,
        stride("yStride")?,
        plane("u")?,
        plane("v")?,
        stride("uvStride")?,
    )
    .map_err(|error| error.to_string())
}

fn f64_to_u32(value: f64) -> u32 {
    #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        value.clamp(0.0, f64::from(u32::MAX)) as u32
    }
}

/// Fatal pipeline errors become session errors.
pub(crate) fn pipeline_error(error: PduError) -> anyhow::Error {
    anyhow::anyhow!("graphics pipeline: {error}")
}
