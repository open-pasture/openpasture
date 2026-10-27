//! The v2 registries: every closed set on the wire is a small integer with a
//! name for the JSON view. The normative list, and how to add to it, is
//! `opencollar/protocol/registry.md`.
//!
//! A value this build doesn't know is kept as `Other(code)` and named
//! `"other"`; it never makes a whole message invalid.

macro_rules! registry {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $code:literal => $text:literal,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)*
            /// A code this build doesn't know.
            Other(u64),
        }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),*];

            pub fn code(self) -> u64 {
                match self {
                    $($name::$variant => $code,)*
                    $name::Other(c) => c,
                }
            }

            pub fn from_code(code: u64) -> Self {
                match code {
                    $($code => $name::$variant,)*
                    c => $name::Other(c),
                }
            }

            /// The JSON view: the name, or the number of a code this build
            /// doesn't know, so a reader that keeps only JSON keeps the code.
            pub fn to_json(self) -> serde_json::Value {
                match self {
                    $name::Other(c) => serde_json::Value::from(c),
                    known => serde_json::Value::from(known.name()),
                }
            }

            /// The name; `"other"` for unknown codes.
            pub fn name(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)*
                    $name::Other(_) => "other",
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($text => Some($name::$variant),)*
                    _ => None,
                }
            }

            pub fn is_known(self) -> bool {
                !matches!(self, $name::Other(_))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.name())
            }
        }
    };
}

registry! {
    /// Key 0 of every signed payload. 1–63 are commands (signed by the
    /// server or owner key, `herd` required); 64–127 are signed documents.
    MessageType {
        Boundary = 1 => "boundary",
        Config = 2 => "config",
        Membership = 3 => "membership",
        Roster = 4 => "roster",
        Bind = 5 => "bind",
        Handover = 6 => "handover",
        Confirm = 7 => "confirm",
        Owner = 8 => "owner",
        Firmware = 9 => "firmware",
        Resend = 10 => "resend",
        Release = 11 => "release",
        /// Permanent no-go zones the collar enforces under every boundary.
        Zones = 12 => "zones",
        /// "As of now, the farm's policy is still epoch N": lets staged
        /// boundaries take effect.
        Lease = 13 => "lease",
        /// Carries a signed trust update for the bootloader.
        Trust = 14 => "trust",
        Factory = 64 => "factory",
        Manifest = 65 => "manifest",
        Batch = 66 => "batch",
        Hello = 67 => "hello",
        /// A change to the firmware keys a bootloader trusts.
        TrustUpdate = 68 => "trust_update",
    }
}

impl MessageType {
    /// Commands: what a collar obeys. Documents are everything else signed.
    pub fn is_command(self) -> bool {
        (1..64).contains(&self.code())
    }
}

registry! {
    /// Element 0 of every uplink record.
    RecordType {
        Status = 1 => "status",
        Fixes = 2 => "fixes",
        Cue = 3 => "cue",
        Episode = 4 => "episode",
        Ack = 5 => "ack",
        Health = 6 => "health",
        Device = 7 => "device",
        Slots = 8 => "slots",
        Anchor = 9 => "anchor",
        Firmware = 10 => "firmware",
    }
}

registry! {
    #[derive(Default)]
    DeviceClass {
        /// LoRa only; most collars.
        #[default]
        Node = 1 => "node",
        /// LoRa and LTE-M, on an animal; takes turns as the herd's link.
        Anchor = 2 => "anchor",
        /// An anchor on a post or a building: no animal, no fence.
        Station = 3 => "station",
        /// LTE (or another IP link) straight to the server, no LoRa.
        Direct = 4 => "direct",
    }
}

registry! {
    /// A collar's part in its herd, from its membership.
    Role {
        Node = 1 => "node",
        Anchor = 2 => "anchor",
        Station = 3 => "station",
    }
}

registry! {
    FenceState {
        Unknown = 0 => "unknown",
        Inside = 1 => "inside",
        Warning = 2 => "warning",
        Outside = 3 => "outside",
    }
}

registry! {
    CueKind {
        Warn = 1 => "warn",
        Outside = 2 => "outside",
    }
}

registry! {
    CueMode {
        Audio = 0 => "audio",
        Track = 1 => "track",
    }
}

registry! {
    EpisodeOutcome {
        TurnedBack = 1 => "turned_back",
        Crossed = 2 => "crossed",
        Rest = 3 => "rest",
        BoundaryChanged = 4 => "boundary_changed",
    }
}

