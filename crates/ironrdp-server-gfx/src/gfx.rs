//! RDPGFX (Graphics Pipeline) DVC handler

use std::sync::{Arc, Mutex};

use ironrdp_core::{encode_vec, impl_as_any, Encode, WriteCursor, EncodeResult};
use ironrdp_dvc::{DvcEncode, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_pdu::rdp::vc::dvc::gfx::{
    Avc420BitmapStream, Avc444BitmapStream, CapabilitiesAdvertisePdu, CapabilitiesConfirmPdu, CapabilitiesV10Flags,
    CapabilitiesV103Flags, CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitiesV81Flags,
    CapabilitySet, ClientPdu, Codec1Type, CreateSurfacePdu, EndFramePdu, Encoding,
    MapSurfaceToOutputPdu, PixelFormat as GfxPixelFormat, QuantQuality, ResetGraphicsPdu,
    QueueDepth, ServerPdu, StartFramePdu, Timestamp, WireToSurface1Pdu,
};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::{decode, PduResult};

type DvcMessage = ironrdp_dvc::DvcMessage;
use tracing::{debug, info};

use crate::display::{GfxFrameUpdate, GfxUncompressedUpdate};

/// GFX channel name as defined by RDP spec
pub const GFX_CHANNEL_NAME: &str = "Microsoft::Windows::RDS::Graphics";

/// Wrapper to make raw PDU bytes usable as DvcMessage
pub struct RawGfxPdu(pub Vec<u8>);

impl Encode for RawGfxPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "GfxServerPdu"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for RawGfxPdu {}

// SAFETY: RawGfxPdu only contains Vec<u8> which is Send
unsafe impl Send for RawGfxPdu {}

/// Wrap raw GFX PDU bytes in RDP_SEGMENTED_DATA (ZGFX) format.
/// MS-RDPEGFX Section 2.2.5: ALL GFX PDUs must be ZGFX-wrapped before DVC transport.
/// Using uncompressed mode (descriptor 0xE0/0xE1, compression type 0x04).
fn wrap_zgfx(data: &[u8]) -> Vec<u8> {
    const SINGLE: u8 = 0xE0;
    const MULTIPART: u8 = 0xE1;
    const UNCOMPRESSED: u8 = 0x04;
    // Max data per segment = 65534 bytes. The segmentSize field includes the
    // 1-byte compression type, so segmentSize = data_len + 1 ≤ 65535 (0xFFFF).
    const MAX_SEG_DATA: usize = 65534;

    if data.len() <= MAX_SEG_DATA {
        // Single segment: descriptor(1) + compression_type(1) + data
        let mut out = Vec::with_capacity(2 + data.len());
        out.push(SINGLE);
        out.push(UNCOMPRESSED);
        out.extend_from_slice(data);
        out
    } else {
        // Multipart: descriptor(1) + seg_count(2) + uncompressed_size(4) + segments
        let seg_count = data.len().div_ceil(MAX_SEG_DATA);
        let mut out = Vec::with_capacity(7 + data.len() + seg_count * 5);
        out.push(MULTIPART);
        out.extend_from_slice(&(seg_count as u16).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        for chunk in data.chunks(MAX_SEG_DATA) {
            // segmentSize = compression_type(1) + chunk data
            let seg_size = (chunk.len() + 1) as u32;
            out.extend_from_slice(&seg_size.to_le_bytes());
            out.push(UNCOMPRESSED);
            out.extend_from_slice(chunk);
        }
        out
    }
}

/// Unwrap ZGFX segmented data from client. Returns None if not ZGFX-wrapped.
fn unwrap_zgfx(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        0xE0 => {
            // Single segment: skip descriptor(1) + compression_type(1)
            if data.len() > 2 && data[1] == 0x04 {
                Some(data[2..].to_vec())
            } else {
                None
            }
        }
        0xE1 => {
            // Multipart: descriptor(1) + segment_count(2) + uncompressed_size(4)
            if data.len() < 7 {
                return None;
            }
            let seg_count = u16::from_le_bytes([data[1], data[2]]) as usize;
            let mut offset = 7;
            let mut result = Vec::new();
            for _ in 0..seg_count {
                if offset + 4 > data.len() {
                    break;
                }
                let seg_size = u32::from_le_bytes([
                    data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
                ]) as usize;
                offset += 4;
                if offset < data.len() && data[offset] == 0x04 {
                    // Uncompressed: skip compression_type byte
                    offset += 1;
                    let end = (offset + seg_size - 1).min(data.len());
                    result.extend_from_slice(&data[offset..end]);
                    offset = end;
                } else {
                    break;
                }
            }
            Some(result)
        }
        _ => None, // Not ZGFX-wrapped
    }
}

fn make_dvc_message(pdu: &ServerPdu) -> PduResult<DvcMessage> {
    let data = encode_vec(pdu).map_err(|e| ironrdp_pdu::custom_err!("GfxEncode", e))?;
    let wrapped = wrap_zgfx(&data);
    Ok(Box::new(RawGfxPdu(wrapped)))
}

/// Shared state between GfxHandler and the server
#[derive(Debug)]
pub struct GfxState {
    pub channel_id: Option<u32>,
    pub surface_created: bool,
    pub caps_confirmed: bool,
    pub frame_id: u32,
    pub avc420_supported: bool,
    /// Whether the client supports AVC444 (V10+ with AVC not disabled)
    pub avc444_supported: bool,
    /// Whether AVC444 is enabled by server config (chroma_mode = "avc444")
    pub avc444_enabled: bool,
    pub confirmed_cap: Option<CapabilitySet>,
    pub width: u16,
    pub height: u16,
    /// Frames sent but not yet acknowledged
    pub pending_acks: u32,
    /// The client told us (QueueDepth::Suspend) or showed us (no acks for
    /// seconds) that it won't acknowledge frames: flow control is off.
    pub acks_suspended: bool,
    /// Last acknowledged frame ID
    pub last_ack_frame: u32,
    /// Network quality estimate (0.0 = congested, 1.0 = excellent)
    pub network_quality: f32,
    /// Send timestamps for RTT calculation (frame_id → Instant)
    pub frame_send_times: std::collections::HashMap<u32, std::time::Instant>,
    /// Exponentially weighted moving average RTT in ms
    pub rtt_ewma_ms: f64,
    /// Last frame encode time in ms
    pub last_encode_ms: f64,
    /// Last frame size in bytes
    pub last_frame_bytes: u32,
    /// Total bytes sent
    pub total_bytes_sent: u64,
    /// Time of first frame sent
    pub start_time: Option<std::time::Instant>,
    /// Client peer IP address (set after TCP connection is established)
    pub peer_addr: Option<std::net::IpAddr>,
    /// Instantaneous bitrate tracking (per-frame Mbps samples since last log)
    bitrate_samples: Vec<f64>,
    bitrate_max: f64,
    bitrate_min: f64,
    last_frame_time: Option<std::time::Instant>,
    /// Hot-updatable: target bitrate override (set via ConfigUpdate)
    pub target_bitrate: Option<u32>,
    /// Hot-updatable: show cursor flag (set via ConfigUpdate)
    pub show_cursor: Option<bool>,
    /// Hot-updatable: resolution (set via ConfigUpdate)
    pub resolution: Option<String>,
    /// Hot-updatable: encoder preference (set via ConfigUpdate)
    pub encoder_pref: Option<String>,
    /// Hot-updatable: chroma mode (set via ConfigUpdate)
    pub chroma_mode: Option<String>,
}

impl GfxState {
    pub fn new(width: u16, height: u16, avc444_enabled: bool) -> Self {
        Self {
            channel_id: None,
            surface_created: false,
            caps_confirmed: false,
            frame_id: 0,
            avc420_supported: false,
            avc444_supported: false,
            avc444_enabled,
            confirmed_cap: None,
            width,
            height,
            pending_acks: 0,
            acks_suspended: false,
            last_ack_frame: 0,
            network_quality: 1.0,
            frame_send_times: std::collections::HashMap::new(),
            rtt_ewma_ms: 0.0,
            last_encode_ms: 0.0,
            last_frame_bytes: 0,
            total_bytes_sent: 0,
            start_time: None,
            peer_addr: None,
            bitrate_samples: Vec::new(),
            bitrate_max: 0.0,
            bitrate_min: f64::MAX,
            last_frame_time: None,
            target_bitrate: None,
            show_cursor: None,
            resolution: None,
            encoder_pref: None,
            chroma_mode: None,
        }
    }

    pub fn next_frame_id(&mut self) -> u32 {
        self.frame_id += 1;
        self.pending_acks += 1;
        self.total_bytes_sent += self.last_frame_bytes as u64;
        if self.start_time.is_none() {
            self.start_time = Some(std::time::Instant::now());
        }
        // Track instantaneous bitrate per frame
        if let Some(prev) = self.last_frame_time {
            let dt = prev.elapsed().as_secs_f64().max(0.0001);
            let mbps = self.last_frame_bytes as f64 * 8.0 / dt / 1_000_000.0;
            self.bitrate_samples.push(mbps);
            if mbps > self.bitrate_max { self.bitrate_max = mbps; }
            if mbps < self.bitrate_min { self.bitrate_min = mbps; }
        }
        self.last_frame_time = Some(std::time::Instant::now());
        if !self.acks_suspended {
            self.frame_send_times.insert(self.frame_id, std::time::Instant::now());
        }
        // Limit map size to prevent unbounded growth
        if self.frame_send_times.len() > 120 {
            let cutoff = self.frame_id.saturating_sub(120);
            self.frame_send_times.retain(|&id, _| id > cutoff);
        }
        self.frame_id
    }

    /// Frames sent that the client hasn't acknowledged yet. Counted by frame
    /// ID, so a client that acknowledges only some frames can't drift it.
    pub fn frames_in_flight(&self) -> u32 {
        if self.acks_suspended {
            0
        } else {
            self.frame_id.saturating_sub(self.last_ack_frame)
        }
    }

    /// Whether the next frame should wait: the client is more than
    /// `max_in_flight` frames behind and is still acknowledging. A client that
    /// hasn't acknowledged its oldest outstanding frame within `stall_timeout`
    /// is treated as not acknowledging at all, so it can't freeze the stream.
    pub fn client_is_behind(&mut self, max_in_flight: u32, stall_timeout: std::time::Duration) -> bool {
        if self.acks_suspended || self.frames_in_flight() <= max_in_flight {
            return false;
        }
        let oldest_unacked = self.frame_send_times.values().min().copied();
        match oldest_unacked {
            Some(sent) if sent.elapsed() > stall_timeout => {
                tracing::warn!(
                    in_flight = self.frames_in_flight(),
                    "Client stopped acknowledging frames — sending without flow control"
                );
                self.suspend_acks();
                false
            }
            _ => true,
        }
    }

    pub fn suspend_acks(&mut self) {
        self.acks_suspended = true;
        self.frame_send_times.clear();
    }

    /// Update network quality and RTT based on frame acknowledgment
    pub fn ack_frame(&mut self, ack_frame_id: u32) {
        self.last_ack_frame = self.last_ack_frame.max(ack_frame_id);
        // Everything up to the acked frame is done, even if it wasn't acked itself
        self.frame_send_times.retain(|&id, _| id > ack_frame_id);
        self.pending_acks = self.frames_in_flight();

        // Calculate RTT from send timestamp
        if let Some(send_time) = self.frame_send_times.remove(&ack_frame_id) {
            let rtt_ms = send_time.elapsed().as_secs_f64() * 1000.0;
            // EWMA with alpha=0.2 for smooth averaging
            if self.rtt_ewma_ms == 0.0 {
                self.rtt_ewma_ms = rtt_ms;
            } else {
                self.rtt_ewma_ms = self.rtt_ewma_ms * 0.8 + rtt_ms * 0.2;
            }
        }

        // Network quality based on RTT and ack backlog
        self.network_quality = if self.rtt_ewma_ms < 15.0 && self.pending_acks <= 2 {
            1.0 // Excellent: max bitrate
        } else if self.rtt_ewma_ms < 30.0 && self.pending_acks <= 5 {
            0.8 // Good
        } else if self.rtt_ewma_ms < 60.0 && self.pending_acks <= 10 {
            0.5 // Fair
        } else if self.rtt_ewma_ms < 100.0 {
            0.3 // Poor
        } else {
            0.15 // Congested
        };
    }

    /// Get recommended bitrate based on network quality and base bitrate
    pub fn adaptive_bitrate(&self, base_bitrate: u32) -> u32 {
        (base_bitrate as f32 * self.network_quality) as u32
    }

    pub fn is_ready(&self) -> bool {
        self.channel_id.is_some() && self.avc420_supported && self.caps_confirmed
    }
}

/// GFX DVC processor
pub struct GfxHandler {
    pub state: Arc<Mutex<GfxState>>,
    /// ZGFX history for client-to-server data (clients may compress it)
    decompressor: ironrdp_graphics::zgfx::Decompressor,
}

/// RDPGFX_CMDID_CAPSADVERTISE
const CMDID_CAPS_ADVERTISE: u16 = 0x12;
/// RDPGFX_HEADER: cmdId(2) + flags(2) + pduLength(4)
const GFX_HEADER_SIZE: usize = 8;

/// Parses a CapabilitiesAdvertise body, skipping capability sets this build
/// doesn't know. Newer clients (e.g. the Windows App) advertise versions past
/// 10.7; the stock decoder rejects the whole PDU for one unknown version,
/// which left the session without graphics (a black screen).
fn parse_caps_advertise(body: &[u8]) -> Vec<CapabilitySet> {
    let mut sets = Vec::new();
    if body.len() < 2 {
        return sets;
    }
    let count = u16::from_le_bytes([body[0], body[1]]) as usize;
    let mut offset = 2;
    for _ in 0..count {
        if offset + 8 > body.len() {
            break;
        }
        let version = u32::from_le_bytes(body[offset..offset + 4].try_into().unwrap());
        let len = u32::from_le_bytes(body[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let end = offset + 8 + len;
        if end > body.len() {
            break;
        }
        match decode::<CapabilitySet>(&body[offset..end]) {
            Ok(set) => sets.push(set),
            Err(_) => info!("GFX: skipping capability set version {version:#x} ({len} bytes)"),
        }
        offset = end;
    }
    sets
}

impl_as_any!(GfxHandler);

impl GfxHandler {
    pub fn new(state: Arc<Mutex<GfxState>>) -> Self {
        Self { state, decompressor: ironrdp_graphics::zgfx::Decompressor::new() }
    }

    /// Create a single ZGFX-wrapped buffer containing all GFX PDUs for an H.264 frame.
    /// Per MS-RDPEGFX Section 2.2.5: "The server SHOULD combine multiple RDPGFX commands
    /// into a single RDP_SEGMENTED_DATA structure."
    /// First call also includes surface setup PDUs (ResetGraphics + CreateSurface + MapSurfaceToOutput).
    pub fn create_frame_pdu(state: &mut GfxState, frame: &GfxFrameUpdate) -> Vec<u8> {
        // Concatenate all raw GFX PDUs, then ZGFX-wrap once
        let mut raw_pdus = Vec::new();

        let enc_w = frame.enc_width;
        let enc_h = frame.enc_height;

        // Detect resolution change (e.g. client resize or HiDPI mismatch) — rebuild surface
        if state.surface_created
            && (state.width != frame.width || state.height != frame.height)
        {
            info!(
                old_w = state.width, old_h = state.height,
                new_w = frame.width, new_h = frame.height,
                "GFX resolution changed, rebuilding surface"
            );
            state.width = frame.width;
            state.height = frame.height;
            state.surface_created = false;
        }

        // First frame: surface setup (CapConfirm already sent by DVC handler)
        if !state.surface_created {
            if let Ok(data) = encode_vec(&ServerPdu::ResetGraphics(ResetGraphicsPdu {
                width: state.width as u32,
                height: state.height as u32,
                monitors: vec![Monitor {
                    left: 0,
                    top: 0,
                    right: state.width as i32 - 1,
                    bottom: state.height as i32 - 1,
                    flags: MonitorFlags::PRIMARY,
                }],
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            if let Ok(data) = encode_vec(&ServerPdu::CreateSurface(CreateSurfacePdu {
                surface_id: 0,
                width: state.width,
                height: state.height,
                pixel_format: GfxPixelFormat::XRgb,
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            if let Ok(data) = encode_vec(&ServerPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                surface_id: 0,
                output_origin_x: 0,
                output_origin_y: 0,
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            state.surface_created = true;
            info!("GFX surface created: {}x{} (enc: {}x{})", state.width, state.height, enc_w, enc_h);
        }

        let frame_id = state.next_frame_id();

        // StartFrame
        if let Ok(data) = encode_vec(&ServerPdu::StartFrame(StartFramePdu {
            timestamp: Timestamp {
                milliseconds: 0,
                seconds: 0,
                minutes: 0,
                hours: 0,
            },
            frame_id,
        })) {
            raw_pdus.extend_from_slice(&data);
        }

        let make_rect = || InclusiveRectangle {
            left: 0,
            top: 0,
            right: frame.width,   // RDPGFX_RECT16 exclusive bound = visible crop
            bottom: frame.height, // RDPGFX_RECT16 exclusive bound = visible crop
        };

        let make_dest_rect = || InclusiveRectangle {
            left: 0,
            top: 0,
            right: frame.width,   // RDPGFX_RECT16 exclusive bound = match surface/desktop
            bottom: frame.height, // RDPGFX_RECT16 exclusive bound = match surface/desktop
        };

        let make_qq = || QuantQuality {
            quantization_parameter: 22,
            progressive: false,
            quality: 100,
        };

        // Choose AVC444 or AVC420 path based on available data and negotiated caps
        let use_avc444 = frame.h264_aux.is_some()
            && state.avc444_supported
            && state.avc444_enabled;

        if use_avc444 {
            // AVC444 path: WireToSurface1 + Avc444BitmapStream
            let aux_data = frame.h264_aux.as_ref().unwrap();
            let avc444_stream = Avc444BitmapStream {
                encoding: Encoding::LUMA_AND_CHROMA,
                stream1: Avc420BitmapStream {
                    rectangles: vec![make_rect()],
                    quant_qual_vals: vec![make_qq()],
                    data: &frame.h264_data,
                },
                stream2: Some(Avc420BitmapStream {
                    rectangles: vec![make_rect()],
                    quant_qual_vals: vec![make_qq()],
                    data: aux_data,
                }),
            };

            if let Ok(avc444_data) = encode_vec(&avc444_stream) {
                if let Ok(data) = encode_vec(&ServerPdu::WireToSurface1(WireToSurface1Pdu {
                    surface_id: 0,
                    codec_id: Codec1Type::Avc444,
                    pixel_format: GfxPixelFormat::XRgb,
                    destination_rectangle: make_dest_rect(),
                    bitmap_data: avc444_data,
                })) {
                    raw_pdus.extend_from_slice(&data);
                }
            }
        } else {
            // AVC420 path: WireToSurface1 + Avc420BitmapStream
            let avc_stream = Avc420BitmapStream {
                rectangles: vec![make_rect()],
                quant_qual_vals: vec![make_qq()],
                data: &frame.h264_data,
            };

            if let Ok(avc_data) = encode_vec(&avc_stream) {
                if let Ok(data) = encode_vec(&ServerPdu::WireToSurface1(WireToSurface1Pdu {
                    surface_id: 0,
                    codec_id: Codec1Type::Avc420,
                    pixel_format: GfxPixelFormat::XRgb,
                    destination_rectangle: make_dest_rect(),
                    bitmap_data: avc_data,
                })) {
                    raw_pdus.extend_from_slice(&data);
                }
            }
        }

        // EndFrame
        if let Ok(data) = encode_vec(&ServerPdu::EndFrame(EndFramePdu { frame_id })) {
            raw_pdus.extend_from_slice(&data);
        }

        debug!(
            frame_id,
            raw_bytes = raw_pdus.len(),
            h264_bytes = frame.h264_data.len(),
            "GFX frame PDU created",
        );

        // Single ZGFX wrap for all concatenated PDUs
        wrap_zgfx(&raw_pdus)
    }

    /// Create a ZGFX-wrapped buffer for uncompressed dirty rect updates.
    /// Uses WireToSurface1 with Codec1Type::Uncompressed for each rect.
    /// First call also includes surface setup PDUs.
    pub fn create_uncompressed_pdu(state: &mut GfxState, update: &GfxUncompressedUpdate) -> Vec<u8> {
        let mut raw_pdus = Vec::new();

        // Detect resolution change — rebuild surface
        if state.surface_created
            && (state.width != update.width || state.height != update.height)
        {
            info!(
                old_w = state.width, old_h = state.height,
                new_w = update.width, new_h = update.height,
                "GFX uncompressed resolution changed, rebuilding surface"
            );
            state.width = update.width;
            state.height = update.height;
            state.surface_created = false;
        }

        // First frame: surface setup (same as H.264 path)
        if !state.surface_created {
            if let Ok(data) = encode_vec(&ServerPdu::ResetGraphics(ResetGraphicsPdu {
                width: state.width as u32,
                height: state.height as u32,
                monitors: vec![Monitor {
                    left: 0,
                    top: 0,
                    right: state.width as i32 - 1,
                    bottom: state.height as i32 - 1,
                    flags: MonitorFlags::PRIMARY,
                }],
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            if let Ok(data) = encode_vec(&ServerPdu::CreateSurface(CreateSurfacePdu {
                surface_id: 0,
                width: update.width,
                height: update.height,
                pixel_format: GfxPixelFormat::XRgb,
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            if let Ok(data) = encode_vec(&ServerPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                surface_id: 0,
                output_origin_x: 0,
                output_origin_y: 0,
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            state.surface_created = true;
            info!("GFX surface created (uncompressed): {}x{}", update.width, update.height);
        }

        let frame_id = state.next_frame_id();

        // StartFrame
        if let Ok(data) = encode_vec(&ServerPdu::StartFrame(StartFramePdu {
            timestamp: Timestamp {
                milliseconds: 0,
                seconds: 0,
                minutes: 0,
                hours: 0,
            },
            frame_id,
        })) {
            raw_pdus.extend_from_slice(&data);
        }

        // WireToSurface1 per dirty rect with Codec1Type::Uncompressed
        let mut total_bytes = 0usize;
        for rect in &update.rects {
            if let Ok(data) = encode_vec(&ServerPdu::WireToSurface1(WireToSurface1Pdu {
                surface_id: 0,
                codec_id: Codec1Type::Uncompressed,
                pixel_format: GfxPixelFormat::XRgb,
                destination_rectangle: InclusiveRectangle {
                    left: rect.x,
                    top: rect.y,
                    right: rect.x + rect.width,
                    bottom: rect.y + rect.height,
                },
                bitmap_data: rect.pixel_data.to_vec(),
            })) {
                total_bytes += rect.pixel_data.len();
                raw_pdus.extend_from_slice(&data);
            }
        }

        // EndFrame
        if let Ok(data) = encode_vec(&ServerPdu::EndFrame(EndFramePdu { frame_id })) {
            raw_pdus.extend_from_slice(&data);
        }

        debug!(
            frame_id,
            rects = update.rects.len(),
            raw_bytes = total_bytes,
            "GFX uncompressed frame PDU created",
        );

        wrap_zgfx(&raw_pdus)
    }
}

impl DvcProcessor for GfxHandler {
    fn channel_name(&self) -> &str {
        GFX_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        info!(channel_id, "GFX channel opened");
        self.decompressor = ironrdp_graphics::zgfx::Decompressor::new();
        let mut state = self.state.lock().unwrap();
        state.channel_id = Some(channel_id);
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // Client GFX data is ZGFX-wrapped and may be compressed.
        let mut decompressed = Vec::new();
        let data: Vec<u8> = match self.decompressor.decompress(payload, &mut decompressed) {
            Ok(_) => decompressed,
            Err(e) => {
                debug!("GFX: ZGFX decompress failed ({e}), trying raw unwrap");
                unwrap_zgfx(payload).unwrap_or_else(|| payload.to_vec())
            }
        };

        // One message can carry several PDUs back to back
        let mut messages = Vec::new();
        let mut offset = 0;
        while offset + GFX_HEADER_SIZE <= data.len() {
            let len = u32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let end = if len >= GFX_HEADER_SIZE && offset + len <= data.len() { offset + len } else { data.len() };
            messages.extend(self.process_pdu(&data[offset..end])?);
            offset = end;
        }
        Ok(messages)
    }

    fn close(&mut self, channel_id: u32) {
        info!(channel_id, "GFX channel closed");
        let mut state = self.state.lock().unwrap();
        state.channel_id = None;
        state.surface_created = false;
        state.caps_confirmed = false;
        state.avc420_supported = false;
        state.avc444_supported = false;
        state.confirmed_cap = None;
    }
}

impl DvcServerProcessor for GfxHandler {}

impl GfxHandler {
    /// Handles one client PDU (header included).
    fn process_pdu(&mut self, data: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let cmd_id = u16::from_le_bytes([data[0], data[1]]);
        let client_pdu: ClientPdu = if cmd_id == CMDID_CAPS_ADVERTISE {
            ClientPdu::CapabilitiesAdvertise(CapabilitiesAdvertisePdu(parse_caps_advertise(&data[GFX_HEADER_SIZE..])))
        } else {
            match decode(data) {
                Ok(pdu) => pdu,
                Err(e) => {
                    // Unknown PDU type (e.g., QoE FrameAcknowledge 0x16, CacheImportOffer 0x10)
                    // Log and ignore rather than crashing the connection
                    debug!(
                        payload_len = data.len(),
                        first_bytes = ?&data[..data.len().min(8)],
                        "GFX: ignoring unknown client PDU: {e}"
                    );
                    return Ok(Vec::new());
                }
            }
        };

        match client_pdu {
            ClientPdu::CapabilitiesAdvertise(caps) => {
                let cap_sets = &caps.0;
                info!("GFX client capabilities: {} sets", cap_sets.len());

                let mut state = self.state.lock().unwrap();

                // If already confirmed, ignore duplicate CapabilitiesAdvertise
                if state.caps_confirmed {
                    info!("GFX caps already confirmed, ignoring duplicate CapabilitiesAdvertise");
                    return Ok(Vec::new());
                }

                let mut best_cap = None;

                for cap in cap_sets {
                    match cap {
                        CapabilitySet::V10_7 { flags } if !flags.contains(CapabilitiesV107Flags::AVC_DISABLED) => {
                            state.avc420_supported = true;
                            state.avc444_supported = true;
                            best_cap = Some(cap.clone());
                            break;
                        }
                        CapabilitySet::V10_6 { flags } | CapabilitySet::V10_6Err { flags }
                            if !flags.contains(CapabilitiesV104Flags::AVC_DISABLED) =>
                        {
                            state.avc420_supported = true;
                            state.avc444_supported = true;
                            best_cap = Some(cap.clone());
                        }
                        CapabilitySet::V10_5 { flags } | CapabilitySet::V10_4 { flags }
                            if !flags.contains(CapabilitiesV104Flags::AVC_DISABLED) =>
                        {
                            state.avc420_supported = true;
                            state.avc444_supported = true;
                            if best_cap.is_none() { best_cap = Some(cap.clone()); }
                        }
                        CapabilitySet::V10_3 { flags }
                            if !flags.contains(CapabilitiesV103Flags::AVC_DISABLED) =>
                        {
                            state.avc420_supported = true;
                            state.avc444_supported = true;
                            if best_cap.is_none() { best_cap = Some(cap.clone()); }
                        }
                        CapabilitySet::V10_2 { flags } | CapabilitySet::V10 { flags }
                            if !flags.contains(CapabilitiesV10Flags::AVC_DISABLED) =>
                        {
                            state.avc420_supported = true;
                            state.avc444_supported = true;
                            if best_cap.is_none() { best_cap = Some(cap.clone()); }
                        }
                        CapabilitySet::V10_1 => {
                            // V10_1 has no AVC_DISABLED flag — always supports AVC
                            state.avc420_supported = true;
                            state.avc444_supported = true;
                            if best_cap.is_none() { best_cap = Some(cap.clone()); }
                        }
                        CapabilitySet::V8_1 { flags }
                            if flags.contains(CapabilitiesV81Flags::AVC420_ENABLED) =>
                        {
                            state.avc420_supported = true;
                            // V8.1 only supports AVC420, not AVC444
                            if best_cap.is_none() { best_cap = Some(cap.clone()); }
                        }
                        _ => {
                            if best_cap.is_none() { best_cap = Some(cap.clone()); }
                        }
                    }
                }

                let confirmed = best_cap.unwrap_or(CapabilitySet::V8 {
                    flags: ironrdp_pdu::rdp::vc::dvc::gfx::CapabilitiesV8Flags::empty(),
                });

                info!(
                    avc420 = state.avc420_supported,
                    avc444_client = state.avc444_supported,
                    avc444_enabled = state.avc444_enabled,
                    "GFX capabilities negotiated"
                );

                // Send CapabilitiesConfirm from the DVC handler so it goes through
                // DrdynvcServer's proper encoding path. Bitmaps are suppressed once
                // GFX channel is open, so no bitmap/GFX mixing will occur.
                state.confirmed_cap = Some(confirmed.clone());
                state.caps_confirmed = true;

                let confirm_pdu = ServerPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu(confirmed));
                let msg = make_dvc_message(&confirm_pdu)?;
                info!("GFX CapabilitiesConfirm sent via DVC handler");
                Ok(vec![msg])
            }

            ClientPdu::FrameAcknowledge(ack) => {
                let mut state = self.state.lock().unwrap();
                if ack.queue_depth == QueueDepth::Suspend && !state.acks_suspended {
                    info!("Client suspended frame acknowledgements — flow control off");
                    state.suspend_acks();
                }
                state.ack_frame(ack.frame_id);

                // Log stats every 60 acked frames (~1 second at 60fps)
                if ack.frame_id % 60 == 0 {
                    let net_ms = (state.rtt_ewma_ms - state.last_encode_ms).max(0.0);

                    // Compute instant bitrate stats from samples since last log
                    let n = state.bitrate_samples.len() as f64;
                    let (inst_avg, inst_std) = if n > 0.0 {
                        let sum: f64 = state.bitrate_samples.iter().sum();
                        let avg = sum / n;
                        let var: f64 = state.bitrate_samples.iter().map(|x| (x - avg).powi(2)).sum::<f64>() / n;
                        (avg, var.sqrt())
                    } else {
                        (0.0, 0.0)
                    };
                    let inst_max = if state.bitrate_max > 0.0 { state.bitrate_max } else { 0.0 };
                    let inst_min = if state.bitrate_min < f64::MAX { state.bitrate_min } else { 0.0 };

                    info!(
                        "RTT {:.1}ms | encode {:.1}ms | net {:.1}ms | instant {:.1}/{:.1}/{:.1} Mbps (avg/max/min) | std {:.1} | {}KB/f | {} pending",
                        state.rtt_ewma_ms,
                        state.last_encode_ms,
                        net_ms,
                        inst_avg,
                        inst_max,
                        inst_min,
                        inst_std,
                        state.last_frame_bytes / 1024,
                        state.frames_in_flight(),
                    );

                    // Reset for next window
                    state.bitrate_samples.clear();
                    state.bitrate_max = 0.0;
                    state.bitrate_min = f64::MAX;
                }
                Ok(Vec::new())
            }
        }
    }
}

#[cfg(test)]
mod flow_control_tests {
    use super::GfxState;
    use std::time::Duration;

    const PATIENT: Duration = Duration::from_secs(60);

    #[test]
    fn holds_frames_while_client_is_behind() {
        let mut s = GfxState::new(1920, 1080, false);
        for _ in 0..4 {
            s.next_frame_id();
        }
        assert_eq!(s.frames_in_flight(), 4);
        assert!(s.client_is_behind(3, PATIENT));
        s.ack_frame(2);
        assert_eq!(s.frames_in_flight(), 2);
        assert!(!s.client_is_behind(3, PATIENT));
    }

    #[test]
    fn sparse_acks_do_not_drift() {
        let mut s = GfxState::new(1920, 1080, false);
        for _ in 0..10 {
            s.next_frame_id();
        }
        s.ack_frame(10); // acked the latest only
        assert_eq!(s.frames_in_flight(), 0);
        assert!(s.frame_send_times.is_empty());
        s.ack_frame(4); // a late, out-of-order ack can't move it backwards
        assert_eq!(s.frames_in_flight(), 0);
    }

    #[test]
    fn stalled_client_turns_flow_control_off() {
        let mut s = GfxState::new(1920, 1080, false);
        for _ in 0..5 {
            s.next_frame_id();
        }
        assert!(!s.client_is_behind(3, Duration::ZERO), "stalled client must not block");
        assert!(s.acks_suspended);
        assert_eq!(s.frames_in_flight(), 0);
        s.next_frame_id();
        assert!(s.frame_send_times.is_empty(), "no send-time tracking once suspended");
    }

    #[test]
    fn caps_advertise_skips_unknown_versions() {
        use super::{parse_caps_advertise, CapabilitySet};
        // count=3: an unknown future version, V10.7, V8.1
        let mut body = vec![3u8, 0];
        for (version, data) in [(0x000B_0000u32, vec![0u8; 12]), (0x000A_0701, vec![0; 4]), (0x0008_0105, vec![0x10, 0, 0, 0])] {
            body.extend_from_slice(&version.to_le_bytes());
            body.extend_from_slice(&(data.len() as u32).to_le_bytes());
            body.extend_from_slice(&data);
        }
        let sets = parse_caps_advertise(&body);
        assert_eq!(sets.len(), 2);
        assert!(matches!(sets[0], CapabilitySet::V10_7 { .. }));
        assert!(matches!(sets[1], CapabilitySet::V8_1 { .. }));
        // truncated input doesn't panic
        assert_eq!(parse_caps_advertise(&body[..11]).len(), 0);
    }
}
