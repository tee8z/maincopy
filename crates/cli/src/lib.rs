//! Command-side contracts for Maincopy's administration API.

mod client;
mod credentials;
mod models;
mod nip98;
mod startup;
mod transport;

pub use startup::run;
