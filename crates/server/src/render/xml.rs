use std::io;

/// XML writers share an inclusive byte limit before appending any output.
pub(super) struct BoundedOutput {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl BoundedOutput {
    pub(super) const fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
        }
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl io::Write for BoundedOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(buffer.len())
            .is_none_or(|length| length > self.max_bytes)
        {
            return Err(io::Error::other("XML output limit reached"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) const fn is_xml_1_0_character(character: char) -> bool {
    matches!(
        character,
        '\u{0009}' | '\u{000A}' | '\u{000D}'
            | '\u{0020}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}'
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn bounded_output_accepts_its_inclusive_limit_and_rejects_the_next_byte() {
        let mut output = BoundedOutput::new(4);
        output.write_all(b"1234").unwrap();
        assert_eq!(output.bytes, b"1234");
        assert!(output.write_all(b"5").is_err());
        assert_eq!(output.into_bytes(), b"1234");
    }
}
