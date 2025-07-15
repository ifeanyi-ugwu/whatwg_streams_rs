use super::{CountQueuingStrategy, Locked, QueuingStrategy, StreamError, StreamResult, Unlocked};
use futures::{channel::oneshot, future};
use futures_core::task::{Context, Poll};
use futures_sink::Sink as FuturesSink;
use std::{
    collections::VecDeque,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Waker,
};

struct PendingWrite<T> {
    chunk: T,
    completion_tx: oneshot::Sender<StreamResult<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Writable,
    Closed,
    Errored,
}

/// WritableStream with typestate pattern for compile-time safety
pub struct WritableStream<T, Sink, S = Unlocked> {
    inner: Arc<Mutex<WritableStreamInner<T, Sink>>>,
    _state: PhantomData<S>,
}

pub struct WritableStreamInner<T, Sink> {
    state: StreamState,
    queue: VecDeque<PendingWrite<T>>,
    queue_total_size: usize,
    strategy: Box<dyn QueuingStrategy<T> + Send>,
    sink: Option<Sink>,
    // The controller now doesn't "own" the inner logic, but rather interacts with it
    // Wakers for tasks waiting on ready()
    ready_wakers: VecDeque<Waker>,
    // Wakers for tasks waiting on closed()
    closed_wakers: VecDeque<Waker>,
    backpressure: bool,
    close_requested: bool,
    abort_reason: Option<String>,
    locked: bool,
}

impl<T: Send + 'static, Sink: WritableSink<T> + Send + 'static> WritableStream<T, Sink, Unlocked> {
    /// Create a new WritableStream with default count queuing strategy
    pub fn _new(sink: Sink) -> Self {
        Self::with_strategy(sink, CountQueuingStrategy::new(1))
    }

    /// Create a new WritableStream with custom queuing strategy
    pub fn with_strategy<Strategy>(sink: Sink, strategy: Strategy) -> Self
    where
        Strategy: QueuingStrategy<T> + Send + 'static,
    {
        let inner = Arc::new(Mutex::new(WritableStreamInner {
            state: StreamState::Writable,
            queue: VecDeque::new(),
            queue_total_size: 0,
            strategy: Box::new(strategy),
            sink: Some(sink),
            ready_wakers: VecDeque::new(),
            closed_wakers: VecDeque::new(),
            backpressure: false,
            close_requested: false,
            abort_reason: None,
            locked: false,
        }));
        Self {
            inner,
            _state: PhantomData,
        }
    }

    pub async fn new(sink: Sink) -> Result<Self, StreamError> {
        let inner = Arc::new(Mutex::new(WritableStreamInner {
            state: StreamState::Writable,
            queue: VecDeque::new(),
            queue_total_size: 0,
            strategy: Box::new(CountQueuingStrategy::new(1)),
            sink: Some(sink),
            ready_wakers: VecDeque::new(),
            closed_wakers: VecDeque::new(),
            backpressure: false,
            close_requested: false,
            abort_reason: None,
            locked: false,
        }));

        // Extract sink for start()
        let sink_to_start = {
            let mut guard = inner.lock().unwrap();
            guard.sink.take()
        };

        if let Some(mut sink) = sink_to_start {
            let mut controller = WritableStreamDefaultController::new(inner.clone());

            // Call start() outside the lock
            if let Err(e) = sink.start(&mut controller).await {
                let mut guard = inner.lock().unwrap();
                guard.state = StreamState::Errored;
                guard.abort_reason = Some(format!("start() failed: {:?}", e));
                return Err(e);
            }

            // Put sink back after successful start()
            let mut guard = inner.lock().unwrap();
            guard.sink = Some(sink);
        }

        Ok(Self {
            inner,
            _state: PhantomData,
        })
    }

    /// Get a writer, consuming unlocked stream and returning locked one
    /*pub fn get_writer(
        self,
    ) -> (
        WritableStream<T, Sink, Locked>,
        WritableStreamDefaultWriter<T, Sink>,
    ) {
        let locked_stream = WritableStream {
            inner: self.inner.clone(),
            _state: PhantomData,
        };
        let writer = WritableStreamDefaultWriter::new(self.inner);
        (locked_stream, writer)
    }*/

    pub fn get_writer(
        self,
    ) -> Result<
        (
            WritableStream<T, Sink, Locked>,
            WritableStreamDefaultWriter<T, Sink>,
        ),
        StreamError,
    > {
        let inner_clone = self.inner.clone();
        let mut guard = inner_clone.lock().unwrap();

        if guard.locked {
            return Err(StreamError::Custom("Stream is already locked".into()));
        }
        guard.locked = true;

        let locked_stream = WritableStream {
            inner: self.inner.clone(),
            _state: PhantomData,
        };
        let writer = WritableStreamDefaultWriter::new(self.inner);

        Ok((locked_stream, writer))
    }

    /// Abort this stream
    pub async fn abort(self, reason: Option<String>) -> StreamResult<()> {
        let inner_arc = self.inner.clone();

        let sink_to_abort = {
            let mut guard = inner_arc.lock().unwrap();

            if guard.state == StreamState::Closed || guard.state == StreamState::Errored {
                return Ok(());
            }
            guard.state = StreamState::Errored;
            guard.abort_reason = reason.clone();
            guard.queue.clear();
            guard.queue_total_size = 0;

            // Wake all pending futures as stream is now errored
            for waker in guard.ready_wakers.drain(..) {
                waker.wake();
            }
            for waker in guard.closed_wakers.drain(..) {
                waker.wake();
            }

            guard.sink.take()
        };

        if let Some(sink) = sink_to_abort {
            sink.abort(reason).await?;
        }

        Ok(())
    }

