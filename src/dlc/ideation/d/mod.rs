pub mod readable;
pub mod transform;
pub mod writable;
pub mod writable_new;

use futures::FutureExt;
use futures::future::BoxFuture;
use readable::ReadableStreamDefaultController;
use std::error::Error;
use std::fmt;
use transform::TransformStreamDefaultController;

// Re-exports for compatibility with futures ecosystem
pub use futures_core::Stream;
pub use futures_sink::Sink;
pub use futures_sink::Sink as FuturesSink;

pub use readable::{ReadableSource, ReadableStream, ReadableStreamDefaultReader};
pub use transform::{TransformStream, Transformer};
pub use writable::{WritableSink, WritableStream, WritableStreamDefaultWriter};

/// Generic error type for stream operations
#[derive(Debug)]
pub enum StreamError {
    Canceled,
    TypeError(String),
    NetworkError(String),
    Custom(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Canceled => write!(f, "Stream was canceled"),
            StreamError::TypeError(msg) => write!(f, "Type error: {}", msg),
            StreamError::NetworkError(msg) => write!(f, "Network error: {}", msg),
            StreamError::Custom(err) => write!(f, "Custom error: {}", err),
        }
    }
}

impl Error for StreamError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            StreamError::Custom(err) => Some(err.as_ref()),
            _ => None,
        }
    }
}

// Automatic conversions from common error types
impl From<std::io::Error> for StreamError {
    fn from(e: std::io::Error) -> Self {
        StreamError::Custom(Box::new(e))
    }
}

impl From<Box<dyn Error + Send + Sync>> for StreamError {
    fn from(e: Box<dyn Error + Send + Sync>) -> Self {
        StreamError::Custom(e)
    }
}

/// Result of a stream operation
pub type StreamResult<T> = Result<T, StreamError>;

/// Status of a stream
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Readable,
    Closed,
    Errored,
}

// ============================================================================
// TYPESTATE DESIGN - Prevents invalid usage at compile time
// ============================================================================

/// Type-level marker for unlocked streams
pub struct Unlocked;
/// Type-level marker for locked streams  
pub struct Locked;

// ============================================================================
// QUEUING STRATEGIES - Zero-cost with generics
// ============================================================================

/// Generic queuing strategy trait
pub trait QueuingStrategy<T> {
    fn size(&self, chunk: &T) -> usize;
    fn high_water_mark(&self) -> usize;
}

/// Count-based strategy (zero-cost)
pub struct CountQueuingStrategy {
    high_water_mark: usize,
}

impl CountQueuingStrategy {
    pub const fn new(high_water_mark: usize) -> Self {
        Self { high_water_mark }
    }
}

impl<T> QueuingStrategy<T> for CountQueuingStrategy {
    fn size(&self, _chunk: &T) -> usize {
        1
    }

    fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }
}

/// Byte length strategy for types with known byte sizes
pub struct ByteLengthQueuingStrategy {
    high_water_mark: usize,
}

impl ByteLengthQueuingStrategy {
    pub const fn new(high_water_mark: usize) -> Self {
        Self { high_water_mark }
    }
}

impl QueuingStrategy<Vec<u8>> for ByteLengthQueuingStrategy {
    fn size(&self, chunk: &Vec<u8>) -> usize {
        chunk.len()
    }

    fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }
}

impl QueuingStrategy<String> for ByteLengthQueuingStrategy {
    fn size(&self, chunk: &String) -> usize {
        chunk.len()
    }

    fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }
}

impl QueuingStrategy<&[u8]> for ByteLengthQueuingStrategy {
    fn size(&self, chunk: &&[u8]) -> usize {
        chunk.len()
    }

    fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }
}

// ============================================================================
// PIPE OPERATIONS
// ============================================================================

/// Options for piping operations
pub struct PipeOptions {
    pub prevent_close: bool,
    pub prevent_abort: bool,
    pub prevent_cancel: bool,
    pub signal: Option<AbortSignal>,
}

impl Default for PipeOptions {
    fn default() -> Self {
        Self {
            prevent_close: false,
            prevent_abort: false,
            prevent_cancel: false,
            signal: None,
        }
    }
}

/// Abort signal for cancelling operations
pub struct AbortSignal {
    aborted: bool,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self { aborted: false }
    }

    pub fn aborted(&self) -> bool {
        self.aborted
    }

    pub fn abort(&mut self) {
        self.aborted = true;
    }
}

/// Extension trait for piping operations
pub trait PipeExt<T: Send + 'static> {
    /// Pipe to a writable stream
    async fn pipe_to(
        self,
        writable: WritableStream<T, Unlocked>,
        options: PipeOptions,
    ) -> StreamResult<()>;

    /// Pipe through a transform stream
    fn pipe_through<O: Send + 'static>(
        self,
        transform: TransformStream<T, O>,
        options: PipeOptions,
    ) -> ReadableStream<O, Unlocked>;
}

