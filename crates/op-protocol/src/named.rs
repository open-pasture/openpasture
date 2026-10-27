//! Lenient registry values on the v1 uplink. A collar reports cue kinds,
//! episode outcomes and ack statuses by name; a name this build doesn't know
//! (a newer collar's) is kept as sent instead of refusing the whole report.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{AckStatus, CueKind, EpisodeOutcome};

/// A closed set whose members have wire names.
pub trait RegistryName: Copy + Sized {
    fn name(self) -> &'static str;
    fn from_name(s: &str) -> Option<Self>;
}

impl RegistryName for CueKind {
    fn name(self) -> &'static str {
        self.as_str()
    }

    fn from_name(s: &str) -> Option<Self> {
        [Self::Warn, Self::Outside].into_iter().find(|k| k.as_str() == s)
    }
}

impl RegistryName for EpisodeOutcome {
    fn name(self) -> &'static str {
        self.as_str()
    }

    fn from_name(s: &str) -> Option<Self> {
        [Self::TurnedBack, Self::Crossed, Self::Rest, Self::BoundaryChanged].into_iter().find(|o| o.as_str() == s)
    }
}

impl RegistryName for AckStatus {
    fn name(self) -> &'static str {
        self.as_str()
    }

    fn from_name(s: &str) -> Option<Self> {
        Self::parse(s)
    }
}

/// One of a registry's values as the wire carries it: one this build knows,
/// or another, kept as sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Named<T> {
    Known(T),
    Other(String),
}

impl<T: RegistryName> Named<T> {
    pub fn parse(s: &str) -> Self {
        T::from_name(s).map_or_else(|| Self::Other(s.to_owned()), Self::Known)
    }

    /// The name as sent.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Known(t) => t.name(),
            Self::Other(s) => s,
        }
    }

    /// The value, if this build knows it.
    pub fn known(&self) -> Option<T> {
        match self {
            Self::Known(t) => Some(*t),
            Self::Other(_) => None,
        }
    }
}

impl<T> From<T> for Named<T> {
    fn from(t: T) -> Self {
        Self::Known(t)
    }
}

impl<T: Default> Default for Named<T> {
    fn default() -> Self {
        Self::Known(T::default())
    }
}

impl<T: PartialEq> PartialEq<T> for Named<T> {
    fn eq(&self, other: &T) -> bool {
        matches!(self, Self::Known(t) if t == other)
    }
}

impl<T: RegistryName> std::fmt::Display for Named<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<T: RegistryName> Serialize for Named<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de, T: RegistryName> Deserialize<'de> for Named<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(d)?))
    }
}

/// A held slot's status: `applied`, else `received` for anything else a
/// newer collar might send (it holds that version, not in effect).
pub(crate) fn held_status<'de, D: Deserializer<'de>>(d: D) -> Result<AckStatus, D::Error> {
    Ok(AckStatus::parse(&String::deserialize(d)?).unwrap_or(AckStatus::Received))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ack, PositionReport};

    #[test]
    fn unknown_names_are_kept_not_refused() {
        let rep: PositionReport = serde_json::from_str(
            r#"{"fixes":[],"cues":[{"at":"2026-09-27T12:00:00Z","kind":"pulse","level":2,"margin_m":-1.0}],
                "episodes":[{"start":"2026-09-27T12:00:00Z","end":"2026-09-27T12:00:05Z","ring":0,"cues":3,"max_level":2,"min_margin_m":0.5,"outcome":"herded"}],
                "slots":[{"version":3,"status":"deferred"}]}"#,
        )
        .unwrap();
        rep.validate().unwrap();
        assert_eq!(rep.cues[0].kind, Some(Named::Other("pulse".into())));
        assert_eq!(rep.episodes[0].outcome.as_str(), "herded");
        assert_eq!(rep.slots.unwrap()[0].status, AckStatus::Received);
        let back = serde_json::to_value(&rep.cues[0]).unwrap();
        assert_eq!(back["kind"], "pulse", "written back as sent");

        let ack: Ack = serde_json::from_str(r#"{"command_id":"b","version":1,"status":"queued","at":"2026-09-27T12:00:00Z"}"#).unwrap();
        assert_eq!((ack.status.known(), ack.status.as_str()), (None, "queued"));
        let ack: Ack = serde_json::from_str(r#"{"command_id":"b","version":1,"status":"applied","at":"2026-09-27T12:00:00Z"}"#).unwrap();
        assert!(ack.status == AckStatus::Applied);
    }
}
