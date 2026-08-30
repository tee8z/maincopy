//! Concrete client for Maincopy's private local admin API.

#[cfg(any(unix, windows))]
mod client;

#[cfg(any(unix, windows))]
pub use client::{AdminClient, AdminClientError};
