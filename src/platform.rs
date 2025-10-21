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
pub use futures::future::BoxFuture as PlatformFuture;

#[cfg(feature = "send")]
pub trait MaybeSend: Send {}
#[cfg(feature = "send")]
impl<T: Send> MaybeSend for T {}

#[cfg(feature = "send")]
pub trait MaybeSync: Sync {}
#[cfg(feature = "send")]
impl<T: Sync> MaybeSync for T {}

// Type alias for boxed futures with proper Send bounds
#[cfg(feature = "send")]
pub type PlatformBoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[cfg(feature = "send")]
pub type PlatformBoxFutureStatic<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;

// Type alias for boxed QueuingStrategy trait objects
#[cfg(feature = "send")]
pub type BoxedStrategy<T> = Box<dyn crate::streams::QueuingStrategy<T> + Send + 'static>;

#[cfg(feature = "send")]
pub type BoxedStrategyStatic<T> = Box<dyn crate::streams::QueuingStrategy<T> + Send>;

// ============================================================================
// SINGLE-THREADED (local feature)
// ============================================================================
#[cfg(feature = "local")]
pub use std::rc::Rc as SharedPtr;

#[cfg(feature = "local")]
pub use futures::future::LocalBoxFuture as PlatformFuture;

#[cfg(feature = "local")]
pub trait MaybeSend {}
#[cfg(feature = "local")]
impl<T> MaybeSend for T {}

#[cfg(feature = "local")]
pub trait MaybeSync {}
#[cfg(feature = "local")]
impl<T> MaybeSync for T {}

// Type alias for boxed futures without Send bounds for local feature
#[cfg(feature = "local")]
pub type PlatformBoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

#[cfg(feature = "local")]
pub type PlatformBoxFutureStatic<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'static>>;

// Type alias for boxed QueuingStrategy trait objects without Send
#[cfg(feature = "local")]
pub type BoxedStrategy<T> = Box<dyn crate::streams::QueuingStrategy<T> + 'static>;

#[cfg(feature = "local")]
pub type BoxedStrategyStatic<T> = Box<dyn crate::streams::QueuingStrategy<T>>;
