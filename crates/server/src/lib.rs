#[path = "../build_support/frontend_digest.rs"]
mod frontend_digest_contract;

pub mod admin;
mod cli;
pub mod config;
pub mod content;
mod database;
pub mod distribution;
pub mod error;
pub mod frontend_assets;
pub mod jobs;
mod observability;
pub mod payments;
mod process_lock;
pub mod render;
pub mod startup;
pub mod web;

#[cfg(test)]
#[path = "../build_support/frontend.rs"]
mod frontend_build_support;
#[cfg(test)]
#[path = "../build_support/frontend_io.rs"]
mod frontend_io;
