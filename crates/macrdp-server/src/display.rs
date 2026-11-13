use anyhow::Result;
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, GfxFrameUpdate, GfxUncompressedUpdate,
    PixelFormat as RdpPixelFormat, RdpServerDisplay, RdpServerDisplayUpdates,
    RGBAPointer, UncompressedRect, gfx::GfxState,
};
use macrdp_audio::SharedAudioTx;
use macrdp_capture::cursor::{self, CursorImage};
use macrdp_capture::virtual_display::VirtualDisplay;
use macrdp_capture::{CaptureConfig, CapturePixelFormat, CapturedFrame, CgFallbackCapturer, FrameData, ScreenCapturer};
use macrdp_encode::{self, Quality, VideoEncoder};
use macrdp_input::power::DisplayAwake;
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::handler::MouseCoordMapper;
use crate::perf_stats::SharedPerfStats;

/// Maximum tile size for bitmap updates
const TILE_SIZE: u16 = 64;

/// Maximum total dirty area (in pixels) for the uncompressed GFX path.
/// Below this threshold, raw pixels are sent instead of H.264 encoding.
/// 65536 pixels ≈ 256×256 rect ≈ 256 KB of raw BGRA data.
const UNCOMPRESSED_MAX_PIXELS: u32 = 65536;

/// Size limits for the client-sized virtual display. The maximum is the
/// largest frame VideoToolbox's H.264 encoder accepts.
const VIRTUAL_MIN: (u32, u32) = (800, 608);
const VIRTUAL_MAX: (u32, u32) = (4096, 2304);

/// Fits a client-requested size into the virtual display limits, keeping its
/// aspect ratio. Both sides are rounded down to a multiple of 16: the H.264
/// encoder works in 16-pixel blocks, and any other size makes the encoded
/// frame bigger than the desktop (e.g. 3248 for 3236), which the client then
/// squeezes into the desktop. That drifts the picture against the pointer,
/// more toward the right/bottom edge.
fn fit_virtual_size(width: u16, height: u16) -> (u16, u16) {
    let (w, h) = (width.max(1) as f64, height.max(1) as f64);
    let shrink = (VIRTUAL_MAX.0 as f64 / w).min(VIRTUAL_MAX.1 as f64 / h).min(1.0);
    let block = |v: f64, min: u32| ((v as u32).max(min) & !15) as u16;
    (block(w * shrink, VIRTUAL_MIN.0), block(h * shrink, VIRTUAL_MIN.1))
}

/// Frames the client may leave unacknowledged before new ones are held back.
/// A fast client acknowledges within a frame interval and never hits this;
/// a slow one (software decoding) is kept a few frames behind instead of
/// thousands, with stale frames dropped before they're encoded.
pub const DEFAULT_MAX_FRAMES_IN_FLIGHT: u32 = 3;
/// A client that hasn't acknowledged its oldest frame for this long is
/// assumed not to acknowledge at all; flow control turns off for the session
const ACK_STALL_TIMEOUT: Duration = Duration::from_secs(3);

/// The hardware encoder sometimes accepts frames and returns nothing at all:
/// with low-latency rate control, Apple's H.264 encoder drops every frame when
/// the bitrate target falls in a band around 110-120 Mbps at sizes up to about
/// 1920x1200 (e.g. a 1920x1200 client got 111 Mbps -> black screen). After
/// this many empty encodes in a row the encoder is recreated at a lower bitrate.
const EMPTY_ENCODES_BEFORE_RESET: u32 = 5;
/// Each reset multiplies the bitrate by this...
const RESET_BITRATE_FACTOR: f64 = 0.75;
/// ...but not below this
const MIN_RESET_BITRATE: u32 = 8_000_000;

/// How often the frame loop wakes up without a new frame (cursor shape checks,
/// lock-screen fallback frames, stall detection)
const POLL_INTERVAL: Duration = Duration::from_millis(33);

/// A paused capture (screen locked) switches to CoreGraphics after this long
const PAUSED_FALLBACK_AFTER: Duration = Duration::from_millis(300);

/// Restart screen capture if it delivers nothing for this long while the
/// client is sending input...
const STALL_TIMEOUT: Duration = Duration::from_secs(10);
/// ...at most this often
const STALL_RESTART_INTERVAL: Duration = Duration::from_secs(30);

/// Convert a captured frame into tiled BitmapUpdate chunks
pub fn frame_to_bitmap_updates(frame: &CapturedFrame, tile_size: u16) -> Vec<BitmapUpdate> {
    let bgra = match frame.data.as_bgra_bytes() {
        Some(b) => b,
        None => return Vec::new(), // PixelBuffer frames don't go through bitmap path
    };
    let mut updates = Vec::new();
    let bpp: usize = 4;

    let cols = (frame.width as u16 + tile_size - 1) / tile_size;
    let rows = (frame.height as u16 + tile_size - 1) / tile_size;

    for row in 0..rows {
        for col in 0..cols {
            let x = col * tile_size;
            let y = row * tile_size;
            let w = (frame.width as u16 - x).min(tile_size);
            let h = (frame.height as u16 - y).min(tile_size);

            let Some(width) = NonZeroU16::new(w) else { continue };
            let Some(height) = NonZeroU16::new(h) else { continue };

            let mut tile_data = Vec::with_capacity(w as usize * h as usize * bpp);
            for dy in 0..h {
                let src_y = (y + dy) as usize;
                let src_x_start = x as usize * bpp;
                let src_x_end = src_x_start + w as usize * bpp;
                let row_start = src_y * frame.stride;
                if row_start + src_x_end <= bgra.len() {
                    tile_data.extend_from_slice(&bgra[row_start + src_x_start..row_start + src_x_end]);
                }
            }

            let stride = w as usize * bpp;
            let Some(stride) = NonZeroUsize::new(stride) else { continue };

            updates.push(BitmapUpdate {
                x,
                y,
                width,
                height,
                format: RdpPixelFormat::BgrA32,
                data: Bytes::from(tile_data),
                stride,
            });
        }
    }

    updates
}