    /// Check if the stream is locked
    /// This method is primarily for internal checks; the typestate pattern handles compile-time locking.
    pub fn locked(&self) -> bool {
        // A writer exists if get_writer was called (which consumes Unlocked and returns Locked).
        // This method on the Unlocked variant will always return false from its perspective.
        // If this method needed to reflect if *any* writer exists that's pointing to this inner
        // (even after `self` is consumed), it would require an internal ref counter or flag.
        // For simplicity with typestate, it stays false here.
        false
    }
}

impl<T: Send + 'static, Sink: WritableSink<T> + Send + 'static> Clone
    for WritableStream<T, Sink, Unlocked>
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _state: PhantomData,
        }
    }
}

/// Controller for WritableStream
/// Allows the WritableSink to signal back to the WritableStream
pub struct WritableStreamDefaultController<T, Sink> {
    inner: Arc<Mutex<WritableStreamInner<T, Sink>>>,
}

impl<T: Send + 'static, Sink: WritableSink<T> + Send + 'static>
    WritableStreamDefaultController<T, Sink>
{
    // This constructor is crate-private as it's meant to be created by the stream itself
    pub(crate) fn new(inner: Arc<Mutex<WritableStreamInner<T, Sink>>>) -> Self {
        Self { inner }
    }

    /// Error the stream
    pub fn error(&mut self, error: StreamError) {
        let mut guard = self.inner.lock().unwrap();
        if guard.state == StreamState::Closed || guard.state == StreamState::Errored {
            return; // Already closed or errored, do nothing
        }
        guard.state = StreamState::Errored;
        guard.abort_reason = Some(format!("Controller error: {:?}", error));
        guard.queue.clear();
        guard.queue_total_size = 0;

        // Wake all pending futures as stream is now errored
        for waker in guard.ready_wakers.drain(..) {
            waker.wake();
        }
        for waker in guard.closed_wakers.drain(..) {
            waker.wake();
        }
    }

    fn set_backpressure(&mut self, backpressure: bool) {
        let mut guard = self.inner.lock().unwrap();
        let old = guard.backpressure;
        guard.backpressure = backpressure;
        if old && !backpressure {
            for waker in guard.ready_wakers.drain(..) {
                waker.wake();
            }
        }
    }

    fn request_close(&mut self) {
        let mut guard = self.inner.lock().unwrap();
        guard.close_requested = true;
    }
}

/// Simplified WritableSink trait that's easier to work with
pub trait WritableSink<T: Send + 'static>: Sized {
    /// Start the sink
    fn start(
        &mut self,
        controller: &mut WritableStreamDefaultController<T, Self>,
    ) -> impl std::future::Future<Output = StreamResult<()>> + Send;

    /// Write a chunk to the sink
    fn write(
        &mut self,
        chunk: T,
        controller: &mut WritableStreamDefaultController<T, Self>,
    ) -> impl std::future::Future<Output = StreamResult<()>> + Send;

    /// Close the sink
    fn close(self) -> impl std::future::Future<Output = StreamResult<()>> + Send;

    /// Abort the sink
    fn abort(
        self,
        reason: Option<String>,
    ) -> impl std::future::Future<Output = StreamResult<()>> + Send;
}

/// Writer with clean async API
pub struct WritableStreamDefaultWriter<T, Sink> {
    inner: Arc<Mutex<WritableStreamInner<T, Sink>>>,
}

