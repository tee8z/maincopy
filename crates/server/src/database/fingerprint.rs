use uuid::Uuid;

/// Length-framed command fields for durable idempotency receipts. Callers own
/// the action name, field order, and any domain-specific alternative tags.
pub(crate) struct CommandFingerprintBuilder(blake3::Hasher);

impl CommandFingerprintBuilder {
    pub(crate) fn new(action: &'static str) -> Self {
        let mut builder = Self(blake3::Hasher::new());
        builder.field(action.as_bytes());
        builder
    }

    pub(crate) fn field(&mut self, value: &[u8]) {
        self.0.update(&(value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    pub(crate) fn optional_field(&mut self, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                self.field(b"some");
                self.field(value);
            }
            None => self.field(b"none"),
        }
    }

    pub(crate) fn uuid(&mut self, value: &Uuid) {
        self.field(value.as_bytes());
    }

    pub(crate) fn version(&mut self, value: u64) {
        self.field(&value.to_be_bytes());
    }

    pub(crate) fn boolean(&mut self, value: bool) {
        self.field(&[u8::from(value)]);
    }

    pub(crate) fn finish(self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}
