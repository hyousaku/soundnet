//! Hand-written FFI bindings for libroc 0.3.x.
//!
//! Only the surface we actually need in `soundnet-engine` is exposed:
//! context lifecycle, sender/receiver open/close, endpoint URI setup,
//! bind/connect, frame read/write. Keep in sync with `/usr/include/roc/*.h`.

#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_char, c_int, c_uint, c_void};

pub type roc_slot = u64;
pub const ROC_SLOT_DEFAULT: roc_slot = 0;

#[repr(C)]
pub struct roc_context {
    _private: [u8; 0],
}
#[repr(C)]
pub struct roc_sender {
    _private: [u8; 0],
}
#[repr(C)]
pub struct roc_receiver {
    _private: [u8; 0],
}
#[repr(C)]
pub struct roc_endpoint {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_interface {
    ROC_INTERFACE_CONSOLIDATED = 1,
    ROC_INTERFACE_AUDIO_SOURCE = 11,
    ROC_INTERFACE_AUDIO_REPAIR = 12,
    ROC_INTERFACE_AUDIO_CONTROL = 13,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_protocol {
    ROC_PROTO_RTSP = 10,
    ROC_PROTO_RTP = 20,
    ROC_PROTO_RTP_RS8M_SOURCE = 30,
    ROC_PROTO_RS8M_REPAIR = 31,
    ROC_PROTO_RTP_LDPC_SOURCE = 32,
    ROC_PROTO_LDPC_REPAIR = 33,
    ROC_PROTO_RTCP = 70,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_packet_encoding {
    ROC_PACKET_ENCODING_AVP_L16_MONO = 11,
    ROC_PACKET_ENCODING_AVP_L16_STEREO = 10,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_fec_encoding {
    ROC_FEC_ENCODING_DISABLE = -1,
    ROC_FEC_ENCODING_DEFAULT = 0,
    ROC_FEC_ENCODING_RS8M = 1,
    ROC_FEC_ENCODING_LDPC_STAIRCASE = 2,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_format {
    ROC_FORMAT_PCM_FLOAT32 = 1,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_channel_layout {
    ROC_CHANNEL_LAYOUT_MULTITRACK = 1,
    ROC_CHANNEL_LAYOUT_MONO = 2,
    ROC_CHANNEL_LAYOUT_STEREO = 3,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct roc_media_encoding {
    pub rate: c_uint,
    pub format: roc_format,
    pub channels: roc_channel_layout,
    pub tracks: c_uint,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_clock_source {
    ROC_CLOCK_SOURCE_EXTERNAL = 0,
    ROC_CLOCK_SOURCE_INTERNAL = 1,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_clock_sync_backend {
    ROC_CLOCK_SYNC_BACKEND_DISABLE = -1,
    ROC_CLOCK_SYNC_BACKEND_DEFAULT = 0,
    ROC_CLOCK_SYNC_BACKEND_NIQ = 2,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_clock_sync_profile {
    ROC_CLOCK_SYNC_PROFILE_DEFAULT = 0,
    ROC_CLOCK_SYNC_PROFILE_RESPONSIVE = 1,
    ROC_CLOCK_SYNC_PROFILE_GRADUAL = 2,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_resampler_backend {
    ROC_RESAMPLER_BACKEND_DEFAULT = 0,
    ROC_RESAMPLER_BACKEND_BUILTIN = 1,
    ROC_RESAMPLER_BACKEND_SPEEX = 2,
    ROC_RESAMPLER_BACKEND_SPEEXDEC = 3,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum roc_resampler_profile {
    ROC_RESAMPLER_PROFILE_DEFAULT = 0,
    ROC_RESAMPLER_PROFILE_HIGH = 1,
    ROC_RESAMPLER_PROFILE_MEDIUM = 2,
    ROC_RESAMPLER_PROFILE_LOW = 3,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct roc_context_config {
    pub max_packet_size: c_uint,
    pub max_frame_size: c_uint,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct roc_sender_config {
    pub frame_encoding: roc_media_encoding,
    pub packet_encoding: c_int, // roc_packet_encoding OR custom registered id (0 = auto)
    pub packet_length: u64,
    pub packet_interleaving: c_uint,
    pub fec_encoding: roc_fec_encoding,
    pub fec_block_source_packets: c_uint,
    pub fec_block_repair_packets: c_uint,
    pub clock_source: roc_clock_source,
    pub resampler_backend: roc_resampler_backend,
    pub resampler_profile: roc_resampler_profile,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct roc_receiver_config {
    pub frame_encoding: roc_media_encoding,
    pub clock_source: roc_clock_source,
    pub clock_sync_backend: roc_clock_sync_backend,
    pub clock_sync_profile: roc_clock_sync_profile,
    pub resampler_backend: roc_resampler_backend,
    pub resampler_profile: roc_resampler_profile,
    pub target_latency: u64,
    pub latency_tolerance: u64,
    pub no_playback_timeout: i64,
    pub choppy_playback_timeout: i64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct roc_interface_config {
    pub outgoing_address: [c_char; 48],
    pub multicast_group: [c_char; 48],
    pub reuse_address: c_int,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct roc_session_metrics {
    pub niq_latency: u64,
    pub e2e_latency: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct roc_receiver_metrics {
    pub num_sessions: c_uint,
    pub sessions: *mut roc_session_metrics,
    pub sessions_size: usize,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct roc_sender_metrics {
    pub unused: c_int,
}

#[repr(C)]
pub struct roc_frame {
    pub samples: *mut c_void,
    pub samples_size: usize,
}

extern "C" {
    // ---- context ----
    pub fn roc_context_open(
        config: *const roc_context_config,
        result: *mut *mut roc_context,
    ) -> c_int;
    pub fn roc_context_register_encoding(
        context: *mut roc_context,
        encoding_id: c_int,
        encoding: *const roc_media_encoding,
    ) -> c_int;
    pub fn roc_context_close(context: *mut roc_context) -> c_int;

    // ---- endpoint ----
    pub fn roc_endpoint_allocate(result: *mut *mut roc_endpoint) -> c_int;
    pub fn roc_endpoint_set_uri(endpoint: *mut roc_endpoint, uri: *const c_char) -> c_int;
    pub fn roc_endpoint_set_protocol(endpoint: *mut roc_endpoint, proto: roc_protocol) -> c_int;
    pub fn roc_endpoint_set_host(endpoint: *mut roc_endpoint, host: *const c_char) -> c_int;
    pub fn roc_endpoint_set_port(endpoint: *mut roc_endpoint, port: c_int) -> c_int;
    pub fn roc_endpoint_get_port(endpoint: *const roc_endpoint, port: *mut c_int) -> c_int;
    pub fn roc_endpoint_deallocate(endpoint: *mut roc_endpoint) -> c_int;

    // ---- sender ----
    pub fn roc_sender_open(
        context: *mut roc_context,
        config: *const roc_sender_config,
        result: *mut *mut roc_sender,
    ) -> c_int;
    pub fn roc_sender_configure(
        sender: *mut roc_sender,
        slot: roc_slot,
        iface: roc_interface,
        config: *const roc_interface_config,
    ) -> c_int;
    pub fn roc_sender_connect(
        sender: *mut roc_sender,
        slot: roc_slot,
        iface: roc_interface,
        endpoint: *const roc_endpoint,
    ) -> c_int;
    pub fn roc_sender_unlink(sender: *mut roc_sender, slot: roc_slot) -> c_int;
    pub fn roc_sender_write(sender: *mut roc_sender, frame: *const roc_frame) -> c_int;
    pub fn roc_sender_close(sender: *mut roc_sender) -> c_int;

    // ---- receiver ----
    pub fn roc_receiver_open(
        context: *mut roc_context,
        config: *const roc_receiver_config,
        result: *mut *mut roc_receiver,
    ) -> c_int;
    pub fn roc_receiver_configure(
        receiver: *mut roc_receiver,
        slot: roc_slot,
        iface: roc_interface,
        config: *const roc_interface_config,
    ) -> c_int;
    pub fn roc_receiver_bind(
        receiver: *mut roc_receiver,
        slot: roc_slot,
        iface: roc_interface,
        endpoint: *mut roc_endpoint,
    ) -> c_int;
    pub fn roc_receiver_unlink(receiver: *mut roc_receiver, slot: roc_slot) -> c_int;
    pub fn roc_receiver_read(receiver: *mut roc_receiver, frame: *mut roc_frame) -> c_int;
    pub fn roc_receiver_close(receiver: *mut roc_receiver) -> c_int;
}
