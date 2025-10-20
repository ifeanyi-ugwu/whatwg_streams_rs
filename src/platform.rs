//! Platform-specific type aliases and trait bounds
//!
//! This module provides conditional compilation for single-threaded (Rc-based)
//! vs multi-threaded (Arc-based) implementations.
//!
//! - `send` feature (default): Uses Arc and requires Send+Sync bounds
//! - `local` feature: Uses Rc and removes Send+Sync requirements

// ============================================================================
// MULTI-THREADED (send feature - default)
// ============================================================================
#[cfg(feature = "send")]
pub use std::sync::Arc as SharedPtr;

#[cfg(feature = "send")]
pub trait MaybeSend: Send {}
#[cfg(feature = "send")]
impl<T: Send> MaybeSend for T {}

#[cfg(feature = "send")]
pub trait MaybeSync: Sync {}
#[cfg(feature = "send")]
impl<T: Sync> MaybeSync for T {}

// ============================================================================
// SINGLE-THREADED (local feature)
// ============================================================================
#[cfg(feature = "local")]
pub use std::rc::Rc as SharedPtr;

#[cfg(feature = "local")]
pub trait MaybeSend {}
#[cfg(feature = "local")]
impl<T> MaybeSend for T {}

#[cfg(feature = "local")]
pub trait MaybeSync {}
#[cfg(feature = "local")]
impl<T> MaybeSync for T {}
