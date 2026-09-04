#[path = "../build_support/frontend_digest.rs"]
mod frontend_digest_contract;

mod admin;
mod cli;
pub mod config;
#[cfg(test)]
mod content_fixtures;
mod content_sync;
mod database;
pub mod domain;
pub mod error;
pub mod frontend_assets;
mod git_ssh_contract;
mod git_sync;
mod identity_bootstrap;
mod observability;
mod password_executor;
mod process_lock;
pub mod render;
mod source_bootstrap;
mod source_provenance;
mod source_sync;
pub mod startup;
pub mod web;

#[cfg(test)]
#[path = "../build_support/frontend.rs"]
mod frontend_build_support;
#[cfg(test)]
#[path = "../build_support/frontend_io.rs"]
mod frontend_io;
