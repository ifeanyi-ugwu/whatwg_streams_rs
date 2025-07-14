// Improved WHATWG Streams implementation in idiomatic Rust
use futures::FutureExt;
use futures::future::BoxFuture;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
// Re-exports for compatibility with futures ecosystem
pub use futures_core::Stream;
pub use futures_sink::Sink;
pub use futures_sink::Sink as FuturesSink;

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

/// ReadableStream with typestate to prevent misuse
pub struct ReadableStream<T, S = Unlocked> {
    inner: ReadableStreamInner<T>,
    _state: PhantomData<S>,
}

struct ReadableStreamInner<T> {
    state: StreamState,
    controller: ReadableStreamDefaultController<T>,
    // Additional fields would go here
}

impl<T: Send + 'static> ReadableStream<T, Unlocked> {
    /// Create a new ReadableStream with the given source
    pub fn new<Source>(source: Source) -> Self
    where
        Source: ReadableSource<T> + Send + 'static,
    {
        let controller = ReadableStreamDefaultController::new();
        Self {
            inner: ReadableStreamInner {
                state: StreamState::Readable,
                controller,
            },
            _state: PhantomData,
        }
    }

    /// Get a reader for this stream, consuming the unlocked stream and returning a locked one
    pub fn get_reader(self) -> (ReadableStream<T, Locked>, ReadableStreamDefaultReader<T>) {
        let locked_stream = ReadableStream {
            inner: self.inner,
            _state: PhantomData,
        };
        let reader = ReadableStreamDefaultReader::new();
        (locked_stream, reader)
    }

    /// Tee this stream into two streams
    pub fn tee(self) -> (ReadableStream<T, Unlocked>, ReadableStream<T, Unlocked>) {
        // Implementation would clone the underlying source
        unimplemented!("Tee requires complex internal buffering")
    }

    /// Cancel this stream
    pub async fn cancel(mut self, reason: Option<String>) -> StreamResult<()> {
        self.inner.state = StreamState::Closed;
        Ok(())
    }
}

impl<T: Send + 'static> ReadableStream<T, Locked> {
    /// Release the lock, returning an unlocked stream
    pub fn release_lock(self) -> ReadableStream<T, Unlocked> {
        ReadableStream {
            inner: self.inner,
            _state: PhantomData,
        }
    }
}

// Implement futures::Stream for ReadableStream to integrate with ecosystem
impl<T: Send + 'static, S> Stream for ReadableStream<T, S> {
    type Item = StreamResult<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // This would delegate to the internal controller/source
        // For now, just return pending
        Poll::Pending
    }
}

/// Controller for a ReadableStream using GATs instead of boxed futures
pub struct ReadableStreamDefaultController<T> {
    desired_size: Option<usize>,
    _marker: PhantomData<T>,
}

impl<T: Send + 'static> ReadableStreamDefaultController<T> {
    fn new() -> Self {
        Self {
            desired_size: Some(1),
            _marker: PhantomData,
        }
    }

    /// Enqueue a chunk to the stream
    pub fn enqueue(&mut self, _chunk: T) -> StreamResult<()> {
        // Implementation would add to internal queue
        Ok(())
    }

    /// Close the stream
    pub fn close(&mut self) -> StreamResult<()> {
        self.desired_size = None;
        Ok(())
    }

    /// Error the stream
    pub fn error(&mut self, _error: StreamError) {
        self.desired_size = None;
    }

    /// Get the desired size for the stream's internal queue
    pub fn desired_size(&self) -> Option<usize> {
        self.desired_size
    }
}

/// Source trait using GATs instead of boxed futures
pub trait ReadableSource<T: Send + 'static> {
    type StartFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    type PullFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    type CancelFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    /// Called when the stream is started
    fn start<'a>(
        &'a mut self,
        controller: &'a mut ReadableStreamDefaultController<T>,
    ) -> Self::StartFuture<'a>;

    /// Called when the stream needs more data
    fn pull<'a>(
        &'a mut self,
        controller: &'a mut ReadableStreamDefaultController<T>,
    ) -> Self::PullFuture<'a>;

    /// Called when the stream is canceled
    fn cancel<'a>(&'a mut self, reason: Option<String>) -> Self::CancelFuture<'a>;
}

/// Pollable trait for sources that want to work with futures ecosystem
pub trait PollableSource<T: Send + 'static> {
    fn poll_pull(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> Poll<StreamResult<()>>;

    fn poll_cancel(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        reason: Option<String>,
    ) -> Poll<StreamResult<()>>;
}

/// Reader for a ReadableStream with clean async API
pub struct ReadableStreamDefaultReader<T> {
    _marker: PhantomData<T>,
}

impl<T: Send + 'static> ReadableStreamDefaultReader<T> {
    fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }

    /// Read a chunk from the stream using natural async/await
    pub async fn read(&mut self) -> StreamResult<Option<T>> {
        // Implementation would poll the underlying stream
        Ok(None)
    }

    /// Cancel the stream
    pub async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
        Ok(())
    }

    /// Return a future that resolves when the stream is closed
    pub async fn closed(&self) -> StreamResult<()> {
        Ok(())
    }
}

// ============================================================================
// WRITABLE STREAMS
// ============================================================================

/// WritableStream with typestate
pub struct WritableStream<T, S = Unlocked> {
    inner: WritableStreamInner<T>,
    _state: PhantomData<S>,
}

struct WritableStreamInner<T> {
    state: StreamState,
    controller: WritableStreamDefaultController,
    _marker: PhantomData<T>,
}