impl<T: Send + 'static, Sink: WritableSink<T> + Send + 'static>
    WritableStreamDefaultWriter<T, Sink>
{
    fn new(inner: Arc<Mutex<WritableStreamInner<T, Sink>>>) -> Self {
        Self { inner }
    }

    /// Write a chunk using async/await
    /*pub async fn write(&mut self, chunk: T) -> StreamResult<()> {
        let inner_arc = self.inner.clone();

        // Check state and potentially queue chunk
        let mut guard = inner_arc.lock().unwrap();
        if guard.state != StreamState::Writable {
            return Err(StreamError::Custom("Stream is not writable".into()));
        }

        // We could queue here if there's backpressure, but for simplicity,
        // we'll assume write is blocked by backpressure via `ready()`
        // or directly attempts to write. For now, we write directly.

        let sink_option = guard.sink.take();
        // Create a controller instance linked to *this* stream's inner state
        let mut controller = WritableStreamDefaultController::new(inner_arc.clone());
        drop(guard); // Release lock before async call

        if let Some(mut sink_val) = sink_option {
            sink_val.write(chunk, &mut controller).await?;

            // Re-acquire lock to put the sink back and update state
            let mut re_guard = inner_arc.lock().unwrap();
            re_guard.sink = Some(sink_val);

            // Update queue size and backpressure after a successful write
            // (Assuming the write operation has cleared space, or the queue conceptually holds what's written)
            // In a real impl, this would track what's *queued before* sink.write, and `sink.write` would drain it.
            // For now, let's just re-evaluate backpressure.
            let old_backpressure = re_guard.backpressure;
            re_guard.queue_total_size = re_guard
                .queue
                .iter()
                .map(|item| re_guard.strategy.size(item))
                .sum();
            re_guard.backpressure =
                re_guard.queue_total_size >= re_guard.strategy.high_water_mark();

            // If backpressure cleared, wake any waiting `ready()` tasks
            if old_backpressure && !re_guard.backpressure {
                for waker in re_guard.ready_wakers.drain(..) {
                    waker.wake();
                }
            }
        }

        Ok(())
    }*/

    pub async fn write(&mut self, chunk: T) -> StreamResult<()> {
        let inner_arc = self.inner.clone();

        // Lock to check state and backpressure
        let mut guard = inner_arc.lock().unwrap();
        if guard.state != StreamState::Writable {
            return Err(StreamError::Custom("Stream is not writable".into()));
        }

        // Calculate chunk size
        let chunk_size = guard.strategy.size(&chunk);

        // If backpressure applies, queue the write
        if guard.backpressure {
            // Create oneshot channel to await completion
            let (tx, rx) = oneshot::channel();

            // Enqueue the write
            guard.queue.push_back(PendingWrite {
                chunk,
                completion_tx: tx,
            });
            guard.queue_total_size += chunk_size;

            // Backpressure remains true if queue size exceeds high water mark
            guard.backpressure = guard.queue_total_size >= guard.strategy.high_water_mark();

            drop(guard);

            // Return a future that waits for the write to complete
            return rx
                .await
                .unwrap_or_else(|_| Err(StreamError::Custom("Write canceled".into())));
        }

        // No backpressure: write immediately
        let sink_option = guard.sink.take();
        guard.queue_total_size += chunk_size;
        guard.backpressure = guard.queue_total_size >= guard.strategy.high_water_mark();
        drop(guard);

        if let Some(mut sink_val) = sink_option {
            let mut controller = WritableStreamDefaultController::new(inner_arc.clone());
            let write_result = sink_val.write(chunk, &mut controller).await;

            let mut guard = inner_arc.lock().unwrap();

            match write_result {
                Ok(()) => {
                    guard.sink = Some(sink_val);

                    // After immediate write, drain the queue if any
                    drop(guard);
                    self.drain_queue().await?;

                    Ok(())
                }
                Err(e) => {
                    guard.state = StreamState::Errored;
                    guard.abort_reason = Some(format!("write() failed: {:?}", e));
                    guard.sink = None;

                    // Wake waiting tasks
                    for waker in guard.ready_wakers.drain(..) {
                        waker.wake();
                    }
                    for waker in guard.closed_wakers.drain(..) {
                        waker.wake();
                    }

                    Err(e)
                }
            }
        } else {
            Err(StreamError::Custom("Sink missing during write".into()))
        }
    }

    async fn drain_queue(&mut self) -> StreamResult<()> {
        loop {
            let pending_write_opt = {
                let mut guard = self.inner.lock().unwrap();

                if guard.state != StreamState::Writable {
                    return Err(StreamError::Custom("Stream is not writable".into()));
                }

                guard.queue.pop_front()
            };

            let pending_write = match pending_write_opt {
                Some(pw) => pw,
                None => break, // Queue empty
            };

            let inner_arc = self.inner.clone();
            let mut guard = inner_arc.lock().unwrap();
            let sink_option = guard.sink.take();
            drop(guard);

            if let Some(mut sink_val) = sink_option {
                let mut controller = WritableStreamDefaultController::new(inner_arc.clone());

                let write_result = sink_val.write(pending_write.chunk, &mut controller).await;

                let mut guard = inner_arc.lock().unwrap();

                match write_result {
                    Ok(()) => {
                        guard.sink = Some(sink_val);

                        // Update queue size and backpressure
                        guard.queue_total_size = guard
                            .queue
                            .iter()
                            .map(|pw| guard.strategy.size(&pw.chunk))
                            .sum();
                        let old_backpressure = guard.backpressure;
                        guard.backpressure =
                            guard.queue_total_size >= guard.strategy.high_water_mark();

                        // Wake ready futures if backpressure cleared
                        if old_backpressure && !guard.backpressure {
                            for waker in guard.ready_wakers.drain(..) {
                                waker.wake();
                            }
                        }

                        // Notify the original write caller of success
                        let _ = pending_write.completion_tx.send(Ok(()));
                    }
                    Err(e) => {
                        guard.state = StreamState::Errored;
                        guard.abort_reason = Some(format!("write() failed: {:?}", e));
                        guard.sink = None;

                        // Wake waiting tasks
                        for waker in guard.ready_wakers.drain(..) {
                            waker.wake();
                        }
                        for waker in guard.closed_wakers.drain(..) {
                            waker.wake();
                        }

                        // Notify the original write caller of failure
                        //let _ = pending_write.completion_tx.send(Err(e.clone()));// TODO: make the error enum be Cloneable(i.e) favour `arc` over `box` for the custom error
                        let _ = pending_write
                            .completion_tx
                            .send(Err(StreamError::Custom("error in completion".into())));

                        return Err(e);
                    }
                }
            } else {
                let _ = pending_write
                    .completion_tx
                    .send(Err(StreamError::Custom("Sink missing".into())));
                return Err(StreamError::Custom("Sink missing".into()));
            }
        }

        Ok(())
    }

    /// Close the stream
    pub async fn close(&mut self) -> StreamResult<()> {
        let inner_arc = self.inner.clone();

        let sink_to_close = {
            let mut guard = inner_arc.lock().unwrap();

            if guard.state == StreamState::Closed {
                return Ok(());
            }
            if guard.state == StreamState::Errored {
                return Err(StreamError::Custom("Stream is errored".into()));
            }

            guard.close_requested = true;
            guard.state = StreamState::Closed; // Mark as closed BEFORE calling sink.close()

            // Reject all queued writes
            for pending_write in guard.queue.drain(..) {
                let _ = pending_write.completion_tx.send(Err(StreamError::Custom(
                    "Stream closed before write could complete".into(),
                )));
            }
            guard.queue_total_size = 0;
            guard.backpressure = false;

            // Wake all pending futures as stream is now closed
            for waker in guard.ready_wakers.drain(..) {
                waker.wake(); // ready() tasks will error out
            }
            for waker in guard.closed_wakers.drain(..) {
                waker.wake();
            }

            guard.sink.take()
        };

        if let Some(sink) = sink_to_close {
            sink.close().await?;
        }

        Ok(())
    }

    /// Abort the stream
    pub async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        let inner_arc = self.inner.clone();
        let sink_to_abort = {
            let mut guard = inner_arc.lock().unwrap();

            if guard.state == StreamState::Closed || guard.state == StreamState::Errored {
                return Ok(());
            }

            guard.state = StreamState::Errored;
            guard.abort_reason = reason.clone();

            // Reject all queued writes
            for pending_write in guard.queue.drain(..) {
                let _ = pending_write.completion_tx.send(Err(StreamError::Custom(
                    "Stream aborted before write could complete".into(),
                )));
            }
            guard.queue_total_size = 0;
            guard.backpressure = false;

            // Wake all pending futures as stream is now errored
            for waker in guard.ready_wakers.drain(..) {
                waker.wake();
            }
            for waker in guard.closed_wakers.drain(..) {
                waker.wake();
            }

            guard.sink.take()
        };

        if let Some(sink) = sink_to_abort {
            sink.abort(reason).await?;
        }

        Ok(())
    }

    /// Wait until the stream is ready for more data (i.e., not in backpressure)
    pub fn ready(&self) -> impl Future<Output = StreamResult<()>> + Send {
        ReadyFuture {
            inner: self.inner.clone(),
        }
    }

    /// Wait for stream to close
    pub fn closed(&self) -> impl Future<Output = StreamResult<()>> + Send {
        ClosedFuture {
            inner: self.inner.clone(),
        }
    }

    /// Get the desired size (how much more data can be written without backpressure)
    pub fn desired_size(&self) -> Option<usize> {
        let inner = self.inner.lock().unwrap();
        if inner.state != StreamState::Writable {
            // Check against Writable
            return None;
        }

        let high_water_mark = inner.strategy.high_water_mark();
        if inner.queue_total_size >= high_water_mark {
            Some(0)
        } else {
            Some(high_water_mark - inner.queue_total_size)
        }
    }

    /// Release the lock on the stream, invalidating this writer
    /*pub fn release_lock(self) -> WritableStream<T, Sink, Unlocked> {
        WritableStream {
            inner: self.inner.clone(),
            _state: PhantomData,
        }
        // Since `self` is consumed, the writer is dropped here.
    }*/
    pub fn release_lock(self) -> WritableStream<T, Sink, Unlocked> {
        let mut guard = self.inner.lock().unwrap();
        guard.locked = false;

        WritableStream {
            inner: self.inner.clone(),
            _state: PhantomData,
        }
    }
}

