//! Private environment contract between the Git supervisor and SSH helper.

pub(crate) const SSH_EXECUTABLE_ENV: &str = "MAINCOPY_SSH_EXECUTABLE";
pub(crate) const PRIVATE_KEY_ENV: &str = "MAINCOPY_SSH_PRIVATE_KEY";
pub(crate) const KNOWN_HOSTS_ENV: &str = "MAINCOPY_SSH_KNOWN_HOSTS";
pub(crate) const EXPECTED_TARGET_ENV: &str = "MAINCOPY_SSH_EXPECTED_TARGET";
pub(crate) const EXPECTED_PORT_ENV: &str = "MAINCOPY_SSH_EXPECTED_PORT";
pub(crate) const EXPECTED_REPOSITORY_ENV: &str = "MAINCOPY_SSH_EXPECTED_REPOSITORY";
