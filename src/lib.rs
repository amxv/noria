//! Noria's routing policy, native protocol adapters and local control plane.
pub mod app;
pub mod config;
pub mod core;
pub mod distribution;
pub mod integration;
pub mod model;
pub mod provider;
pub mod proxy;
pub mod service;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