/// Display adapter that bridges ScreenCapturer to ironrdp-server
pub struct MacDisplay {
    width: u16,
    height: u16,
    /// Maximum capture resolution (physical display resolution).
    /// Client can resize freely up to this limit.
    max_width: u16,
    max_height: u16,
    /// Shared mouse coordinate mapper — updated on resize
    coord_mapper: MouseCoordMapper,
    /// Whether resolution is fixed by config (true) or follows client (false)
    fixed_resolution: bool,
    frame_rate: u32,
    quality: Quality,
    encoder_pref: macrdp_encode::EncoderPreference,
    /// Whether AVC444 mode is requested by config
    mode_444: bool,
    show_cursor: bool,
    base_bitrate: u32,
    gfx_state: Arc<Mutex<GfxState>>,
    /// Give each session its own display at the client's size (match_client_resolution)
    match_client: bool,
    max_frames_in_flight: u32,
    /// Mouse mapper's logical size for the Mac's own display, used when no
    /// virtual display is active
    native_logical: (u16, u16),
    /// Shared audio sender slot — updated per connection by AudioFactory
    shared_audio_tx: Option<SharedAudioTx>,
    /// Shared performance statistics collector (None = disabled)
    perf_stats: Option<SharedPerfStats>,
}

impl MacDisplay {
    pub fn new(
        width: u16, height: u16,
        fixed_resolution: bool,
        frame_rate: u32, quality: Quality,
        encoder_pref: macrdp_encode::EncoderPreference,
        mode_444: bool,
        show_cursor: bool,
        bitrate_override: Option<u32>,
        gfx_state: Arc<Mutex<GfxState>>,
        coord_mapper: MouseCoordMapper,
        shared_audio_tx: Option<SharedAudioTx>,
        perf_stats: Option<SharedPerfStats>,
    ) -> Self {
        let base_bitrate = bitrate_override
            .unwrap_or_else(|| macrdp_encode::screen_bitrate(width as u32, height as u32, frame_rate as f32, quality));
        tracing::info!(base_bitrate_mbps = base_bitrate as f64 / 1_000_000.0, "Base bitrate");
        // Max resolution: use physical display size so clients can resize freely.
        // SCK scales_to_fit handles any resolution.
        let (max_w, max_h) = macrdp_capture::detect_display_scale()
            .map(|scale| (width.max((width as u32 * scale) as u16), height.max((height as u32 * scale) as u16)))
            .unwrap_or((width.max(3840), height.max(2160)));
        let native_logical = coord_mapper.logical_size();
        Self {
            width, height,
            max_width: max_w, max_height: max_h,
            match_client: false,
            max_frames_in_flight: DEFAULT_MAX_FRAMES_IN_FLIGHT,
            native_logical,
            coord_mapper,
            fixed_resolution,
            frame_rate, quality, encoder_pref, mode_444, show_cursor, base_bitrate, gfx_state,
            shared_audio_tx,
            perf_stats,
        }
    }

    /// How far a client may fall behind before frames are held back (see
    /// DEFAULT_MAX_FRAMES_IN_FLIGHT).
    pub fn max_frames_in_flight(mut self, frames: u32) -> Self {
        self.max_frames_in_flight = frames.max(1);
        self
    }

    /// Create a display at each client's requested size (needs resolution "auto").
    pub fn match_client_resolution(mut self, enabled: bool) -> Self {
        self.match_client = enabled && !self.fixed_resolution;
        if self.match_client {
            tracing::info!("Each session gets a virtual display at the client's resolution");
        }
        self
    }

    /// Creates this session's virtual display at the current size and points
    /// capture and mouse mapping at it. Falls back to the Mac's own display.
    async fn prepare_display(&mut self, capture_config: &mut CaptureConfig) -> Option<VirtualDisplay> {
        if !self.match_client {
            return None;
        }
        let (w, h) = (self.width as u32, self.height as u32);
        match tokio::task::spawn_blocking(move || VirtualDisplay::new(w, h)).await {
            Ok(Ok(vd)) => {
                capture_config.display_id = Some(vd.display_id());
                self.coord_mapper.set_logical_size(self.width, self.height);
                Some(vd)
            }
            Ok(Err(e)) => {
                tracing::warn!("Couldn't create a client-sized display, scaling the Mac's display instead: {e}");
                self.use_native_display();
                None
            }
            Err(e) => {
                tracing::warn!("Virtual display task failed: {e}");
                self.use_native_display();
                None
            }
        }
    }

