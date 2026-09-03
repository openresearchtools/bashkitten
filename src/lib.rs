pub mod agent;
pub mod auth;
pub mod config;
pub mod models;
pub mod paths;
pub mod providers;
pub mod session;
pub mod tools;
pub mod web;
pub mod worker;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PI_REFERENCE_VERSION: &str = "0.84.4";
pub const PI_REFERENCE_COMMIT: &str = "b79e4cc834970cca69daebffab7df1da7d1e52c4";
