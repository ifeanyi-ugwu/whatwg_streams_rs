use super::{Locked, StreamError, StreamResult, StreamState, Unlocked};
use futures_sink::Sink as FuturesSink;
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

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