    /// Scale the Mac's own display to the session size (the pre-virtual-display behaviour).
    fn use_native_display(&mut self) {
        let (lw, lh) = self.native_logical;
        self.coord_mapper.set_logical_size(lw, lh);
        let (w, h) = (self.width.min(self.max_width), self.height.min(self.max_height));
        if (w, h) != (self.width, self.height) {
            self.width = w;
            self.height = h;
            self.coord_mapper.update_rdp_size(w, h);
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for MacDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize { width: self.width, height: self.height }
    }

    fn request_resize(&mut self, width: u16, height: u16) {
        if self.fixed_resolution {
            tracing::debug!("Ignoring resize request — resolution is fixed by config");
            return;
        }
        let (w, h) = if self.match_client {
            fit_virtual_size(width, height)
        } else {
            (width.min(self.max_width), height.min(self.max_height))
        };
        if w > 0 && h > 0 && (w != self.width || h != self.height) {
            tracing::info!(
                old_w = self.width, old_h = self.height,
                new_w = w, new_h = h,
                "Adopting client-requested resolution"
            );
            self.width = w;
            self.height = h;
            self.base_bitrate = macrdp_encode::screen_bitrate(
                w as u32, h as u32, self.frame_rate as f32, self.quality,
            );
            self.coord_mapper.update_rdp_size(w, h);
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        let mut capture_config = CaptureConfig {
            width: self.width as u32,
            height: self.height as u32,
            frame_rate: self.frame_rate,
            pixel_format: if self.encoder_pref == macrdp_encode::EncoderPreference::Hardware && !self.mode_444 {
                CapturePixelFormat::Nv12
            } else {
                CapturePixelFormat::Bgra
            },
            show_cursor: self.show_cursor,
            display_id: None,
        };
        // Give this session a display at the client's size (dropped with the session)
        let virtual_display = self.prepare_display(&mut capture_config).await;
        capture_config.width = self.width as u32;
        capture_config.height = self.height as u32;

        // Wake the display before capturing: while it's off macOS draws nothing,
        // so the client would get a black screen. Held until the client leaves.
        let display_awake = DisplayAwake::new();

        // Read the current audio sender from the shared slot (set by AudioFactory per connection)
        let audio_tx = self.shared_audio_tx.as_ref()
            .and_then(|shared| shared.lock().unwrap().clone());
        let capturer = ScreenCapturer::new(capture_config.clone(), audio_tx.clone()).await?;

        // Create H.264 encoder with configured quality and encoder preference
        let encoder = macrdp_encode::create_encoder(
            self.width as u32,
            self.height as u32,
            self.frame_rate as f32,
            self.quality,
            self.encoder_pref,
            self.mode_444,
            self.base_bitrate,
        ).ok();

        if encoder.is_some() {
            tracing::info!("H.264 encoder available — will use GFX path when client supports it");
        }

        Ok(Box::new(MacDisplayUpdates {
            capturer,
            capture_config,
            encoder,
            gfx_state: Arc::clone(&self.gfx_state),
            base_bitrate: self.base_bitrate,
            mode_444: self.mode_444,
            display_frame_count: 0,
            skip_next_frame: false,
            overload_count: 0,
            perf_stats: self.perf_stats.clone(),
            // The cursor isn't in the video, so mirror its shape to the client instead.
            cursor_sync: !self.show_cursor,
            cursor_seed: None,
            last_cursor: None,
            coord_mapper: self.coord_mapper.clone(),
            _display_awake: display_awake,
            audio_tx,
            quality: self.quality,
            encoder_pref: self.encoder_pref,
            empty_encodes: 0,
            max_frames_in_flight: self.max_frames_in_flight,
            needs_full_frame: false,
            held_back_count: 0,
            deferred_frame: None,
            fallback: None,
            paused_since: None,
            last_fallback_frame: None,
            last_capture_event: Instant::now(),
            last_capture_restart: Instant::now(),
            _virtual_display: virtual_display,
        }))
    }
}

struct MacDisplayUpdates {
    capturer: ScreenCapturer,
    capture_config: CaptureConfig,
    encoder: Option<Box<dyn VideoEncoder>>,
    gfx_state: Arc<Mutex<GfxState>>,
    base_bitrate: u32,
    mode_444: bool,
    display_frame_count: u64,
    skip_next_frame: bool,
    /// Counter for rate-limiting encode overload warnings
    overload_count: u64,
    /// Shared performance statistics collector (None = disabled)
    perf_stats: Option<SharedPerfStats>,
    /// Send the Mac's cursor shape to the client (when it isn't drawn into the video)
    cursor_sync: bool,
    /// Cursor change counter at the last check
    cursor_seed: Option<i32>,
    /// Last cursor shape sent to the client
    last_cursor: Option<CursorImage>,
    coord_mapper: MouseCoordMapper,
    /// Keeps the display on for as long as this session's updates are alive
    _display_awake: Option<DisplayAwake>,
    /// Audio sender, so a restarted capturer keeps the session's audio
    audio_tx: Option<tokio::sync::mpsc::Sender<macrdp_capture::AudioFrame>>,
    /// Encoder settings, to recreate it if it stops producing output
    quality: Quality,
    encoder_pref: macrdp_encode::EncoderPreference,
    /// Encodes in a row that returned no data
    empty_encodes: u32,
    max_frames_in_flight: u32,
    /// A held-back frame may have carried small dirty-rect updates that are now
    /// lost, so the next frame actually sent must be a full one
    needs_full_frame: bool,
    /// Frames held back because the client was behind (for rate-limited logging)
    held_back_count: u64,
    /// Latest frame held back (encode overload, or client behind); sent on a
    /// later tick unless a newer frame replaces it first
    deferred_frame: Option<CapturedFrame>,
    /// CoreGraphics capture, used while ScreenCaptureKit is paused (lock screen)
    fallback: Option<CgFallbackCapturer>,
    /// When ScreenCaptureKit started reporting paused, if it currently is
    paused_since: Option<Instant>,
    last_fallback_frame: Option<Instant>,
    /// Last time ScreenCaptureKit delivered anything (frame, idle or paused)
    last_capture_event: Instant,
    last_capture_restart: Instant,
    /// This session's client-sized display; removed when the session ends.
    /// Declared last so it outlives the capturer that's recording it.
    _virtual_display: Option<VirtualDisplay>,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for MacDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        // Never returns Ok(None): to ironrdp that means the display is gone.
        // Internally, Ok(None) from next_frame_update means "nothing to send yet".
        loop {
            if self.cursor_sync {
                if let Some(pointer) = self.changed_cursor() {
                    return Ok(Some(DisplayUpdate::RGBAPointer(pointer)));
                }
            }
            if let Some(update) = self.next_frame_update().await? {
                return Ok(Some(update));
            }
        }
    }
}

impl MacDisplayUpdates {
    /// The Mac's cursor as an RDP pointer, if its shape changed since the last call.
    fn changed_cursor(&mut self) -> Option<RGBAPointer> {
        let seed = cursor::cursor_seed();
        if self.cursor_seed == Some(seed) {
            return None;
        }
        self.cursor_seed = Some(seed);
        let image = cursor::current_cursor(self.coord_mapper.rdp_pixels_per_point())?;
        if self.last_cursor.as_ref() == Some(&image) {
            return None;
        }
        self.last_cursor = Some(image.clone());
        Some(RGBAPointer {
            width: image.width,
            height: image.height,
            hot_x: image.hot_x,
            hot_y: image.hot_y,
            data: image.bgra_bottom_up,
        })
    }