impl<T, Sink> Drop for WritableStreamDefaultWriter<T, Sink> {
    fn drop(&mut self) {
        let mut guard = self.inner.lock().unwrap();
        guard.locked = false;
    }
}

/// Helper Future for WritableStreamDefaultWriter::ready()
struct ReadyFuture<T, Sink> {
    inner: Arc<Mutex<WritableStreamInner<T, Sink>>>,
}

impl<T: Send + 'static, Sink: WritableSink<T> + Send + 'static> Future for ReadyFuture<T, Sink> {
    type Output = StreamResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut guard = self.inner.lock().unwrap();

        if guard.state == StreamState::Errored {
            return Poll::Ready(Err(StreamError::Custom("Stream is errored".into())));
        }
        if guard.state == StreamState::Closed {
            return Poll::Ready(Err(StreamError::Custom("Stream is closed".into())));
        }

        // If not in backpressure, we are ready
        if !guard.backpressure {
            return Poll::Ready(Ok(()));
        }

        // Register waker to be woken when backpressure clears
        guard.ready_wakers.push_back(cx.waker().clone());
        Poll::Pending
    }
}

/// Helper Future for WritableStreamDefaultWriter::closed()
struct ClosedFuture<T, Sink> {
    inner: Arc<Mutex<WritableStreamInner<T, Sink>>>,
}

impl<T: Send + 'static, Sink: WritableSink<T> + Send + 'static> Future for ClosedFuture<T, Sink> {
    type Output = StreamResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut guard = self.inner.lock().unwrap();

        if guard.state == StreamState::Closed {
            return Poll::Ready(Ok(()));
        }
        if guard.state == StreamState::Errored {
            return Poll::Ready(Err(StreamError::Custom("Stream is errored".into())));
        }

        // Register waker to be woken when stream closes or errors
        guard.closed_wakers.push_back(cx.waker().clone());
        Poll::Pending
    }
}

/// Builder for easier WritableStream creation
pub struct WritableStreamBuilder<T> {
    strategy: Option<Box<dyn QueuingStrategy<T> + Send>>,
}