impl<T: Send + 'static> WritableStream<T, Unlocked> {
    /// Create a new WritableStream
    pub fn new<Sink>(sink: Sink) -> Self
    where
        Sink: WritableSink<T> + Send + 'static,
    {
        Self {
            inner: WritableStreamInner {
                state: StreamState::Readable,
                controller: WritableStreamDefaultController::new(),
                _marker: PhantomData,
            },
            _state: PhantomData,
        }
    }

    /// Get a writer, consuming unlocked stream and returning locked one
    pub fn get_writer(self) -> (WritableStream<T, Locked>, WritableStreamDefaultWriter<T>) {
        let locked_stream = WritableStream {
            inner: self.inner,
            _state: PhantomData,
        };
        let writer = WritableStreamDefaultWriter::new();
        (locked_stream, writer)
    }

    /// Abort this stream
    pub async fn abort(mut self, _reason: Option<String>) -> StreamResult<()> {
        self.inner.state = StreamState::Errored;
        Ok(())
    }
}

// Implement futures::Sink for WritableStream
impl<T: Send + 'static, S> FuturesSink<T> for WritableStream<T, S> {
    type Error = StreamError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, _item: T) -> Result<(), Self::Error> {
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// Controller for WritableStream
pub struct WritableStreamDefaultController {
    // Implementation details
}

impl WritableStreamDefaultController {
    fn new() -> Self {
        Self {}
    }

    /// Error the stream
    pub fn error(&mut self, _error: StreamError) {
        // Implementation
    }
}

/// Sink trait using GATs
pub trait WritableSink<T: Send + 'static> {
    type StartFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    type WriteFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    type CloseFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    type AbortFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    fn start<'a>(
        &'a mut self,
        controller: &'a mut WritableStreamDefaultController,
    ) -> Self::StartFuture<'a>;
    fn write<'a>(
        &'a mut self,
        chunk: T,
        controller: &'a mut WritableStreamDefaultController,
    ) -> Self::WriteFuture<'a>;
    fn close<'a>(&'a mut self) -> Self::CloseFuture<'a>;
    fn abort<'a>(&'a mut self, reason: Option<String>) -> Self::AbortFuture<'a>;
}

/// Writer with clean async API
pub struct WritableStreamDefaultWriter<T> {
    _marker: PhantomData<T>,
}

impl<T: Send + 'static> WritableStreamDefaultWriter<T> {
    fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }

    /// Write a chunk using async/await
    pub async fn write(&mut self, _chunk: T) -> StreamResult<()> {
        Ok(())
    }

    /// Close the stream
    pub async fn close(&mut self) -> StreamResult<()> {
        Ok(())
    }

    /// Abort the stream
    pub async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
        Ok(())
    }

    /// Check if ready for more data
    pub async fn ready(&self) -> StreamResult<()> {
        Ok(())
    }

    /// Wait for stream to close
    pub async fn closed(&self) -> StreamResult<()> {
        Ok(())
    }
}

// ============================================================================
// TRANSFORM STREAMS
// ============================================================================

/// TransformStream connecting readable and writable sides
pub struct TransformStream<I, O> {
    readable: ReadableStream<O, Unlocked>,
    writable: WritableStream<I, Unlocked>,
}

impl<I: Send + 'static, O: Send + 'static> TransformStream<I, O> {
    /// Create a new TransformStream
    pub fn new<T>(transformer: T) -> Self
    where
        T: Transformer<I, O> + Send + 'static,
    {
        // Implementation would wire transformer between readable/writable sides
        unimplemented!("TransformStream requires complex internal coordination")
    }

    /// Get the readable side
    pub fn readable(self) -> ReadableStream<O, Unlocked> {
        self.readable
    }

    /// Get the writable side  
    pub fn writable(self) -> WritableStream<I, Unlocked> {
        self.writable
    }

    /// Split into both sides
    pub fn split(self) -> (ReadableStream<O, Unlocked>, WritableStream<I, Unlocked>) {
        (self.readable, self.writable)
    }
}

/// Controller for transform operations
pub struct TransformStreamDefaultController<O> {
    _marker: PhantomData<O>,
}

impl<O: Send + 'static> TransformStreamDefaultController<O> {
    /// Enqueue to readable side
    pub fn enqueue(&mut self, _chunk: O) -> StreamResult<()> {
        Ok(())
    }

    /// Error both sides
    pub fn error(&mut self, _error: StreamError) {
        // Implementation
    }

    /// Terminate both sides
    pub fn terminate(&mut self) {
        // Implementation
    }

    /// Get desired size
    pub fn desired_size(&self) -> Option<usize> {
        Some(1)
    }
}

/// Transformer trait using GATs
pub trait Transformer<I: Send + 'static, O: Send + 'static> {
    type StartFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    type TransformFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    type FlushFuture<'a>: Future<Output = StreamResult<()>> + Send + 'a
    where
        Self: 'a;

    fn start<'a>(
        &'a mut self,
        controller: &'a mut TransformStreamDefaultController<O>,
    ) -> Self::StartFuture<'a>;
    fn transform<'a>(
        &'a mut self,
        chunk: I,
        controller: &'a mut TransformStreamDefaultController<O>,
    ) -> Self::TransformFuture<'a>;
    fn flush<'a>(
        &'a mut self,
        controller: &'a mut TransformStreamDefaultController<O>,
    ) -> Self::FlushFuture<'a>;
}

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
