use std::{ffi::OsString, path::PathBuf};

use thiserror::Error;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Invocation {
    Render { input: PathBuf, output: PathBuf },
    ProtocolVersion { output: PathBuf },
}

impl Invocation {
    pub(crate) fn parse_process_arguments() -> Result<Self, InvocationError> {
        Self::parse(std::env::args_os().skip(1))
    }

    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, InvocationError> {
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        match arguments.as_slice() {
            [flag, output] if flag == "--protocol-version" => Ok(Self::ProtocolVersion {
                output: PathBuf::from(output),
            }),
            [input, output] => Ok(Self::Render {
                input: PathBuf::from(input),
                output: PathBuf::from(output),
            }),
            _ => Err(InvocationError),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("expected --protocol-version or exactly one input path and one output path")]
pub(crate) struct InvocationError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_only_the_fixed_internal_protocol() {
        assert_eq!(
            Invocation::parse([
                OsString::from("--protocol-version"),
                OsString::from("protocol.txt"),
            ])
            .unwrap(),
            Invocation::ProtocolVersion {
                output: PathBuf::from("protocol.txt")
            }
        );
        assert_eq!(
            Invocation::parse([OsString::from("input.mmd"), OsString::from("output.svg")]).unwrap(),
            Invocation::Render {
                input: PathBuf::from("input.mmd"),
                output: PathBuf::from("output.svg")
            }
        );
        for rejected in [
            Vec::new(),
            vec![OsString::from("input.mmd")],
            vec![
                OsString::from("input.mmd"),
                OsString::from("output.svg"),
                OsString::from("extra"),
            ],
        ] {
            assert_eq!(Invocation::parse(rejected).unwrap_err(), InvocationError);
        }
    }
}
