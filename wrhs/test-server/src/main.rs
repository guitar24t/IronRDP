//! RDP test server for web clients: streams a moving test pattern and a tone.
//!
//! - Picture: over the Graphics Pipeline as H.264 (AVC420) when the client decodes it,
//!   as uncompressed updates when it supports the pipeline without H.264, and as plain
//!   bitmap updates without the pipeline.
//! - Sound: a 440 Hz tone, 16-bit PCM over RDPSND, when the client asks for audio.
//!
//! The pattern is the left half red, the right half blue and a white square moving to
//! the right along the top, so a test can check the colors and see the picture move.
//!
//! Usage: wrhs-av-test-server --bind 127.0.0.1:3390 --cert server.crt --key server.key
//!        [--sec tls|hybrid] [--user user] [--pass pass] [--size 1024x768] [--fps 30]

use core::net::SocketAddr;
use core::num::{NonZeroU16, NonZeroUsize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, bail};
use ironrdp_connector::DesktopSize;
use ironrdp_egfx::pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet};
use ironrdp_egfx::server::GraphicsPipelineHandler;
use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler, RdpsndServerMessage};
use ironrdp_server::{
    BitmapUpdate, Credentials, DisplayContext, DisplayUpdate, EgfxServerMessage, GfxContext, GfxServerFactory,
    GfxServerHandle, KeyboardEvent, MouseEvent, PixelFormat, RdpServer, RdpServerDisplay, RdpServerDisplayUpdates,
    RdpServerInputHandler, ServerEvent, ServerEventSender, ServerResult, SoundServerFactory, TlsIdentityCtx,
};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Duration, Instant, interval, sleep_until};
use tracing::{info, warn};

const BOX: usize = 64;

/// Desktop size and milliseconds per picture (`--size`, `--fps`).
static PICTURE: std::sync::OnceLock<(u16, u16, u64)> = std::sync::OnceLock::new();

fn width() -> u16 {
    PICTURE.get().map_or(1024, |p| p.0)
}

fn height() -> u16 {
    PICTURE.get().map_or(768, |p| p.1)
}

fn frame_ms() -> u64 {
    PICTURE.get().map_or(33, |p| p.2)
}

type Events = Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>;

fn send(events: &Events, event: ServerEvent) {
    if let Some(sender) = events.lock().expect("poisoned").as_ref() {
        let _ = sender.send(event);
    }
}

/// Picture `n` of the test pattern, RGBA.
fn pattern(n: u64) -> Vec<u8> {
    let (w, h) = (usize::from(width()), usize::from(height()));
    let box_x = usize::try_from(n * 8).expect("fits") % (w - BOX);
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let px = &mut rgba[(y * w + x) * 4..][..4];
            let in_box = (box_x..box_x + BOX).contains(&x) && (32..32 + BOX).contains(&y);
            let color = if in_box {
                [255, 255, 255]
            } else if x < w / 2 {
                [200, 30, 30]
            } else {
                [30, 30, 200]
            };
            px.copy_from_slice(&[color[0], color[1], color[2], 255]);
        }
    }
    rgba
}

fn clamp(v: i32) -> u8 {
    u8::try_from(v.clamp(0, 255)).expect("clamped")
}

/// RGBA to YUV 4:2:0 planes with the forward transform of MS-RDPEGFX 3.3.8.3.1
/// ("Color Conversion"), which is what a client converts back with.
fn rgba_to_yuv420(rgba: &[u8], w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let px = |x: usize, y: usize| {
        let p = &rgba[(y * w + x) * 4..][..3];
        (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]))
    };
    let mut luma = vec![0; w * h];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = px(x, y);
            luma[y * w + x] = clamp((54 * r + 183 * g + 18 * b) >> 8);
        }
    }
    let (cw, ch) = (w / 2, h / 2);
    let (mut u, mut v) = (vec![0; cw * ch], vec![0; cw * ch]);
    for y in 0..ch {
        for x in 0..cw {
            let (mut su, mut sv) = (0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (r, g, b) = px(2 * x + dx, 2 * y + dy);
                su += ((-29 * r - 99 * g + 128 * b) >> 8) + 128;
                sv += ((128 * r - 116 * g - 12 * b) >> 8) + 128;
            }
            u[y * cw + x] = clamp((su + 2) / 4);
            v[y * cw + x] = clamp((sv + 2) / 4);
        }
    }
    (luma, u, v)
}

fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect()
}

// ---------------------------------------------------------------- display

#[derive(Clone)]
struct Display {
    events: Events,
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: width(),
            height: height(),
        }
    }

    async fn updates(&mut self, ctx: DisplayContext) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        info!(gfx = ctx.gfx_handle.is_some(), "display updates requested");
        Ok(Box::new(Updates {
            gfx: ctx.gfx_handle,
            events: Arc::clone(&self.events),
            frame: 0,
            next_frame: Instant::now(),
            surface: None,
            encoder: None,
        }))
    }
}