    /// Waits for the next capture event. With cursor sync on, gives up after
    /// CURSOR_POLL and returns None so the caller can check the cursor.
    async fn next_capture_event(&mut self) -> Option<Option<macrdp_capture::CaptureEvent>> {
        tokio::select! {
            event = self.capturer.next_frame() => Some(event),
            _ = tokio::time::sleep(POLL_INTERVAL) => None,
        }
    }

    /// On a poll tick: a CoreGraphics frame while capture is paused (so the
    /// lock screen shows instead of a frozen picture), or a capture restart if
    /// ScreenCaptureKit has gone silent while the client is active.
    async fn handle_tick(&mut self) -> Result<Option<DisplayUpdate>> {
        if self.paused_since.is_none() {
            if let Some(frame) = self.deferred_frame.take() {
                return self.encode_and_send(frame);
            }
        }
        if let Some(since) = self.paused_since {
            let due = self.last_fallback_frame.map_or(true, |t| t.elapsed() >= self.capture_frame_interval());
            if since.elapsed() >= PAUSED_FALLBACK_AFTER && due {
                self.last_fallback_frame = Some(Instant::now());
                let fallback = self.fallback.get_or_insert_with(|| CgFallbackCapturer::new(&self.capture_config));
                if let Some(frame) = fallback.capture_frame() {
                    return self.encode_and_send(frame);
                }
            }
            return Ok(None);
        }

        let client_active = macrdp_input::power::last_user_activity()
            .is_some_and(|t| t.elapsed() < Duration::from_secs(6));
        if client_active
            && self.last_capture_event.elapsed() >= STALL_TIMEOUT
            && self.last_capture_restart.elapsed() >= STALL_RESTART_INTERVAL
        {
            self.last_capture_restart = Instant::now();
            tracing::warn!(
                silent_secs = self.last_capture_event.elapsed().as_secs(),
                "Screen capture went silent while the client is active — restarting it"
            );
            match ScreenCapturer::new(self.capture_config.clone(), self.audio_tx.clone()).await {
                Ok(capturer) => {
                    self.capturer = capturer;
                    self.last_capture_event = Instant::now();
                }
                Err(e) => tracing::warn!("Couldn't restart screen capture: {e}"),
            }
        }
        Ok(None)
    }