impl<T: Send + 'static> PipeExt<T> for ReadableStream<T, Unlocked> {
    async fn pipe_to(
        self,
        _writable: WritableStream<T, Unlocked>,
        _options: PipeOptions,
    ) -> StreamResult<()> {
        // Implementation would connect reader to writer
        Ok(())
    }

    fn pipe_through<O: Send + 'static>(
        self,
        transform: TransformStream<T, O>,
        _options: PipeOptions,
    ) -> ReadableStream<O, Unlocked> {
        // Implementation would pipe self to transform.writable and return transform.readable
        let (readable, _writable) = transform.split();
        readable
    }
}

// ============================================================================
// EXAMPLE IMPLEMENTATIONS
// ============================================================================

/// Example counter source using async fn instead of boxed futures
pub struct CounterSource {
    current: u32,
    max: u32,
}

impl CounterSource {
    pub fn new(max: u32) -> Self {
        Self { current: 0, max }
    }
}

impl ReadableSource<u32> for CounterSource {
    /*type StartFuture<'a> = impl Future<Output = StreamResult<()>> + Send + 'a;
    type PullFuture<'a> = impl Future<Output = StreamResult<()>> + Send + 'a;
    type CancelFuture<'a> = impl Future<Output = StreamResult<()>> + Send + 'a;


    fn start<'a>(
        &'a mut self,
        _controller: &'a mut ReadableStreamDefaultController<u32>,
    ) -> Self::StartFuture<'a> {
        async move { Ok(()) }
    }

    fn pull<'a>(
        &'a mut self,
        controller: &'a mut ReadableStreamDefaultController<u32>,
    ) -> Self::PullFuture<'a> {
        async move {
            if self.current < self.max {
                controller.enqueue(self.current)?;
                self.current += 1;
                Ok(())
            } else {
                controller.close()
            }
        }
    }

    fn cancel<'a>(&'a mut self, _reason: Option<String>) -> Self::CancelFuture<'a> {
        async move { Ok(()) }
    }
    */

    type StartFuture<'a> = BoxFuture<'a, StreamResult<()>>;
    type PullFuture<'a> = BoxFuture<'a, StreamResult<()>>;
    type CancelFuture<'a> = BoxFuture<'a, StreamResult<()>>;

    fn start<'a>(
        &'a mut self,
        _controller: &'a mut ReadableStreamDefaultController<u32>,
    ) -> Self::StartFuture<'a> {
        Box::pin(async move { Ok(()) })
    }

    fn pull<'a>(
        &'a mut self,
        controller: &'a mut ReadableStreamDefaultController<u32>,
    ) -> Self::PullFuture<'a> {
        Box::pin(async move {
            if self.current < self.max {
                controller.enqueue(self.current)?;
                self.current += 1;
                Ok(())
            } else {
                controller.close()
            }
        })
    }

    fn cancel<'a>(&'a mut self, _reason: Option<String>) -> Self::CancelFuture<'a> {
        Box::pin(async move { Ok(()) })
    }
}

/// Example transformer from numbers to strings
pub struct NumberToStringTransformer;

/*impl Transformer<u32, String> for NumberToStringTransformer {
   type StartFuture<'a> = impl Future<Output = StreamResult<()>> + Send + 'a;
    type TransformFuture<'a> = impl Future<Output = StreamResult<()>> + Send + 'a;
    type FlushFuture<'a> = impl Future<Output = StreamResult<()>> + Send + 'a;

    fn start<'a>(
        &'a mut self,
        _controller: &'a mut TransformStreamDefaultController<String>,
    ) -> Self::StartFuture<'a> {
        async move { Ok(()) }
    }

    fn transform<'a>(
        &'a mut self,
        chunk: u32,
        controller: &'a mut TransformStreamDefaultController<String>,
    ) -> Self::TransformFuture<'a> {
        async move {
            controller.enqueue(format!("Number: {}", chunk))?;
            Ok(())
        }
    }

    fn flush<'a>(
        &'a mut self,
        _controller: &'a mut TransformStreamDefaultController<String>,
    ) -> Self::FlushFuture<'a> {
        async move { Ok(()) }
    }
}*/

impl Transformer<u32, String> for NumberToStringTransformer {
    type StartFuture<'a> = BoxFuture<'a, StreamResult<()>>;
    type TransformFuture<'a> = BoxFuture<'a, StreamResult<()>>;
    type FlushFuture<'a> = BoxFuture<'a, StreamResult<()>>;

    fn start<'a>(
        &'a mut self,
        _controller: &'a mut TransformStreamDefaultController<String>,
    ) -> Self::StartFuture<'a> {
        async move { Ok(()) }.boxed()
    }

    fn transform<'a>(
        &'a mut self,
        chunk: u32,
        controller: &'a mut TransformStreamDefaultController<String>,
    ) -> Self::TransformFuture<'a> {
        async move {
            controller.enqueue(format!("Number: {}", chunk))?;
            Ok(())
        }
        .boxed()
    }

    fn flush<'a>(
        &'a mut self,
        _controller: &'a mut TransformStreamDefaultController<String>,
    ) -> Self::FlushFuture<'a> {
        async move { Ok(()) }.boxed()
    }
}
