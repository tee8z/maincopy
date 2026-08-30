#[path = "../build_support/frontend_digest.rs"]
mod frontend_digest_contract;

pub mod admin;
mod cli;
mod clock;
pub mod config;
pub mod content;
pub mod distribution;
pub mod error;
pub mod frontend_assets;
pub mod jobs;
mod observability;
pub mod payments;
pub mod render;
pub mod startup;
pub mod web;

#[cfg(test)]
#[path = "../build_support/frontend.rs"]
mod frontend_build_support;
#[cfg(test)]
#[path = "../build_support/frontend_io.rs"]
mod frontend_io;
