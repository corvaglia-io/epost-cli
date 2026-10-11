//! Unofficial, read-only ePost inbox and archive retrieval.
//! No provider credentials are necessary to run the synthetic test suite.

pub mod api;
pub mod config;
pub mod credentials;
pub mod error;
pub mod keychain;
pub mod models;
pub mod pagination;
pub mod progress;
pub mod secret;
pub mod storage;
pub mod summary;
