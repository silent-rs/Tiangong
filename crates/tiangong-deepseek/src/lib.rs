pub mod balance;
pub mod chat;
pub mod client;
pub mod config;
pub mod dsml;
pub mod error;
pub mod files;
pub mod models;
pub mod responses;
pub mod types;

pub use client::DeepSeekClient;
pub use config::DeepSeekClientBuilder;
#[allow(deprecated)]
pub use config::DeepSeekConfig;
pub use error::DeepSeekError;

#[cfg(test)]
mod tests;