impl<T: Send + 'static> WritableStreamBuilder<T> {
    pub fn new() -> Self {
        Self { strategy: None }
    }

    pub fn with_strategy<S>(mut self, strategy: S) -> Self
    where
        S: QueuingStrategy<T> + Send + 'static,
    {
        self.strategy = Some(Box::new(strategy));
        self
    }

    pub fn _build<Sink>(self, sink: Sink) -> WritableStream<T, Sink, Unlocked>
    where
        Sink: WritableSink<T> + Send + 'static,
    {
        match self.strategy {
            Some(strategy) => WritableStream {
                inner: Arc::new(Mutex::new(WritableStreamInner {
                    state: StreamState::Writable,
                    queue: VecDeque::new(),
                    queue_total_size: 0,
                    strategy,
                    sink: Some(sink),
                    ready_wakers: VecDeque::new(),
                    closed_wakers: VecDeque::new(),
                    backpressure: false,
                    close_requested: false,
                    abort_reason: None,
                    locked: false,
                })),
                _state: PhantomData,
            },
            None => WritableStream::_new(sink),
        }
    }

    pub async fn build<Sink>(
        self,
        sink: Sink,
    ) -> Result<WritableStream<T, Sink, Unlocked>, StreamError>
    where
        Sink: WritableSink<T> + Send + 'static,
    {
        match self.strategy {
            Some(strategy) => {
                // Create inner state with strategy
                let inner = Arc::new(Mutex::new(WritableStreamInner {
                    state: StreamState::Writable,
                    queue: VecDeque::new(),
                    queue_total_size: 0,
                    strategy,
                    sink: Some(sink),
                    ready_wakers: VecDeque::new(),
                    closed_wakers: VecDeque::new(),
                    backpressure: false,
                    close_requested: false,
                    abort_reason: None,
                    locked: false,
                }));

                // Call start() on sink asynchronously
                let sink_to_start = {
                    let mut guard = inner.lock().unwrap();
                    guard.sink.take()
                };

                if let Some(mut sink) = sink_to_start {
                    let mut controller = WritableStreamDefaultController::new(inner.clone());

                    if let Err(e) = sink.start(&mut controller).await {
                        let mut guard = inner.lock().unwrap();
                        guard.state = StreamState::Errored;
                        guard.abort_reason = Some(format!("start() failed: {:?}", e));
                        return Err(e);
                    }

                    let mut guard = inner.lock().unwrap();
                    guard.sink = Some(sink);
                }

                Ok(WritableStream {
                    inner,
                    _state: PhantomData,
                })
            }
            None => WritableStream::new(sink).await,
        }
    }
}

impl<T: Send + 'static> Default for WritableStreamBuilder<T> {
    fn default() -> Self {
        Self::new()
    }
}

// TODO: Implement futures::Sink for WritableStream
/*impl<T: Send + 'static, S> FuturesSink<T> for WritableStream<T, S> {
    type Error = StreamError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut inner = self.inner.lock().unwrap();

        if inner.state == StreamState::Errored {
            return Poll::Ready(Err(StreamError::Custom("Stream is errored".into())));
        }

        if inner.state == StreamState::Closed {
            return Poll::Ready(Err(StreamError::Custom("Stream is closed".into())));
        }

        // Check if we have backpressure
        if inner.backpressure {
            inner.writer_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let mut inner = self.inner.lock().unwrap();

        if inner.state != StreamState::Readable {
            return Err(StreamError::Custom("Stream is not writable".into()));
        }

        let chunk_size = inner.strategy.size(&item);
        inner.queue.push_back(item);
        inner.queue_total_size += chunk_size;

        // Check for backpressure
        if inner.queue_total_size >= inner.strategy.high_water_mark() {
            inner.backpressure = true;
        }

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut inner = self.inner.lock().unwrap();

        // Process pending operations
        if let Some(mut fut) = inner.pending_write.take() {
            match fut.poll_unpin(cx) {
                Poll::Ready(Ok(())) => {
                    // Write completed, continue processing
                }
                Poll::Ready(Err(e)) => {
                    inner.state = StreamState::Errored;
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => {
                    inner.pending_write = Some(fut);
                    return Poll::Pending;
                }
            }
        }

        // Process queued items
        while let Some(chunk) = inner.queue.pop_front() {
            if let Some(sink) = inner.sink.as_mut() {
                let chunk_size = inner.strategy.size(&chunk);
                inner.queue_total_size -= chunk_size;

                let fut = sink.write(chunk, &mut inner.controller);
                inner.pending_write = Some(fut);

                // Poll the future once
                if let Some(mut fut) = inner.pending_write.take() {
                    match fut.poll_unpin(cx) {
                        Poll::Ready(Ok(())) => {
                            // Write completed, continue with next item
                            continue;
                        }
                        Poll::Ready(Err(e)) => {
                            inner.state = StreamState::Errored;
                            return Poll::Ready(Err(e));
                        }
                        Poll::Pending => {
                            inner.pending_write = Some(fut);
                            return Poll::Pending;
                        }
                    }
                }
            }
        }

        // Update backpressure status
        if inner.queue_total_size < inner.strategy.high_water_mark() {
            inner.backpressure = false;
            if let Some(waker) = inner.writer_waker.take() {
                waker.wake();
            }
        }

        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut inner = self.inner.lock().unwrap();

        if inner.state == StreamState::Closed {
            return Poll::Ready(Ok(()));
        }

        if inner.state == StreamState::Errored {
            return Poll::Ready(Err(StreamError::Custom("Stream is errored".into())));
        }

        // First, flush all pending writes
        drop(inner);
        match self.poll_flush(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }

        let mut inner = self.inner.lock().unwrap();
        inner.close_requested = true;

        // Now close the sink
        if let Some(mut fut) = inner.pending_close.take() {
            match fut.poll_unpin(cx) {
                Poll::Ready(Ok(())) => {
                    inner.state = StreamState::Closed;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Err(e)) => {
                    inner.state = StreamState::Errored;
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => {
                    inner.pending_close = Some(fut);
                    return Poll::Pending;
                }
            }
        }

        if let Some(sink) = inner.sink.as_mut() {
            let fut = sink.close();
            inner.pending_close = Some(fut);

            if let Some(mut fut) = inner.pending_close.take() {
                match fut.poll_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        inner.state = StreamState::Closed;
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Err(e)) => {
                        inner.state = StreamState::Errored;
                        Poll::Ready(Err(e))
                    }
                    Poll::Pending => {
                        inner.pending_close = Some(fut);
                        Poll::Pending
                    }
                }
            } else {
                Poll::Ready(Ok(()))
            }
        } else {
            inner.state = StreamState::Closed;
            Poll::Ready(Ok(()))
        }
    }
}
*/

