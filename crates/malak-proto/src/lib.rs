//! Malak protocol primitives driven by the caller's bytes.
//!
//! This crate provides the cryptographic exchange and authenticated segment
//! contents. The connection state machine decides which frames can act on a
//! board, and when possession of a key counts as evidence.

mod noise;

pub use noise::{
    Binding, Encrypted, Entrance, HandshakeError, Initiator, KeyPair, MAX_CONTENT, OPENING_BYTES,
    REPLY_BYTES, Responder, TAG_BYTES,
};

mod encoding;
mod framing;
pub use encoding::encode_frame;

pub use framing::{
    Frame, FrameDecoder, FramingError, MAX_HEADER, SegmentDecoder, SegmentKind, frame_prefix,
    max_frame_bytes,
};

mod connection;
mod reply_queue;

pub use connection::{
    Attachment, Connection, ConnectionError, ConnectionLimits, Input, Operation, Phase, Placement,
    Request, Ticket,
};

mod attachment;

pub use attachment::{AttachResponse, attach_id, attach_reply, attach_request, attach_step};

mod owner;

pub use owner::{
    OwnerConnection, OwnerLimits, OwnerOperation, OwnerOutcome, OwnerRequest, OwnerTicket,
    owner_outcome,
};