registry! {
    AckStatus {
        /// Stored; waits for its time (or, for a handover, its confirm).
        Received = 1 => "received",
        Applied = 2 => "applied",
        Rejected = 3 => "rejected",
    }
}

registry! {
    /// Where a firmware update stands on a collar.
    FirmwareState {
        Idle = 0 => "idle",
        Downloading = 1 => "downloading",
        Staged = 2 => "staged",
        /// Booted into the new image, not yet confirmed.
        Testing = 3 => "testing",
        Confirmed = 4 => "confirmed",
        RolledBack = 5 => "rolled_back",
        Refused = 6 => "refused",
    }
}

registry! {
    /// Why a collar refused a command: v1's codes keep their names, then v2's.
    Reject {
        BadSig = 1 => "bad_sig",
        WrongHerd = 2 => "wrong_herd",
        WrongCollar = 3 => "wrong_collar",
        BadJson = 4 => "bad_json",
        TooLarge = 5 => "too_large",
        Stale = 6 => "stale",
        OutOfRange = 7 => "out_of_range",
        TooFewVertices = 8 => "too_few_vertices",
        TooManyVertices = 9 => "too_many_vertices",
        TooManyHoles = 10 => "too_many_holes",
        SelfIntersecting = 11 => "self_intersecting",
        RingsCross = 12 => "rings_cross",
        HoleOutside = 13 => "hole_outside",
        HolesOverlap = 14 => "holes_overlap",
        ZeroArea = 15 => "zero_area",
        HoleTooSmall = 16 => "hole_too_small",
        HoleTooClose = 17 => "hole_too_close",
        BadMargins = 18 => "bad_margins",
        SlotsFull = 19 => "slots_full",
        BadConfig = 20 => "bad_config",
        /// Not deterministic CBOR, or the fields don't make the message.
        BadCbor = 21 => "bad_cbor",
        /// `expires_at` has passed on the collar's GNSS clock.
        Expired = 22 => "expired",
        /// Signed by a key the collar doesn't hold.
        UnknownKid = 23 => "unknown_kid",
        /// A message type this collar doesn't implement.
        Unsupported = 24 => "unsupported",
        /// A bind for a collar that is already claimed.
        Claimed = 25 => "claimed",
        /// A bind whose nonce or proof is wrong.
        BadClaim = 26 => "bad_claim",
        RateLimited = 27 => "rate_limited",
        /// Valid but not now (firmware: battery low, a move or escape running);
        /// the collar tries again by itself.
        NotNow = 28 => "not_now",
        /// A firmware image whose digest, size or signature is wrong.
        BadImage = 29 => "bad_image",
        /// Firmware for other hardware.
        Incompatible = 30 => "incompatible",
        /// Firmware older than what the collar runs.
        Downgrade = 31 => "downgrade",
        /// A boundary whose fenced area overlaps a permanent no-go zone the
        /// collar holds.
        InExclusion = 32 => "in_exclusion",
    }
}

impl Reject {
    /// Every code except `slots_full` and `not_now` is permanent: the server
    /// doesn't offer that version to that collar again (see `server.md` for
    /// the re-offer rules).
    pub fn is_permanent(self) -> bool {
        !matches!(self, Self::SlotsFull | Self::NotNow | Self::RateLimited)
    }

    /// The v1 code of the same name, if v1 has it.
    pub fn to_v1(self) -> Option<crate::RejectCode> {
        crate::RejectCode::parse(self.name())
    }
}

impl From<crate::RejectCode> for Reject {
    fn from(c: crate::RejectCode) -> Self {
        Self::from_name(c.as_str()).expect("every v1 code is in the v2 registry")
    }
}

impl From<op_geo::shape::ShapeCode> for Reject {
    fn from(c: op_geo::shape::ShapeCode) -> Self {
        crate::RejectCode::from(c).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_names_round_trip() {
        for r in Reject::ALL {
            assert_eq!(Reject::from_code(r.code()), *r);
            assert_eq!(Reject::from_name(r.name()), Some(*r));
        }
        assert_eq!(Reject::from_code(999), Reject::Other(999));
        assert_eq!(Reject::Other(999).name(), "other");
        assert_eq!(Reject::Other(999).to_json(), serde_json::json!(999), "the JSON view keeps the number");
        assert_eq!(Reject::BadSig.to_json(), serde_json::json!("bad_sig"));
        assert!(MessageType::Boundary.is_command() && !MessageType::Factory.is_command());
    }

    #[test]
    fn every_v1_code_keeps_its_name() {
        for c in crate::RejectCode::ALL {
            assert_eq!(Reject::from(c).to_v1(), Some(c));
        }
    }
}
