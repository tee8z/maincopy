//! Bounded protected input from the controlling terminal, independent of stdin.
use std::io::{self, Read};

use maincopy_shared::auth_api::SecretString;
use zeroize::{Zeroize as _, Zeroizing};

pub(super) fn prompt_secret(prompt: &str) -> io::Result<SecretString> {
    prompt_bounded(prompt, 1024)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn prompt_bounded(prompt: &str, maximum_bytes: usize) -> io::Result<SecretString> {
    use std::fs::OpenOptions;
    let terminal = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    prompt_on_terminal(terminal, prompt, maximum_bytes)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn prompt_on_terminal(
    mut terminal: std::fs::File,
    prompt: &str,
    maximum_bytes: usize,
) -> io::Result<SecretString> {
    use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
    use std::io::Write as _;
    let original = tcgetattr(&terminal)?;
    let mut protected = original.clone();
    protected
        .local_modes
        .remove(LocalModes::ECHO | LocalModes::ECHONL | LocalModes::ICANON | LocalModes::ISIG);
    protected.special_codes[SpecialCodeIndex::VMIN] = 1;
    protected.special_codes[SpecialCodeIndex::VTIME] = 0;
    let mode = TerminalMode {
        terminal: terminal.try_clone()?,
        original,
    };
    tcsetattr(&terminal, OptionalActions::Now, &protected)?;
    write!(terminal, "{prompt}")?;
    terminal.flush()?;
    let result = read_protected_input(terminal.try_clone()?, maximum_bytes);
    drop(mode);
    writeln!(terminal)?;
    result
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn prompt_bounded(_prompt: &str, _maximum_bytes: usize) -> io::Result<SecretString> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "bounded protected terminal input is supported on Linux and macOS",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct TerminalMode {
    terminal: std::fs::File,
    original: rustix::termios::Termios,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for TerminalMode {
    fn drop(&mut self) {
        let _ = rustix::termios::tcflush(&self.terminal, rustix::termios::QueueSelector::IFlush);
        let _ = rustix::termios::tcsetattr(
            &self.terminal,
            rustix::termios::OptionalActions::Now,
            &self.original,
        );
    }
}

/// The allocation stays initialized at its maximum length until it is erased on drop.
struct SecretBuffer {
    bytes: Zeroizing<Vec<u8>>,
    used: usize,
}

impl SecretBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Zeroizing::new(vec![0; maximum]),
            used: 0,
        }
    }

    fn push(&mut self, byte: u8) -> io::Result<()> {
        let slot = self.bytes.get_mut(self.used).ok_or_else(input_too_large)?;
        *slot = byte;
        self.used += 1;
        Ok(())
    }

    fn erase_character(&mut self) {
        if self.used == 0 {
            return;
        }
        let mut start = self.used - 1;
        while start > 0 && self.bytes[start] & 0xc0 == 0x80 {
            start -= 1;
        }
        self.bytes[start..self.used].zeroize();
        self.used = start;
    }

    fn clear(&mut self) {
        self.bytes[..self.used].zeroize();
        self.used = 0;
    }

    fn erase_word(&mut self) {
        while self.used > 0 && self.bytes[self.used - 1].is_ascii_whitespace() {
            self.erase_character();
        }
        while self.used > 0 && !self.bytes[self.used - 1].is_ascii_whitespace() {
            self.erase_character();
        }
    }

    fn finish(self) -> io::Result<SecretString> {
        let value = std::str::from_utf8(&self.bytes[..self.used]).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "protected input must contain valid UTF-8",
            )
        })?;
        Ok(SecretString::new(value))
    }
}

fn read_protected_input(reader: impl Read, maximum: usize) -> io::Result<SecretString> {
    let mut reader = BoundedInput {
        reader,
        remaining: maximum.saturating_add(1),
    };
    let mut secret = SecretBuffer::new(maximum);
    loop {
        match read_byte(&mut reader)? {
            b'\n' | b'\r' => return secret.finish(),
            8 | 127 => secret.erase_character(),
            21 => secret.clear(),
            23 => secret.erase_word(),
            4 if secret.used == 0 => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            27 => discard_escape(&mut reader)?,
            byte if byte >= 32 => secret.push(byte)?,
            _ => {}
        }
    }
}

fn read_byte(reader: &mut impl Read) -> io::Result<u8> {
    let mut byte = Zeroizing::new([0_u8; 1]);
    if reader.read(&mut *byte)? == 0 {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    Ok(byte[0])
}

fn discard_escape(reader: &mut impl Read) -> io::Result<()> {
    if matches!(read_byte(reader)?, b'[' | b'O') {
        while !(0x40..=0x7e).contains(&read_byte(reader)?) {}
    }
    Ok(())
}

fn input_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "protected input exceeds the byte limit",
    )
}

struct BoundedInput<Reader> {
    reader: Reader,
    remaining: usize,
}