struct Updates {
    gfx: Option<GfxServerHandle>,
    events: Events,
    frame: u64,
    /// The server drops and re-creates the update future whenever another event comes
    /// first (audio arrives every 20 ms), so the frame clock is a deadline kept here.
    next_frame: Instant,
    surface: Option<u16>,
    encoder: Option<openh264::encoder::Encoder>,
}

impl Updates {
    /// Sends one picture over the Graphics Pipeline. Returns false when the pipeline
    /// is not (yet) ready.
    fn send_gfx(&mut self) -> anyhow::Result<bool> {
        let Some(gfx) = &self.gfx else { return Ok(false) };
        let mut server = gfx.lock().expect("poisoned");
        if !server.is_ready() {
            if self.frame % 30 == 0 {
                info!(frame = self.frame, "graphics pipeline not ready");
            }
            return Ok(false);
        }
        let surface = match self.surface {
            Some(surface) => surface,
            None => {
                server.set_output_dimensions(width(), height());
                let surface = server.create_surface(width(), height()).context("create surface")?;
                server.map_surface_to_output(surface, 0, 0);
                if server.supports_avc420() {
                    self.encoder = Some(openh264::encoder::Encoder::new().context("H.264 encoder")?);
                }
                info!(h264 = self.encoder.is_some(), "graphics pipeline ready");
                println!("graphics-pipeline h264={}", self.encoder.is_some());
                self.surface = Some(surface);
                surface
            }
        };
        // Skipping a frame after encoding it would break the H.264 reference chain.
        if !server.should_backpressure() {
            let rgba = pattern(self.frame);
            let ts = u32::try_from((self.frame * frame_ms()) % u64::from(u32::MAX)).expect("fits");
            if let Some(encoder) = self.encoder.as_mut() {
                let (w, h) = (usize::from(width()), usize::from(height()));
                let (y, u, v) = rgba_to_yuv420(&rgba, w, h);
                let yuv = openh264::formats::YUVSlices::new((&y, &u, &v), (w, h), (w, w / 2, w / 2));
                let data = encoder.encode(&yuv).context("encode")?.to_vec();
                if !data.is_empty()
                    && server
                        .send_avc420_frame(surface, &data, &[Avc420Region::full_frame(width(), height(), 22)], ts)
                        .is_none()
                {
                    encoder.force_intra_frame();
                }
            } else {
                server.send_uncompressed_frame(surface, &rgba_to_bgra(&rgba), width(), height(), ts);
            }
        }
        let channel_id = server.channel_id();
        let messages = server.drain_output();
        drop(server);
        if let Some(channel_id) = channel_id
            && !messages.is_empty()
        {
            let messages = ironrdp_dvc::encode_dvc_messages(channel_id, messages, ChannelFlags::SHOW_PROTOCOL)
                .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
            send(
                &self.events,
                ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages }),
            );
        }
        Ok(true)
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        loop {
            sleep_until(self.next_frame).await;
            self.next_frame = (self.next_frame + Duration::from_millis(frame_ms())).max(Instant::now());
            self.frame += 1;
            match self.send_gfx() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => warn!(%error, "graphics pipeline frame failed"),
            }
            // Give the pipeline two seconds to come up, then fall back to bitmaps.
            if self.gfx.is_some() && self.frame < 60 || self.frame % 3 != 0 {
                continue;
            }
            return Ok(Some(DisplayUpdate::Bitmap(BitmapUpdate {
                x: 0,
                y: 0,
                width: NonZeroU16::new(width()).expect("non-zero"),
                height: NonZeroU16::new(height()).expect("non-zero"),
                format: PixelFormat::BgrA32,
                data: rgba_to_bgra(&pattern(self.frame)).into(),
                stride: NonZeroUsize::new(usize::from(width()) * 4).expect("non-zero"),
            })));
        }
    }
}

struct Gfx {
    events: Events,
}

struct GfxHandler;

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, _pdu: &CapabilitiesAdvertisePdu) {}

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        info!(?negotiated, "graphics pipeline negotiated");
    }
}

impl ServerEventSender for Gfx {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.events.lock().expect("poisoned") = Some(sender);
    }
}

impl GfxServerFactory for Gfx {
    fn build_gfx_handler(&self, _ctx: GfxContext) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GfxHandler)
    }
}

// ---------------------------------------------------------------- input

#[derive(Clone)]
struct Input;

impl RdpServerInputHandler for Input {
    fn keyboard(&mut self, _event: KeyboardEvent) {}
    fn mouse(&mut self, _event: MouseEvent) {}
}

