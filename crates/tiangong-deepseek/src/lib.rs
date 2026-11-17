pub mod asr;
pub mod balance;
pub mod chat;
pub mod client;
pub mod config;
pub mod dsml;
pub mod error;
pub mod files;
pub mod models;
pub mod ocr;
pub(crate) mod protocol_buffer;
pub mod responses;
pub mod tools;
pub mod tts;
pub mod types;

pub use client::DeepSeekClient;
pub use config::DeepSeekClientBuilder;
#[allow(deprecated)]
pub use config::DeepSeekConfig;
pub use error::DeepSeekError;

#[cfg(test)]
mod app_suite_tests;
#[cfg(test)]
mod responses_stream_tests;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
