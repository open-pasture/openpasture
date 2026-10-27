//! The OpenCollar protocol, v2 (**draft**): one message set for LoRa herds
//! with anchor collars, LTE anchors and direct collars, specified in
//! `opencollar/protocol/v2/`.
//!
//! - Messages are deterministic CBOR ([`cbor`]) with integer keys and no
//!   floats. Their registries are in [`registry`].
//! - Commands go down as COSE_Sign1 by the server key ([`cose`], [`command`])
//!   and travel as opaque bytes over any transport.
//! - Records go up ([`record`]), each with the collar's sequence number,
//!   sealed in frames with AES-128-CCM under a key only the collar and its
//!   server share ([`frame`], [`crypto`]).
//! - The LoRa binding splits big messages into fragments ([`fragment`]) and
//!   gives every collar an airtime slot from its short address and GNSS time
//!   ([`tdma`]). Anchors post what they hear in signed batches ([`batch`]).
//! - Collars are identified by their own key and claimed with a code on
//!   their card ([`identity`]); firmware comes as signed releases ([`firmware`]).
//!
//! v1 (HTTP + JSON) stays as it is in the crate root.

pub mod batch;
pub mod cbor;
pub mod command;
pub mod cose;
pub mod crypto;
pub mod firmware;
pub mod fragment;
pub mod frame;
pub mod identity;
pub mod json;
pub mod record;
pub mod registry;
pub mod tdma;

pub use command::{Body, Command};
pub use record::{Record, RecordBody};
pub use registry::Reject;
