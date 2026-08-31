#[path = "../build_support/frontend_digest.rs"]
mod frontend_digest_contract;

pub mod admin;
mod cli;
pub mod config;
pub mod content;
mod content_sync;
mod database;
pub mod domain;
pub mod error;
pub mod frontend_assets;
mod observability;
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