    /// Counts an encode that produced no data; after EMPTY_ENCODES_BEFORE_RESET
    /// in a row, recreates the encoder at a lower bitrate.
    fn note_empty_encode(&mut self) {
        self.empty_encodes += 1;
        if self.empty_encodes < EMPTY_ENCODES_BEFORE_RESET {
            return;
        }
        self.empty_encodes = 0;
        let bitrate = ((self.base_bitrate as f64 * RESET_BITRATE_FACTOR) as u32).max(MIN_RESET_BITRATE);
        let (w, h) = (self.capture_config.width, self.capture_config.height);
        tracing::warn!(
            width = w, height = h,
            old_mbps = self.base_bitrate as f64 / 1e6, new_mbps = bitrate as f64 / 1e6,
            "Encoder produced no output — recreating it at a lower bitrate"
        );
        match macrdp_encode::create_encoder(
            w, h, self.capture_config.frame_rate as f32, self.quality, self.encoder_pref, self.mode_444, bitrate,
        ) {
            Ok(encoder) => {
                self.encoder = Some(encoder);
                self.base_bitrate = bitrate;
                self.needs_full_frame = true;
            }
            Err(e) => tracing::warn!("Couldn't recreate the encoder: {e}"),
        }
    }

    /// The client has more than max_frames_in_flight frames it hasn't acknowledged
    fn client_is_behind(&mut self) -> bool {
        let mut state = self.gfx_state.lock().unwrap();
        let behind = state.client_is_behind(self.max_frames_in_flight, ACK_STALL_TIMEOUT);
        if behind {
            self.held_back_count += 1;
            if self.held_back_count % 600 == 1 {
                tracing::info!(
                    in_flight = state.frames_in_flight(),
                    held_back_total = self.held_back_count,
                    "Client is behind — holding frames back"
                );
            }
        }
        behind
    }

    /// Keep `frame` to send on a later tick (a newer frame replaces it). The
    /// frames skipped this way may have held dirty-rect-only updates, so the
    /// next frame actually sent must be a full one.
    fn hold_back(&mut self, frame: CapturedFrame) {
        self.deferred_frame = Some(frame);
        self.needs_full_frame = true;
    }

    fn capture_frame_interval(&self) -> Duration {
        Duration::from_millis(1000 / self.capture_config.frame_rate.clamp(1, 30) as u64)
    }

    async fn next_frame_update(&mut self) -> Result<Option<DisplayUpdate>> {
        // Drain stale frames — always use the latest available frame.
        // If SCK capturer stops (e.g. screen locked), fall back to CGDisplayCreateImage
        // which works at the display level (including lock screen).
        let frame = loop {
            let event = match self.next_capture_event().await {
                None => return self.handle_tick().await,
                Some(Some(e)) => {
                    self.last_capture_event = Instant::now();
                    e
                }
                Some(None) => {
                    // SCK stopped — fall back to CoreGraphics capture (works on lock screen)
                    tracing::warn!("SCStream stopped — switching to CoreGraphics fallback (lock screen?)");
                    let fallback = CgFallbackCapturer::new(&self.capture_config);
                    loop {
                        // Try to restore SCK (faster, has dirty rects)
                        match ScreenCapturer::new(self.capture_config.clone(), self.audio_tx.clone()).await {
                            Ok(new_capturer) => {
                                tracing::info!("SCStream recovered — switching back from CoreGraphics");
                                self.capturer = new_capturer;
                                break;
                            }
                            Err(_) => {
                                // SCK still unavailable — use CGDisplayCreateImage
                                if let Some(cg_frame) = fallback.capture_frame() {
                                    // Send this fallback frame through the normal encoding path
                                    return self.encode_and_send(cg_frame);
                                }
                                tokio::time::sleep(fallback.frame_interval()).await;
                            }
                        }
                    }
                    continue; // retry next_frame with restored SCK capturer
                }
            };
            // Handle CaptureEvent — extract frame or skip idle
            let frame = match event {
                macrdp_capture::CaptureEvent::Frame(f) => {
                    if self.paused_since.take().is_some() {
                        tracing::info!("Screen capture resumed");
                    }
                    // A newer frame supersedes one held back by overload protection
                    self.deferred_frame = None;
                    f
                }
                macrdp_capture::CaptureEvent::Idle => continue,
                macrdp_capture::CaptureEvent::Paused => {
                    if self.paused_since.is_none() {
                        tracing::warn!("Screen capture paused (screen locked?) — showing it via CoreGraphics");
                        self.paused_since = Some(Instant::now());
                    }
                    continue;
                }
            };
            // If another frame is already buffered, skip this one and grab the newer one
            // This prevents frame queuing which adds latency
            match self.capturer.try_next_frame() {
                Some(macrdp_capture::CaptureEvent::Frame(_newer)) => continue,
                Some(macrdp_capture::CaptureEvent::Idle | macrdp_capture::CaptureEvent::Paused) => break frame,
                None => break frame,
            }
        };

        self.encode_and_send(frame)
    }

