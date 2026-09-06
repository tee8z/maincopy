use blake3::Hasher;
use time::{OffsetDateTime, UtcOffset};

/// Length framing shared by the compiler's versioned identity formats.
/// Callers select the context, kind, version, and ordered fields explicitly.
pub(super) struct Transcript(Hasher);

impl Transcript {
    pub(super) fn new(context: &'static str, kind: &[u8], version: u16) -> Self {
        let mut transcript = Self(Hasher::new_derive_key(context));
        transcript.bytes(kind);
        transcript.0.update(&version.to_be_bytes());
        transcript
    }

    pub(super) fn finish(self) -> blake3::Hash {
        self.0.finalize()
    }

    pub(super) fn bytes(&mut self, bytes: &[u8]) {
        self.0.update(&(bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }

    pub(super) fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    pub(super) fn fixed_bytes(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub(super) fn sequence_len(&mut self, length: usize) {
        self.0.update(&(length as u64).to_be_bytes());
    }

    pub(super) fn tag(&mut self, tag: u8) {
        self.0.update(&[tag]);
    }

    pub(super) fn optional<T>(&mut self, value: Option<T>, encode: impl FnOnce(&mut Self, T)) {
        match value {
            Some(value) => {
                self.tag(1);
                encode(self, value);
            }
            None => self.tag(0),
        }
    }

    pub(super) fn authored_timestamp(&mut self, timestamp: OffsetDateTime) {
        self.0.update(&timestamp.unix_timestamp().to_be_bytes());
        self.0.update(&timestamp.nanosecond().to_be_bytes());
        self.0
            .update(&timestamp.offset().whole_seconds().to_be_bytes());
    }

    pub(super) fn utc_timestamp(&mut self, timestamp: OffsetDateTime) {
        let timestamp = timestamp.to_offset(UtcOffset::UTC);
        self.0.update(&timestamp.unix_timestamp().to_be_bytes());
        self.0.update(&timestamp.nanosecond().to_be_bytes());
    }
}