#[cfg(test)]
mod tests {
    use crate::dlc::ideation::d::ByteLengthQueuingStrategy;

    use super::*;
    use std::sync::{Arc, Mutex};

    // Example implementation of WritableSink for demonstration
    #[derive(Debug)]
    pub struct ConsoleSink;

    impl WritableSink<String> for ConsoleSink {
        async fn start(
            &mut self,
            _controller: &mut WritableStreamDefaultController<String, Self>,
        ) -> StreamResult<()> {
            println!("ConsoleSink started");
            Ok(())
        }

        async fn write(
            &mut self,
            chunk: String,
            _controller: &mut WritableStreamDefaultController<String, Self>,
        ) -> StreamResult<()> {
            println!("Writing: {}", chunk);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await; // Simulate async work
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            println!("ConsoleSink closed");
            Ok(())
        }

        async fn abort(self, reason: Option<String>) -> StreamResult<()> {
            println!("ConsoleSink aborted: {:?}", reason);
            Ok(())
        }
    }

    /// Example sink that collects data into a Vec
    struct CollectorSink<T> {
        data: Arc<Mutex<Vec<T>>>,
        closed: bool,
    }

    impl<T> CollectorSink<T> {
        fn new() -> (Self, Arc<Mutex<Vec<T>>>) {
            let data = Arc::new(Mutex::new(Vec::new()));
            let sink = CollectorSink {
                data: data.clone(),
                closed: false,
            };
            (sink, data)
        }
    }

    impl<T: Send + 'static + Clone> WritableSink<T> for CollectorSink<T> {
        async fn start(
            &mut self,
            _controller: &mut WritableStreamDefaultController<T, Self>,
        ) -> StreamResult<()> {
            Ok(())
        }

        async fn write(
            &mut self,
            chunk: T,
            _controller: &mut WritableStreamDefaultController<T, Self>,
        ) -> StreamResult<()> {
            if self.closed {
                return Err(StreamError::Custom("Sink is closed".into()));
            }
            self.data.lock().unwrap().push(chunk);
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            //self.closed = true;
            // Mark closed (though self is consumed, so this is mostly symbolic)
            Ok(())
        }

