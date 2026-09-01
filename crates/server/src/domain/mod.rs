//! Maincopy business domains and their concrete boundary adapters.
//!
//! A domain owns its models and the web, administration, and persistence code
//! that implements operations on those models. Top-level infrastructure
//! modules provide shared mechanisms and compose the domain adapters.

pub(crate) mod auth;
pub(crate) mod profile;
pub mod publication;