    fn encode_and_send(&mut self, frame: CapturedFrame) -> Result<Option<DisplayUpdate>> {
        // Encode overload protection: if the previous encode took too long, hold
        // this frame back instead of encoding it now. It's sent on the next poll
        // tick unless a newer frame replaces it first. (Dropping it outright left
        // the client stale after the last change of a burst, e.g. typing.)
        if self.skip_next_frame {
            self.skip_next_frame = false;
            self.hold_back(frame);
            return Ok(None);
        }

        // Check GFX state and AVC444 negotiation
        let (gfx_ready, use_444) = {
            let state = self.gfx_state.lock().unwrap();
            let ready = state.is_ready() && self.encoder.is_some();
            let use_444 = self.mode_444
                && state.avc444_supported
                && state.avc444_enabled;
            (ready, use_444)
        };

        // Flow control: don't pile frames onto a client that hasn't acknowledged
        // the previous ones (it would decode them all, seconds behind). Keep the
        // newest frame and send it once the client has caught up.
        if gfx_ready && self.client_is_behind() {
            self.hold_back(frame);
            return Ok(None);
        }

        if gfx_ready {
            // GFX uncompressed path — for small dirty regions, skip H.264 encoding entirely
            if !frame.dirty_rects.is_empty() && !self.needs_full_frame {
                let total_area: u32 = frame.dirty_rects.iter()
                    .map(|r| r.width * r.height)
                    .sum();

                if total_area > 0 && total_area <= UNCOMPRESSED_MAX_PIXELS {
                    if let Some(bgra) = frame.data.as_bgra_bytes() {
                        let rects: Vec<UncompressedRect> = frame.dirty_rects.iter()
                            .filter(|r| r.width > 0 && r.height > 0)
                            .filter_map(|r| {
                                // Clamp to frame bounds
                                let x = r.x.min(frame.width.saturating_sub(1));
                                let y = r.y.min(frame.height.saturating_sub(1));
                                let w = r.width.min(frame.width - x);
                                let h = r.height.min(frame.height - y);
                                if w == 0 || h == 0 { return None; }

                                let bpp = 4usize;
                                let row_bytes = w as usize * bpp;
                                let mut pixels = Vec::with_capacity(h as usize * row_bytes);
                                for row in 0..h as usize {
                                    let src_y = y as usize + row;
                                    let offset = src_y * frame.stride + x as usize * bpp;
                                    let end = offset + row_bytes;
                                    if end <= bgra.len() {
                                        pixels.extend_from_slice(&bgra[offset..end]);
                                    }
                                }
                                Some(UncompressedRect {
                                    x: x as u16,
                                    y: y as u16,
                                    width: w as u16,
                                    height: h as u16,
                                    pixel_data: Bytes::from(pixels),
                                })
                            })
                            .collect();

                        if !rects.is_empty() {
                            self.display_frame_count += 1;
                            let total_bytes: usize = rects.iter().map(|r| r.pixel_data.len()).sum();
                            {
                                let mut st = self.gfx_state.lock().unwrap();
                                st.last_encode_ms = 0.0; // zero encode time
                                st.last_frame_bytes = total_bytes as u32;
                            }
                            tracing::debug!(
                                display_frame = self.display_frame_count,
                                rects = rects.len(),
                                total_pixels = total_area,
                                total_bytes,
                                "Display: sending GFX uncompressed dirty rects"
                            );
                            return Ok(Some(DisplayUpdate::GfxUncompressed(GfxUncompressedUpdate {
                                rects,
                                width: frame.width as u16,
                                height: frame.height as u16,
                            })));
                        }
                    }
                    // PixelBuffer frames or extraction failure — fall through to H.264
                }
            }

            // GFX H.264 path — always send at capture rate, never block on acks
            if let Some(encoder) = &mut self.encoder {
                self.display_frame_count += 1;
                let t0 = std::time::Instant::now();

                // Route based on frame data type
                match &frame.data {
                    FrameData::PixelBuffer(buf) => {
                        // Zero-copy VT path — encode CVPixelBuffer directly
                        match encoder.encode_pixel_buffer(buf.as_ptr(), false) {
                            Ok(encoded) if !encoded.data.is_empty() => {
                                let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                                let is_keyframe = encoded.is_keyframe;
                                tracing::debug!(
                                    display_frame = self.display_frame_count,
                                    h264_bytes = encoded.data.len(),
                                    is_keyframe,
                                    encode_ms = format!("{:.1}", encode_ms),
                                    "Display: sending zero-copy GFX frame"
                                );
                                {
                                    let mut st = self.gfx_state.lock().unwrap();
                                    st.last_encode_ms = encode_ms;
                                    st.last_frame_bytes = encoded.data.len() as u32;
                                }
                                if let Some(ps) = &self.perf_stats {
                                    ps.lock().unwrap().record_frame(encode_ms, encoded.data.len() as u32, is_keyframe);
                                }
                                let frame_interval_ms = 1000.0 / self.capture_config.frame_rate as f64;
                                if encode_ms > frame_interval_ms * 0.95 {
                                    self.skip_next_frame = true;
                                    self.overload_count += 1;
                                    // Rate-limit log: warn once per ~60 overloads, debug otherwise
                                    if self.overload_count % 60 == 1 {
                                        tracing::warn!(
                                            encode_ms = format!("{:.1}", encode_ms),
                                            frame_interval_ms = format!("{:.1}", frame_interval_ms),
                                            overload_total = self.overload_count,
                                            "encode overload — skipping frames"
                                        );
                                    }
                                }
                                self.needs_full_frame = false;
                                self.empty_encodes = 0;
                                return Ok(Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
                                    h264_data: encoded.data,
                                    width: frame.width as u16,
                                    height: frame.height as u16,
                                    enc_width: encoded.width as u16,
                                    enc_height: encoded.height as u16,
                                    is_keyframe,
                                    h264_aux: None,
                                })));
                            }
                            Ok(_) => {
                                tracing::warn!("Zero-copy encode returned empty data");
                                self.note_empty_encode();
                                return Ok(None);
                            }
                            Err(e) => {
                                tracing::warn!("Zero-copy encode failed: {e}, falling back to DefaultPointer");
                                return Ok(None);
                            }
                        }
                    }
                    FrameData::Raw(_) => {
                        // Existing BGRA encode path — continues below
                    }
                }
                let bgra = frame.data.as_bgra_bytes().unwrap();