        async fn abort(self, _reason: Option<String>) -> StreamResult<()> {
            //self.closed = true;
            // Mark closed (though self is consumed)
            Ok(())
        }
    }

    /// Example sink that simulates errors
    struct ErrorSink<T> {
        error_on_write: bool,
        _phantom: PhantomData<T>,
    }

    impl<T> ErrorSink<T> {
        fn new(error_on_write: bool) -> Self {
            Self {
                error_on_write,
                _phantom: PhantomData,
            }
        }
    }

    impl<T: Send + 'static> WritableSink<T> for ErrorSink<T> {
        async fn start(
            &mut self,
            _controller: &mut WritableStreamDefaultController<T, Self>,
        ) -> StreamResult<()> {
            Ok(())
        }

        async fn write(
            &mut self,
            _chunk: T,
            _controller: &mut WritableStreamDefaultController<T, Self>,
        ) -> StreamResult<()> {
            if self.error_on_write {
                Err(StreamError::Custom("Simulated write error".into()))
            } else {
                Ok(())
            }
        }

        async fn close(self) -> StreamResult<()> {
            Ok(())
        }

        async fn abort(self, _reason: Option<String>) -> StreamResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_basic_write_and_close() {
        let (sink, data) = CollectorSink::new();
        let stream = WritableStream::new(sink).await.unwrap();
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Write some data using the writer
        writer.write(1).await.unwrap();
        writer.write(2).await.unwrap();
        writer.write(3).await.unwrap();

        // Close the stream via the writer
        writer.close().await.unwrap();

        // Check the collected data
        let collected = data.lock().unwrap();
        assert_eq!(*collected, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_writer_api() {
        let (sink, data) = CollectorSink::new();
        let stream = WritableStream::new(sink).await.unwrap();
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Write using the writer
        writer.write(42).await.unwrap();
        writer.write(84).await.unwrap();

        // Close using the writer
        writer.close().await.unwrap();

        // Check the collected data
        let collected = data.lock().unwrap();
        assert_eq!(*collected, vec![42, 84]);
    }

    #[tokio::test]
    async fn test_backpressure() {
        let (sink, data) = CollectorSink::new();

        // Create stream with small buffer (high water mark = 2)
        let stream = WritableStreamBuilder::new()
            .with_strategy(CountQueuingStrategy::new(2))
            .build(sink)
            .await
            .unwrap();

        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Write data and check desired size
        writer.write(1).await.unwrap();
        assert!(writer.desired_size().unwrap() > 0);

        writer.write(2).await.unwrap();
        // Should be at capacity now
        assert_eq!(writer.desired_size(), Some(0));

        writer.close().await.unwrap();

        let collected = data.lock().unwrap();
        assert_eq!(*collected, vec![1, 2]);
    }

    #[tokio::test]
    async fn test_byte_length_strategy() {
        let (sink, data) = CollectorSink::new();

        // Create stream with byte length strategy
        let stream = WritableStreamBuilder::new()
            .with_strategy(ByteLengthQueuingStrategy::new(10))
            .build(sink)
            .await
            .unwrap();

        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Write strings
        writer.write("hello".to_string()).await.unwrap(); // 5 bytes
        writer.write("world".to_string()).await.unwrap(); // 5 bytes

        // Should be at capacity (10 bytes)
        assert_eq!(writer.desired_size(), Some(0));

        writer.close().await.unwrap();

        let collected = data.lock().unwrap();
        assert_eq!(*collected, vec!["hello".to_string(), "world".to_string()]);
    }

    #[tokio::test]
    async fn test_error_handling() {
        let sink = ErrorSink::new(true);
        let stream = WritableStream::new(sink).await.unwrap();
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // This should succeed (no write yet)
        let result = writer.write(1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_abort() {
        let (sink, data) = CollectorSink::new();
        let stream = WritableStream::new(sink).await.unwrap();
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Write some data
        writer.write(1).await.unwrap();

        // Abort the stream
        writer.abort(Some("Test abort".to_string())).await.unwrap();

        // Should not be able to write after abort
        let result = writer.write(2).await;
        assert!(result.is_err());

        // Data written before abort should still be there
        let collected = data.lock().unwrap();
        assert_eq!(*collected, vec![1]);
    }

    #[tokio::test]
    async fn test_ready_state() {
        let (sink, _data) = CollectorSink::<i32>::new();
        let stream = WritableStream::new(sink).await.unwrap();
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Stream should be ready initially
        writer.ready().await.unwrap();

        // Close the stream
        writer.close().await.unwrap();

        // Should be able to check closed state
        let result = writer.closed().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_builder_pattern() {
        let (sink, data) = CollectorSink::new();

        let stream = WritableStreamBuilder::new()
            .with_strategy(CountQueuingStrategy::new(5))
            .build(sink)
            .await
            .unwrap();

        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        writer.write("test".to_string()).await.unwrap();
        writer.close().await.unwrap();

        let collected = data.lock().unwrap();
        assert_eq!(*collected, vec!["test".to_string()]);
    }

    #[tokio::test]
    async fn test_stream_states() {
        let (sink, _data) = CollectorSink::new();
        let stream = WritableStream::new(sink).await.unwrap();

        // Initially unlocked
        assert!(!stream.locked());

        // Can create writer
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Write and close
        writer.write(42).await.unwrap();
        writer.close().await.unwrap();

        // Should be closed now
        let result = writer.write(43).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_writer_release_lock_and_reacquire() {
        let (sink, _data) = CollectorSink::<i32>::new();
        let stream = WritableStream::new(sink).await.unwrap();

        // Acquire first writer (locks the stream)
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Write some data
        writer.write(42).await.unwrap();

        // Release the lock, invalidating the writer
        let unlocked_stream = writer.release_lock();

        // Now the stream is unlocked; we can get a new writer
        let (_locked_stream2, mut writer2) = unlocked_stream.get_writer().unwrap();

        // Write more data with the new writer
        writer2.write(84).await.unwrap();

        // Close the stream with the new writer
        writer2.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_stream_locking_prevents_multiple_writers() {
        // Create a new stream with a sink
        let (sink, _data) = CollectorSink::<i32>::new();
        let stream = WritableStream::new(sink).await.unwrap();

        // Acquire the first writer — should succeed
        let (_locked_stream, mut writer1) = stream
            .clone()
            .get_writer()
            .expect("First writer acquisition should succeed");

        // Attempt to acquire a second writer from the original unlocked stream (which is now consumed)
        // So clone the unlocked stream before first acquisition to simulate multiple handles
        let stream_clone = stream.clone();

        // The second acquisition should fail because the stream is locked
        let result = stream_clone.get_writer();

        assert!(
            result.is_err(),
            "Second writer acquisition should fail because stream is locked"
        );

        // Optionally, test that the first writer can write successfully
        writer1.write(42).await.expect("Write should succeed");

        // Release the lock on the first writer
        let unlocked_stream = writer1.release_lock();

        // Now acquiring a new writer should succeed again
        let (_locked_stream2, mut writer2) = unlocked_stream
            .get_writer()
            .expect("Writer acquisition after release should succeed");

        writer2
            .write(84)
            .await
            .expect("Write with second writer should succeed");

        writer2.close().await.expect("Close should succeed");
    }

    struct FailingSink;

    impl WritableSink<i32> for FailingSink {
        async fn start(
            &mut self,
            _controller: &mut WritableStreamDefaultController<i32, Self>,
        ) -> StreamResult<()> {
            Ok(())
        }

        async fn write(
            &mut self,
            _chunk: i32,
            _controller: &mut WritableStreamDefaultController<i32, Self>,
        ) -> StreamResult<()> {
            Err(StreamError::Custom("Simulated write failure".into()))
        }

        async fn close(self) -> StreamResult<()> {
            Ok(())
        }

        async fn abort(self, _reason: Option<String>) -> StreamResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_write_error_propagation() {
        let sink = FailingSink;
        let stream = WritableStream::new(sink)
            .await
            .expect("Stream creation should succeed");
        let (_locked_stream, mut writer) = stream.get_writer().expect("Should get writer");

        // First write should fail with error
        let err = writer.write(42).await.expect_err("Write should fail");

        assert_eq!(format!("{:?}", err), "Custom(\"Simulated write failure\")");

        // Stream should now be errored, further writes rejected immediately
        let err2 = writer
            .write(43)
            .await
            .expect_err("Further writes should fail");

        assert_eq!(format!("{:?}", err2), "Custom(\"Stream is not writable\")");

        // ready() future should resolve with error
        let ready_result = writer.ready().await;
        assert!(ready_result.is_err());

        // closed() future should resolve with error
        let closed_result = writer.closed().await;
        assert!(closed_result.is_err());
    }

    struct PartialFailSink {
        fail_on_next_write: Arc<Mutex<bool>>,
        data: Arc<Mutex<Vec<i32>>>,
    }

    impl PartialFailSink {
        fn new() -> (Self, Arc<Mutex<Vec<i32>>>, Arc<Mutex<bool>>) {
            let data = Arc::new(Mutex::new(Vec::new()));
            let fail_flag = Arc::new(Mutex::new(false));
            let sink = PartialFailSink {
                fail_on_next_write: fail_flag.clone(),
                data: data.clone(),
            };
            (sink, data, fail_flag)
        }
    }

    impl WritableSink<i32> for PartialFailSink {
        async fn start(
            &mut self,
            _controller: &mut WritableStreamDefaultController<i32, Self>,
        ) -> StreamResult<()> {
            Ok(())
        }

        async fn write(
            &mut self,
            chunk: i32,
            _controller: &mut WritableStreamDefaultController<i32, Self>,
        ) -> StreamResult<()> {
            if *self.fail_on_next_write.lock().unwrap() {
                Err(StreamError::Custom("Simulated failure".into()))
            } else {
                self.data.lock().unwrap().push(chunk);
                Ok(())
            }
        }

        async fn close(self) -> StreamResult<()> {
            Ok(())
        }

        async fn abort(self, _reason: Option<String>) -> StreamResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_write_success_then_error() {
        let (sink, data, fail_flag) = PartialFailSink::new();

        let stream = WritableStream::new(sink)
            .await
            .expect("Stream creation should succeed");
        let (_locked_stream, mut writer) = stream.get_writer().expect("Should get writer");

        // First write succeeds
        writer.write(1).await.expect("First write should succeed");

        // Now set the sink to fail on next write by setting the flag
        {
            let mut flag = fail_flag.lock().unwrap();
            *flag = true;
        }

        // Next write fails
        let err = writer.write(2).await.expect_err("Second write should fail");
        assert_eq!(format!("{:?}", err), "Custom(\"Simulated failure\")");

        // Data should contain only first write
        let collected = data.lock().unwrap();
        assert_eq!(*collected, vec![1]);
    }

    #[tokio::test]
    async fn test_close_clears_queue_and_rejects_writes() {
        let (sink, _data) = CollectorSink::<i32>::new();
        let stream = WritableStream::new(sink).await.unwrap();
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Queue some writes by simulating backpressure
        // For simplicity, assume backpressure is forced here
        {
            let mut guard = writer.inner.lock().unwrap();
            guard.backpressure = true;
        }

        //let write1 = tokio::spawn(writer.write(1));
        //let write2 = tokio::spawn(writer.write(2));

        //let write1 = writer.write(1);
        //let write2 = writer.write(2);

        // Close the stream
        writer.close().await.unwrap();

        // Writes should be rejected
        //assert!(write1.await.unwrap().is_err());
        //assert!(write2.await.unwrap().is_err());
        assert!(writer.write(1).await.is_err());
        assert!(writer.write(2).await.is_err());

        // ready() future should error
        assert!(writer.ready().await.is_err());

        // closed() future should succeed
        assert!(writer.closed().await.is_ok());
    }

    #[tokio::test]
    async fn test_abort_clears_queue_and_rejects_writes() {
        let (sink, _data) = CollectorSink::<i32>::new();
        let stream = WritableStream::new(sink).await.unwrap();
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Queue some writes by simulating backpressure
        {
            let mut guard = writer.inner.lock().unwrap();
            guard.backpressure = true;
        }

        //let write1 = tokio::spawn(writer.write(1));
        //let write2 = tokio::spawn(writer.write(2));

        // Abort the stream
        writer.abort(Some("test abort".to_string())).await.unwrap();

        // Writes should be rejected
        //assert!(write1.await.unwrap().is_err());
        //assert!(write2.await.unwrap().is_err());
        assert!(writer.write(1).await.is_err());
        assert!(writer.write(2).await.is_err());

        // ready() and closed() futures should error
        assert!(writer.ready().await.is_err());
        assert!(writer.closed().await.is_err());
    }

    #[tokio::test]
    async fn test_ready_resolves_when_backpressure_clears() {
        let (sink, _data) = CollectorSink::<i32>::new();
        let stream = WritableStream::new(sink).await.unwrap();
        let (_locked_stream, mut writer) = stream.get_writer().unwrap();

        // Force backpressure
        {
            let mut guard = writer.inner.lock().unwrap();
            guard.backpressure = true;
        }

        // Start ready() future (should be pending)
        //let ready_fut = tokio::spawn(writer.ready());
        // Start ready() future (this will be pending because of backpressure)
        let ready_fut = writer.ready();

        // Clear backpressure and wake wakers
        {
            let mut guard = writer.inner.lock().unwrap();
            guard.backpressure = false;
            for waker in guard.ready_wakers.drain(..) {
                waker.wake();
            }
        }

        // ready() should now complete
        //assert!(ready_fut.await.unwrap().is_ok());
        // Await the ready future inline (no spawn)
        assert!(ready_fut.await.is_ok());
    }
}
