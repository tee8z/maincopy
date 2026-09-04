#[cfg(feature = "client")]
use std::time::Duration;

#[cfg(any(feature = "client", feature = "helper"))]
pub(crate) const PROTOCOL_VERSION: &str = "maincopy-mermaid-v1";
#[cfg(any(feature = "client", feature = "helper"))]
pub(crate) const DETERMINISTIC_ENVIRONMENT: &str = "maincopy-fontless-v1";
#[cfg(any(feature = "client", feature = "helper"))]
pub(crate) const MAX_SOURCE_BYTES: usize = 256 * 1024;
#[cfg(any(feature = "client", feature = "helper"))]
pub(crate) const MAX_RAW_SVG_BYTES: usize = 2 * 1024 * 1024;
#[cfg(feature = "helper")]
pub(crate) const MAX_ADDRESS_SPACE_BYTES: u64 = 512 * 1024 * 1024;
#[cfg(feature = "helper")]
pub(crate) const MAX_STACK_BYTES: u64 = 16 * 1024 * 1024;
#[cfg(feature = "helper")]
pub(crate) const MAX_CPU_SECONDS: u64 = 5;
#[cfg(feature = "client")]
pub(crate) const MAX_WALL_TIME: Duration = Duration::from_secs(10);

#[cfg(any(feature = "client", feature = "helper"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum HelperExit {
    Success = 0,
    Usage = 64,
    InvalidDiagram = 65,
    InputRejected = 66,
    CannotCreate = 73,
    Io = 74,
    ResourceLimit = 75,
    Internal = 70,
}

#[cfg(any(feature = "client", feature = "helper"))]
impl HelperExit {
    #[cfg(feature = "helper")]
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }
}

#[cfg(any(feature = "client", feature = "helper"))]
impl TryFrom<i32> for HelperExit {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Success),
            64 => Ok(Self::Usage),
            65 => Ok(Self::InvalidDiagram),
            66 => Ok(Self::InputRejected),
            70 => Ok(Self::Internal),
            73 => Ok(Self::CannotCreate),
            74 => Ok(Self::Io),
            75 => Ok(Self::ResourceLimit),
            _ => Err(()),
        }
    }
}