impl<Reader: Read> Read for BoundedInput<Reader> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(input_too_large());
        }
        let limit = buffer.len().min(self.remaining);
        let count = self.reader.read(&mut buffer[..limit])?;
        self.remaining -= count;
        // The caller returns normally so the terminal guard always restores echo.
        if buffer[..count].contains(&3) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "protected input cancelled",
            ));
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn protected_input_rejects_overflow_invalid_utf8_eof_and_interrupts_without_accepting_a_prefix()
    {
        for (input, maximum, expected) in [
            (
                b"correct horse battery staple\n".as_slice(),
                5,
                io::ErrorKind::InvalidData,
            ),
            (
                b"private fixture\x03\n".as_slice(),
                100,
                io::ErrorKind::Interrupted,
            ),
            (b"incomplete".as_slice(), 100, io::ErrorKind::UnexpectedEof),
            (b"\xff\n".as_slice(), 100, io::ErrorKind::InvalidData),
            (b"\x04".as_slice(), 100, io::ErrorKind::UnexpectedEof),
        ] {
            let error = read_protected_input(Cursor::new(input), maximum).unwrap_err();
            assert_eq!(error.kind(), expected);
            assert!(!error.to_string().contains("private fixture"));
        }
    }

    #[test]
    fn protected_input_preserves_unicode_and_editing_without_printing_or_reallocating_secrets() {
        for (input, expected) in [
            (
                "correct horse battery staple\n",
                "correct horse battery staple",
            ),
            ("🦀🦀🦀🦀\n", "🦀🦀🦀🦀"),
            ("🦀猫\x7f!\n", "🦀!"),
            ("discard this\x15replacement\n", "replacement"),
            ("first second  \x17third\n", "first third"),
            ("arrows\x1b[D ignored\n", "arrows ignored"),
            ("other\x1bX ignored\n", "other ignored"),
            ("\x08\tstart\x04\r", "start"),
        ] {
            let secret = read_protected_input(Cursor::new(input), input.len()).unwrap();
            assert_eq!(secret.expose_secret(), expected);
        }
        let secret = read_protected_input(Cursor::new("exact\n"), 5).unwrap();
        assert_eq!(secret.expose_secret(), "exact");
    }

    #[test]
    fn editing_erases_removed_bytes_and_keeps_the_fixed_allocation() {
        let mut secret = SecretBuffer::new(32);
        let allocation = secret.bytes.as_ptr();
        for byte in "first 🦀".as_bytes() {
            secret.push(*byte).unwrap();
        }
        secret.erase_character();
        assert_eq!(&secret.bytes[..secret.used], b"first ");
        assert!(secret.bytes[secret.used..].iter().all(|byte| *byte == 0));
        secret.erase_word();
        assert_eq!(secret.used, 0);
        assert!(secret.bytes.iter().all(|byte| *byte == 0));
        secret.push(b'x').unwrap();
        secret.clear();
        assert_eq!(secret.bytes.as_ptr(), allocation);
        assert!(secret.bytes.iter().all(|byte| *byte == 0));
        let mut empty = SecretBuffer::new(0);
        assert_eq!(
            empty.push(b'x').unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn protected_terminal_disables_echo_and_restores_it_after_success_cancel_and_overflow() {
        use rustix::{
            event::{PollFd, PollFlags, Timespec, poll},
            fs::{OFlags, fcntl_getfl, fcntl_setfl},
            pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt},
            termios::{LocalModes, tcgetattr},
        };
        use std::{fs::File, io::Write as _, sync::mpsc, thread, time::Duration};
        for (input, expected) in [
            ("fixture password\n", None),
            ("fixture\x03", Some(io::ErrorKind::Interrupted)),
            (
                "this secret input is deliberately much too long\n",
                Some(io::ErrorKind::InvalidData),
            ),
        ] {
            let mut master = File::from(
                openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).unwrap(),
            );
            grantpt(&master).unwrap();
            unlockpt(&master).unwrap();
            let slave = File::from(
                ioctl_tiocgptpeer(
                    &master,
                    OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
                )
                .unwrap(),
            );
            let original = tcgetattr(&slave).unwrap();
            let observed = slave.try_clone().unwrap();
            let (send, receive) = mpsc::sync_channel(1);
            let task = thread::spawn(move || {
                send.send(prompt_on_terminal(slave, "Secret: ", 20))
                    .unwrap();
            });
            let mut prompt = [0; 8];
            let mut read = 0;
            while read < prompt.len() {
                let mut poll_fds = [PollFd::new(&master, PollFlags::IN)];
                assert_eq!(
                    poll(
                        &mut poll_fds,
                        Some(&Timespec {
                            tv_sec: 5,
                            tv_nsec: 0
                        })
                    )
                    .unwrap(),
                    1
                );
                let count = master.read(&mut prompt[read..]).unwrap();
                assert_ne!(count, 0);
                read += count;
            }
            assert_eq!(&prompt, b"Secret: ");
            assert!(
                !tcgetattr(&observed)
                    .unwrap()
                    .local_modes
                    .contains(LocalModes::ECHO)
            );
            master.write_all(input.as_bytes()).unwrap();
            let result = receive.recv_timeout(Duration::from_secs(5)).unwrap();
            task.join().unwrap();
            match expected {
                Some(kind) => assert_eq!(result.unwrap_err().kind(), kind),
                None => assert_eq!(result.unwrap().expose_secret(), "fixture password"),
            }
            assert_eq!(
                tcgetattr(&observed).unwrap().local_modes,
                original.local_modes
            );
            fcntl_setfl(&master, fcntl_getfl(&master).unwrap() | OFlags::NONBLOCK).unwrap();
            let mut output = Vec::new();
            let _ = master.read_to_end(&mut output);
            assert_eq!(output, b"\r\n");
        }
    }
}