// ---------------------------------------------------------------- sound

struct Sound {
    events: Events,
}

impl ServerEventSender for Sound {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.events.lock().expect("poisoned") = Some(sender);
    }
}

impl SoundServerFactory for Sound {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(Tone {
            events: Arc::clone(&self.events),
            task: None,
        })
    }
}

#[derive(Debug)]
struct Tone {
    events: Events,
    task: Option<tokio::task::JoinHandle<()>>,
}

const FORMATS: [AudioFormat; 2] = [
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: 2,
        n_samples_per_sec: 48_000,
        n_avg_bytes_per_sec: 192_000,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
    },
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: 2,
        n_samples_per_sec: 44_100,
        n_avg_bytes_per_sec: 176_400,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
    },
];

impl RdpsndServerHandler for Tone {
    fn get_formats(&self) -> &[AudioFormat] {
        &FORMATS
    }

    fn choose_format<'a>(&mut self, common: &'a [NegotiatedFormat]) -> Option<&'a NegotiatedFormat> {
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let rate = format.format().n_samples_per_sec;
        println!("audio-start rate={rate}");
        let events = Arc::clone(&self.events);
        self.task = Some(tokio::spawn(async move {
            let mut tick = interval(Duration::from_millis(20));
            let mut ts: u16 = 0;
            let mut phase = 0.0f64;
            let step = 2.0 * core::f64::consts::PI * 440.0 / f64::from(rate);
            loop {
                tick.tick().await;
                let mut data = Vec::new();
                for _ in 0..rate / 50 {
                    #[expect(clippy::cast_possible_truncation, clippy::as_conversions)]
                    let sample = (phase.sin() * 0.3 * 32767.0) as i16;
                    phase = (phase + step) % (2.0 * core::f64::consts::PI);
                    data.extend_from_slice(&sample.to_le_bytes());
                    data.extend_from_slice(&sample.to_le_bytes());
                }
                send(
                    &events,
                    ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, u32::from(ts))),
                );
                ts = ts.wrapping_add(20);
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------- main

struct Args {
    bind: SocketAddr,
    cert: PathBuf,
    key: PathBuf,
    hybrid: bool,
    user: String,
    pass: String,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut bind = None;
    let mut cert = None;
    let mut key = None;
    let mut hybrid = false;
    let mut user = "user".to_owned();
    let mut pass = "pass".to_owned();
    let (mut w, mut h, mut fps) = (1024u16, 768u16, 30u64);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--bind" => bind = Some(value()?.parse()?),
            "--cert" => cert = Some(PathBuf::from(value()?)),
            "--key" => key = Some(PathBuf::from(value()?)),
            "--sec" => {
                hybrid = match value()?.as_str() {
                    "tls" => false,
                    "hybrid" => true,
                    other => bail!("unknown security {other}"),
                }
            }
            "--user" => user = value()?,
            "--pass" => pass = value()?,
            "--size" => {
                let size = value()?;
                let (a, b) = size.split_once('x').context("--size WxH")?;
                (w, h) = (a.parse()?, b.parse()?);
                if w % 16 != 0 || h % 2 != 0 {
                    bail!("--size: the width must be a multiple of 16 and the height even");
                }
            }
            "--fps" => fps = value()?.parse::<u64>()?.clamp(1, 120),
            other => bail!("unknown argument {other}"),
        }
    }
    PICTURE.set((w, h, 1000 / fps)).ok();
    Ok(Args {
        bind: bind.context("--bind")?,
        cert: cert.context("--cert")?,
        key: key.context("--key")?,
        hybrid,
        user,
        pass,
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("WRHS_TEST_SERVER_LOG"))
        .with_writer(std::io::stderr)
        .init();
    let args = parse_args()?;

    let identity = TlsIdentityCtx::init_from_paths(&args.cert, &args.key).context("TLS identity")?;
    let acceptor = identity.make_acceptor().context("TLS acceptor")?;
    let builder = RdpServer::builder().with_addr(args.bind);
    let builder = if args.hybrid {
        builder.with_hybrid(acceptor, identity.pub_key)
    } else {
        builder.with_tls(acceptor)
    };

    let events: Events = Arc::new(Mutex::new(None));
    let mut server = builder
        .with_input_handler(Input)
        .with_display_handler(Display {
            events: Arc::clone(&events),
        })
        .with_sound_factory(Some(Box::new(Sound {
            events: Arc::clone(&events),
        })))
        .with_gfx_factory(Some(Box::new(Gfx {
            events: Arc::clone(&events),
        })))
        .build();
    server.set_credentials(Some(Credentials {
        username: args.user,
        password: args.pass,
        domain: None,
    }));
    println!("listening {}", args.bind);
    server.run().await.context("server")
}
