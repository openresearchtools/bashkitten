pub mod agent;
pub mod auth;
pub mod config;
pub mod models;
pub mod oauth;
pub mod paths;
pub mod providers;
pub mod session;
pub mod tools;
pub mod web;
pub mod worker;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PI_REFERENCE_VERSION: &str = "0.85.0+astra";
pub const PI_REFERENCE_COMMIT: &str = "9841914c71a74d81abe07f751aefd271fd924e63";