                // AVC444 dual-stream path
                if use_444 && encoder.supports_444() {
                    match encoder.encode_bgra_444(bgra, frame.width, frame.height, frame.stride) {
                        Ok(encoded) if !encoded.main_view.data.is_empty() => {
                            let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                            let total_bytes = encoded.main_view.data.len() + encoded.aux_view.data.len();
                            let is_keyframe = encoded.main_view.is_keyframe;
                            tracing::debug!(
                                display_frame = self.display_frame_count,
                                main_bytes = encoded.main_view.data.len(),
                                aux_bytes = encoded.aux_view.data.len(),
                                is_keyframe,
                                encode_ms = format!("{:.1}", encode_ms),
                                "Display: sending AVC444 GFX frame"
                            );
                            {
                                let mut st = self.gfx_state.lock().unwrap();
                                st.last_encode_ms = encode_ms;
                                st.last_frame_bytes = total_bytes as u32;
                            }
                            if let Some(ps) = &self.perf_stats {
                                ps.lock().unwrap().record_frame(encode_ms, total_bytes as u32, is_keyframe);
                            }
                            let frame_interval_ms = 1000.0 / self.capture_config.frame_rate as f64;
                            if encode_ms > frame_interval_ms * 0.95 {
                                self.skip_next_frame = true;
                                self.overload_count += 1;
                                if self.overload_count % 60 == 1 {
                                    tracing::warn!(
                                        encode_ms = format!("{:.1}", encode_ms),
                                        frame_interval_ms = format!("{:.1}", frame_interval_ms),
                                        overload_total = self.overload_count,
                                        "encode overload — skipping frames"
                                    );
                                }
                            }
                            self.needs_full_frame = false;
                                self.empty_encodes = 0;
                                return Ok(Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
                                h264_data: encoded.main_view.data,
                                width: frame.width as u16,
                                height: frame.height as u16,
                                enc_width: encoded.main_view.width as u16,
                                enc_height: encoded.main_view.height as u16,
                                is_keyframe,
                                h264_aux: Some(encoded.aux_view.data),
                            })));
                        }
                        Ok(_) => {
                            tracing::warn!(
                                display_frame = self.display_frame_count,
                                "AVC444 encode returned EMPTY data — frame dropped!"
                            );
                            self.note_empty_encode();
                                return Ok(None);
                        }
                        Err(e) => {
                            tracing::warn!(display_frame = self.display_frame_count, "AVC444 encode failed: {e}, falling back to AVC420");
                            // Fall through to AVC420 path below
                        }
                    }
                }

                // AVC420 path (default or fallback from AVC444 failure)
                match encoder.encode_bgra(bgra, frame.width, frame.height, frame.stride) {
                    Ok(encoded) if !encoded.data.is_empty() => {
                        let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                        let is_keyframe = encoded.is_keyframe;
                        tracing::debug!(
                            display_frame = self.display_frame_count,
                            h264_bytes = encoded.data.len(),
                            is_keyframe,
                            encode_ms = format!("{:.1}", encode_ms),
                            "Display: sending GFX frame"
                        );
                        {
                            let mut st = self.gfx_state.lock().unwrap();
                            st.last_encode_ms = encode_ms;
                            st.last_frame_bytes = encoded.data.len() as u32;
                        }
                        if let Some(ps) = &self.perf_stats {
                            ps.lock().unwrap().record_frame(encode_ms, encoded.data.len() as u32, is_keyframe);
                        }
                        let frame_interval_ms = 1000.0 / self.capture_config.frame_rate as f64;
                        if encode_ms > frame_interval_ms * 0.95 {
                            self.skip_next_frame = true;
                            self.overload_count += 1;
                            if self.overload_count % 60 == 1 {
                                tracing::warn!(
                                    encode_ms = format!("{:.1}", encode_ms),
                                    frame_interval_ms = format!("{:.1}", frame_interval_ms),
                                    overload_total = self.overload_count,
                                    "encode overload — skipping frames"
                                );
                            }
                        }
                        self.needs_full_frame = false;
                                self.empty_encodes = 0;
                                return Ok(Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
                            h264_data: encoded.data,
                            width: frame.width as u16,
                            height: frame.height as u16,
                            enc_width: encoded.width as u16,
                            enc_height: encoded.height as u16,
                            is_keyframe,
                            h264_aux: None,
                        })));
                    }
                    Ok(_) => {
                        tracing::warn!(
                            display_frame = self.display_frame_count,
                            "H.264 encode returned EMPTY data — frame dropped!"
                        );
                        self.note_empty_encode();
                                return Ok(None);
                    }
                    Err(e) => {
                        tracing::warn!(display_frame = self.display_frame_count, "H.264 encode failed: {e}");
                    }
                }
            }
        } else if self.encoder.is_some() {
            // H.264 encoder exists — never send bitmaps, wait for GFX to become ready.
            // Mixing bitmap and GFX causes 0xd06 DECOMPRESSION_FAILED on reconnect.
            return Ok(None);
        }

        // Bitmap path (only when GFX is not available at all)
        // Requires BGRA raw bytes — PixelBuffer frames should not reach here
        let bgra_bitmap = match &frame.data {
            FrameData::Raw(bytes) => bytes,
            FrameData::PixelBuffer(_) => {
                tracing::warn!("PixelBuffer frame in bitmap path — should not happen");
                return Ok(None);
            }
        };

        if !frame.dirty_rects.is_empty() {
            // Find bounding box of all dirty rects to send a single update
            let mut min_x = frame.width;
            let mut min_y = frame.height;
            let mut max_x = 0u32;
            let mut max_y = 0u32;

            for r in &frame.dirty_rects {
                min_x = min_x.min(r.x);
                min_y = min_y.min(r.y);
                max_x = max_x.max(r.x + r.width);
                max_y = max_y.max(r.y + r.height);
            }

            // Clamp to frame bounds
            max_x = max_x.min(frame.width);
            max_y = max_y.min(frame.height);

            if max_x > min_x && max_y > min_y {
                let w = max_x - min_x;
                let h = max_y - min_y;
                let Some(width) = NonZeroU16::new(w as u16) else { return Ok(None) };
                let Some(height) = NonZeroU16::new(h as u16) else { return Ok(None) };

                // Extract only the dirty region from the full frame buffer
                let bpp = 4usize;
                let dirty_stride = w as usize * bpp;
                let mut dirty_data = Vec::with_capacity(dirty_stride * h as usize);
                for row in min_y..max_y {
                    let src_offset = row as usize * frame.stride + min_x as usize * bpp;
                    let src_end = src_offset + dirty_stride;
                    if src_end <= bgra_bitmap.len() {
                        dirty_data.extend_from_slice(&bgra_bitmap[src_offset..src_end]);
                    }
                }

                let Some(stride) = NonZeroUsize::new(dirty_stride) else { return Ok(None) };

                let update = BitmapUpdate {
                    x: min_x as u16,
                    y: min_y as u16,
                    width,
                    height,
                    format: RdpPixelFormat::BgrA32,
                    data: Bytes::from(dirty_data),
                    stride,
                };

                return Ok(Some(DisplayUpdate::Bitmap(update)));
            }
        }

        // No dirty rects available — send full frame (first frame or fallback)
        let Some(width) = NonZeroU16::new(frame.width as u16) else { return Ok(None) };
        let Some(height) = NonZeroU16::new(frame.height as u16) else { return Ok(None) };
        let Some(stride) = NonZeroUsize::new(frame.stride) else { return Ok(None) };

        let update = BitmapUpdate {
            x: 0,
            y: 0,
            width,
            height,
            format: RdpPixelFormat::BgrA32,
            data: bgra_bitmap.clone(),
            stride,
        };

        Ok(Some(DisplayUpdate::Bitmap(update)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_to_bitmap_updates() {
        let frame = CapturedFrame {
            width: 100,
            height: 50,
            data: FrameData::Raw(Bytes::from(vec![0u8; 100 * 50 * 4])),
            stride: 400,
            timestamp_us: 0,
            dirty_rects: vec![],
        };

        let updates = frame_to_bitmap_updates(&frame, 64);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].x, 0);
        assert_eq!(updates[0].width.get(), 64);
        assert_eq!(updates[1].x, 64);
        assert_eq!(updates[1].width.get(), 36);
    }

    #[test]
    fn test_frame_to_bitmap_updates_exact_tile() {
        let frame = CapturedFrame {
            width: 128,
            height: 64,
            data: FrameData::Raw(Bytes::from(vec![0u8; 128 * 64 * 4])),
            stride: 512,
            timestamp_us: 0,
            dirty_rects: vec![],
        };

        let updates = frame_to_bitmap_updates(&frame, 64);
        assert_eq!(updates.len(), 2);
    }
}

#[cfg(test)]
mod virtual_size_tests {
    use super::fit_virtual_size;

    #[test]
    fn fits_client_sizes() {
        assert_eq!(fit_virtual_size(2560, 1440), (2560, 1440));
        assert_eq!(fit_virtual_size(3236, 1345), (3232, 1344)); // 16-pixel blocks
        assert_eq!(fit_virtual_size(3440, 1440), (3440, 1440));
        assert_eq!(fit_virtual_size(1716, 1104), (1712, 1104));
        assert_eq!(fit_virtual_size(640, 480), (800, 608)); // minimum
        assert_eq!(fit_virtual_size(5120, 1440), (4096, 1152)); // ultrawide shrinks, keeps aspect
        assert_eq!(fit_virtual_size(7680, 4320), (4096, 2304));
        for (w, h) in [(1969, 1276), (3236, 1393), (1698, 1086), (1001, 777)] {
            let (fw, fh) = fit_virtual_size(w, h);
            assert_eq!((fw % 16, fh % 16), (0, 0), "{w}x{h} -> {fw}x{fh}");
        }
    }
}
