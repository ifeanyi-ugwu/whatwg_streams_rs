//! WHATWG Streams implementation for Rust
//!
//! This crate provides an implementation of the WHATWG Streams Standard in Rust.
//!
//! ## Features
//!
//! - **`send` (default)**: Multi-threaded streams using `Arc` (requires `Send + Sync`)
//! - **`local`**: Single-threaded streams using `Rc` (no `Send + Sync` required)
//!
//! ## Examples
//!
//! ### Multi-threaded (default)
//!
//! ```toml
//! [dependencies]
//! whatwg_streams = "0.1"
//! ```
//!
//! ### Single-threaded (WASM or LocalSet)
//!
//! ```toml
//! [dependencies]
//! whatwg_streams = { version = "0.1", default-features = false, features = ["local"] }
//! ```

// Ensure mutual exclusion of features
#[cfg(all(feature = "send", feature = "local"))]
compile_error!(
    "Features 'send' and 'local' are mutually exclusive.\n\
     For multi-threaded: cargo build --features send\n\
     For single-threaded: cargo build --no-default-features --features local"
);

// Ensure at least one feature is enabled
#[cfg(not(any(feature = "send", feature = "local")))]
compile_error!(
    "Must enable either 'send' or 'local' feature.\n\
     For multi-threaded (default): cargo build\n\
     For single-threaded: cargo build --no-default-features --features local"
);

// Platform abstraction layer
mod platform;

// Unified streams implementation
pub mod streams;

// Re-export everything from streams
pub use streams::*;
