//! Sumimi service — the analytics core.
//!
//! The crate is split into a library (this file) and a thin binary so the HTTP API
//! can be integration-tested without spawning a process.

#![forbid(unsafe_code)]

pub mod analyst;
pub mod api;
pub mod auth;
pub mod channels;
pub mod config;
pub mod db;
pub mod error;
pub mod geo;
pub mod instance;
pub mod limits;
pub mod machine;
pub mod memcheck;
pub mod models;
pub mod pool;
pub mod queue;
pub mod reports;
pub mod rss;
pub mod server;
pub mod session;
pub mod sqlite;
pub mod storage;
pub mod static_files;
pub mod testing;
#[cfg(feature = "tls")]
pub mod tls;
pub mod token;
pub mod uptime;
pub mod ua;
pub mod writer;
