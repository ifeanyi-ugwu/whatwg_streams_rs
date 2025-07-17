use super::QueuingStrategy;
use futures::channel::{mpsc, oneshot};
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ArcError(Arc<dyn Error + Send + Sync>);

impl fmt::Debug for ArcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for ArcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Error for ArcError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

impl From<&str> for ArcError {
    fn from(s: &str) -> Self {
        #[derive(Debug)]
        struct SimpleError(String);
        impl fmt::Display for SimpleError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for SimpleError {}

        ArcError(Arc::new(SimpleError(s.to_string())))
    }
}

impl From<String> for ArcError {
    fn from(s: String) -> Self {
        ArcError::from(s.as_str())
    }
}

#[derive(Debug, Clone)]
pub enum StreamError {
    Canceled,
    TypeError(String),
    NetworkError(String),
    Custom(ArcError),
}

// Implement From conversions

// impl From<ArcError> for StreamError {
//     fn from(e: ArcError) -> Self {
//         StreamError::Custom(e)
//     }
// }

// impl From<&str> for StreamError {
//     fn from(s: &str) -> Self {
//         ArcError::from(s).into()
//     }
// }

// impl From<String> for StreamError {
//     fn from(s: String) -> Self {
//         ArcError::from(s).into()
//     }
// }

// And for std errors:

impl<E> From<E> for StreamError
where
    E: Error + Send + Sync + 'static,
{
    fn from(e: E) -> Self {
        StreamError::Custom(ArcError(Arc::new(e)))
    }
}

type StreamResult<T> = Result<T, StreamError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Writable,
    Closed,
    Errored,
}

struct PendingWrite<T> {
    chunk: T,
    completion_tx: oneshot::Sender<StreamResult<()>>,
}

/// Commands sent to stream task for state mutation
pub enum StreamCommand<T> {
    Write {
        chunk: T,
        completion: oneshot::Sender<StreamResult<()>>,
    },
    Close {
        completion: oneshot::Sender<StreamResult<()>>,
    },
    Abort {
        reason: Option<String>,
        completion: oneshot::Sender<StreamResult<()>>,
    },
    GetDesiredSize {
        completion: oneshot::Sender<Option<usize>>,
    },
    RegisterReadyWaker {
        waker: Waker,
    },

    RegisterClosedWaker {
        waker: Waker,
    },

    LockStream {
        completion: oneshot::Sender<Result<(), StreamError>>,
    },
    UnlockStream {
        completion: oneshot::Sender<Result<(), StreamError>>,
    },
    // Later add GetWriter, ReleaseLock, etc
}

/// Public handle, clonable, holds task command sender and atomic flags
pub struct WritableStream<T, Sink, S = Unlocked> {
    command_tx: mpsc::Sender<StreamCommand<T>>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    _sink: PhantomData<Sink>,
    _state: PhantomData<S>,
}

pub trait WritableSink<T: Send + 'static>: Sized {
    /// Start the sink
    /*fn start(
        &mut self,
        controller: &mut WritableStreamDefaultController<T, Self>,
    ) -> impl std::future::Future<Output = StreamResult<()>> + Send;*/
    fn start(
        &mut self,
        controller: &mut WritableStreamDefaultController<T, Self>,
    ) -> impl Future<Output = StreamResult<()>> + Send {
        future::ready(Ok(())) // default no-op
    }

    /// Write a chunk to the sink
    fn write(
        &mut self,
        chunk: T,
        controller: &mut WritableStreamDefaultController<T, Self>,
    ) -> impl std::future::Future<Output = StreamResult<()>> + Send;

    /// Close the sink
    /*fn close(self) -> impl std::future::Future<Output = StreamResult<()>> + Send;

    /// Abort the sink
    fn abort(
        self,
        reason: Option<String>,
    ) -> impl std::future::Future<Output = StreamResult<()>> + Send;*/

    fn close(self) -> impl Future<Output = StreamResult<()>> + Send {
        future::ready(Ok(())) // default no-op
    }

    fn abort(self, reason: Option<String>) -> impl Future<Output = StreamResult<()>> + Send {
        future::ready(Ok(())) // default no-op
    }
}

pub struct Unlocked;
pub struct Locked;

impl<T, Sink, S> Clone for WritableStream<T, Sink, S> {
    fn clone(&self) -> Self {
        Self {
            command_tx: self.command_tx.clone(),
            backpressure: Arc::clone(&self.backpressure),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            _sink: PhantomData,
            _state: PhantomData,
        }
    }
}

impl<T, Sink> WritableStream<T, Sink, Unlocked>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    pub async fn get_writer(
        self,
    ) -> Result<
        (
            WritableStream<T, Sink, Locked>,
            WritableStreamDefaultWriter<T, Sink>,
        ),
        StreamError,
    > {
        let (tx, rx) = oneshot::channel();

        // Send a special LockStream command to stream task to acquire lock
        self.command_tx
            .clone()
            .send(StreamCommand::LockStream { completion: tx })
            .await
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;

        // Wait for lock acquisition confirmation
        rx.await
            .map_err(|_| StreamError::Custom("Lock response lost".into()))??;

        // Upon success, produce the locked stream and writer
        Ok((
            WritableStream {
                command_tx: self.command_tx.clone(),
                backpressure: Arc::clone(&self.backpressure),
                closed: Arc::clone(&self.closed),
                errored: Arc::clone(&self.errored),
                _sink: PhantomData,
                _state: PhantomData::<Locked>,
            },
            WritableStreamDefaultWriter::new(self.clone()),
        ))
    }
}

impl<T, Sink> WritableStreamDefaultWriter<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    pub async fn release_lock(self) -> WritableStream<T, Sink, Unlocked> {
        let (tx, rx) = oneshot::channel();

        self.stream
            .command_tx
            .clone()
            .send(StreamCommand::UnlockStream { completion: tx })
            .await
            .expect("Stream task dropped");

        rx.await
            .expect("Unlock response lost")
            .expect("Unlock failed");

        WritableStream {
            command_tx: self.stream.command_tx.clone(),
            backpressure: Arc::clone(&self.stream.backpressure),
            closed: Arc::clone(&self.stream.closed),
            errored: Arc::clone(&self.stream.errored),
            _sink: PhantomData,
            _state: PhantomData::<Unlocked>,
        }
    }
}

impl<T, Sink> WritableStream<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    /// Create new WritableStream handle and spawn stream task
    pub fn new(sink: Sink, strategy: Box<dyn QueuingStrategy<T> + Send + Sync + 'static>) -> Self {
        let (command_tx, command_rx) = mpsc::channel(16);

        let inner = WritableStreamInner {
            state: StreamState::Writable,
            queue: VecDeque::new(),
            queue_total_size: 0,
            strategy,
            sink: Some(sink),
            backpressure: false,
            locked: false,
            close_requested: false,
            abort_reason: None,
            ready_wakers: WakerSet::new(),
            closed_wakers: WakerSet::new(),
        };

        let backpressure = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));

        // Spawn the async stream task that processes stream commands
        spawn_stream_task(
            command_rx,
            inner,
            Arc::clone(&backpressure),
            Arc::clone(&closed),
            Arc::clone(&errored),
        );

        Self {
            command_tx,
            backpressure,
            closed,
            errored,
            _sink: PhantomData,
            _state: PhantomData,
        }
    }

    pub fn new_with_spawn<F>(
        sink: Sink,
        strategy: Box<dyn QueuingStrategy<T> + Send + Sync + 'static>,
        spawn_fn: F,
    ) -> Self
    where
        F: FnOnce(futures::future::BoxFuture<'static, ()>) + Send + Sync + 'static,
    {
        let (command_tx, command_rx) = mpsc::channel(16);

        let inner = WritableStreamInner {
            state: StreamState::Writable,
            queue: VecDeque::new(),
            queue_total_size: 0,
            strategy,
            sink: Some(sink),
            backpressure: false,
            locked: false,
            close_requested: false,
            abort_reason: None,
            ready_wakers: WakerSet::new(),
            closed_wakers: WakerSet::new(),
        };

        let backpressure = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));

        let backpressure_clone = backpressure.clone();
        let closed_clone = closed.clone();
        let errored_clone = errored.clone();
        let fut = async move {
            stream_task(
                command_rx,
                inner,
                backpressure_clone,
                closed_clone,
                errored_clone,
            )
            .await;
        };

        spawn_fn(Box::pin(fut));

        Self {
            command_tx,
            backpressure,
            closed,
            errored,
            _sink: PhantomData,
            _state: PhantomData,
        }
    }
}

/// Replace with actual spawning mechanism async executor/runtime
///
/// For example, with futures crate ThreadPool:
/// pool.spawn_ok(stream_task(...)) or with async-std/tokio spawn.
fn spawn_stream_task<T, Sink>(
    command_rx: mpsc::Receiver<StreamCommand<T>>,
    inner: WritableStreamInner<T, Sink>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    // For demonstration, spawn a thread running a futures executor.
    // In real use replace with your runtime.
    std::thread::spawn(move || {
        futures::executor::block_on(stream_task(
            command_rx,
            inner,
            backpressure,
            closed,
            errored,
        ));
    });
}

use futures::{SinkExt, StreamExt, future};

async fn stream_task<T, Sink>(
    mut command_rx: mpsc::Receiver<StreamCommand<T>>,
    mut inner: WritableStreamInner<T, Sink>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    while let Some(cmd) = command_rx.next().await {
        match cmd {
            StreamCommand::Write { chunk, completion } => {
                let result = process_write(&mut inner, chunk).await;
                update_flags(&inner, &backpressure, &closed, &errored);
                let _ = completion.send(result);

                if inner.state == StreamState::Writable {
                    // Attempt to drain queued writes after any successful write
                    if let Err(e) = drain_queue(&mut inner).await {
                        inner.state = StreamState::Errored;
                        inner.sink = None;
                        inner.ready_wakers.wake_all();
                        inner.closed_wakers.wake_all();
                    }
                }
            }
            StreamCommand::Close { completion } => {
                let result = process_close(&mut inner).await;
                update_flags(&inner, &backpressure, &closed, &errored);
                let _ = completion.send(result);
            }
            StreamCommand::Abort { reason, completion } => {
                let result = process_abort(&mut inner, reason).await;
                update_flags(&inner, &backpressure, &closed, &errored);
                let _ = completion.send(result);
            }
            StreamCommand::GetDesiredSize { completion } => {
                let size = if inner.state == StreamState::Writable {
                    let high_water_mark = inner.strategy.high_water_mark();
                    if inner.queue_total_size >= high_water_mark {
                        Some(0)
                    } else {
                        Some(high_water_mark - inner.queue_total_size)
                    }
                } else {
                    None
                };
                let _ = completion.send(size);
            }
            StreamCommand::RegisterReadyWaker { waker } => {
                inner.ready_wakers.register(&waker);
            }
            StreamCommand::RegisterClosedWaker { waker } => {
                inner.closed_wakers.register(&waker);
            }
            StreamCommand::LockStream { completion } => {
                if inner.locked {
                    let _ =
                        completion.send(Err(StreamError::Custom("Stream already locked".into())));
                } else {
                    inner.locked = true;
                    let _ = completion.send(Ok(()));
                }
            }
            StreamCommand::UnlockStream { completion } => {
                if !inner.locked {
                    let _ = completion.send(Err(StreamError::Custom("Stream not locked".into())));
                } else {
                    inner.locked = false;
                    let _ = completion.send(Ok(()));
                }
            }
        }
    }
}

async fn process_write<T, Sink>(
    inner: &mut WritableStreamInner<T, Sink>,
    chunk: T,
) -> StreamResult<()>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    if inner.state != StreamState::Writable {
        return Err(StreamError::Custom("Stream is not writable".into()));
    }

    let chunk_size = inner.strategy.size(&chunk);

    if inner.backpressure {
        // Create a oneshot channel so the caller can await completion when write is flushed
        let (tx, rx) = oneshot::channel();

        // Enqueue the write request
        inner.queue.push_back(PendingWrite {
            chunk,
            completion_tx: tx,
        });

        inner.queue_total_size += chunk_size;
        inner.backpressure = inner.queue_total_size >= inner.strategy.high_water_mark();

        // Return a future that will complete when the queued write is done
        // But since this is inside the task, we can only return an error here.
        // Instead, instruct caller to await the returned future.
        // Optionally, for full design, you can enqueue and the caller waits on the rx.
        return Err(StreamError::Custom(
            "Backpressure applied; write queued".into(),
        ));
    }

    // No backpressure: write immediately
    /*if let Some(sink) = inner.sink.as_mut() {
        let mut controller = WritableStreamDefaultController::new(inner);
        let result = sink.write(chunk, &mut controller).await;

        if let Err(_) = result {
            inner.state = StreamState::Errored;
            inner.sink = None;
            // Wake all
            inner.ready_wakers.wake_all();
            inner.closed_wakers.wake_all();
            return result;
        }

        inner.queue_total_size += chunk_size;
        inner.backpressure = inner.queue_total_size >= inner.strategy.high_water_mark();

        if !inner.backpressure {
            inner.ready_wakers.wake_all();
        }

        Ok(())
    } else {
        Err(StreamError::Custom("Sink missing".into()))
    }*/
    if let Some(mut sink) = inner.sink.take() {
        let mut controller = WritableStreamDefaultController::new(inner);

        let result = sink.write(chunk, &mut controller).await;

        // Put the sink back after write
        inner.sink = Some(sink);

        if let Err(_) = result {
            inner.state = StreamState::Errored;
            inner.sink = None;
            inner.ready_wakers.wake_all();
            inner.closed_wakers.wake_all();
            return result;
        }

        inner.queue_total_size += chunk_size;
        inner.backpressure = inner.queue_total_size >= inner.strategy.high_water_mark();

        if !inner.backpressure {
            inner.ready_wakers.wake_all();
        }

        Ok(())
    } else {
        Err(StreamError::Custom("Sink missing".into()))
    }
}

async fn process_close<T, Sink>(inner: &mut WritableStreamInner<T, Sink>) -> StreamResult<()>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    if inner.state == StreamState::Closed {
        return Ok(());
    }
    if inner.state == StreamState::Errored {
        return Err(StreamError::Custom("Stream is errored".into()));
    }

    inner.close_requested = true;
    inner.state = StreamState::Closed;

    // Clear queue and reset backpressure
    inner.queue.clear();
    inner.queue_total_size = 0;
    inner.backpressure = false;

    // Close sink if present
    if let Some(sink) = inner.sink.take() {
        sink.close().await
    } else {
        Ok(())
    }
}

async fn process_abort<T, Sink>(
    inner: &mut WritableStreamInner<T, Sink>,
    _reason: Option<String>,
) -> StreamResult<()>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    if inner.state == StreamState::Closed || inner.state == StreamState::Errored {
        return Ok(());
    }
    inner.state = StreamState::Errored;
    inner.queue.clear();
    inner.queue_total_size = 0;
    inner.backpressure = false;

    if let Some(sink) = inner.sink.take() {
        sink.abort(_reason).await
    } else {
        Ok(())
    }
}

fn update_flags<T, Sink>(
    inner: &WritableStreamInner<T, Sink>,
    backpressure: &AtomicBool,
    closed: &AtomicBool,
    errored: &AtomicBool,
) {
    backpressure.store(inner.backpressure, Ordering::SeqCst);
    closed.store(inner.state == StreamState::Closed, Ordering::SeqCst);
    errored.store(inner.state == StreamState::Errored, Ordering::SeqCst);
}

pub struct WritableStreamDefaultWriter<T, Sink> {
    stream: WritableStream<T, Sink>,
}

impl<T, Sink> WritableStreamDefaultWriter<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    /// Create a new writer linked to the stream
    pub fn new(stream: WritableStream<T, Sink>) -> Self {
        Self { stream }
    }

    /// Write a chunk asynchronously
    pub async fn write(&self, chunk: T) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.stream
            .command_tx
            .clone()
            .send(StreamCommand::Write {
                chunk,
                completion: tx,
            })
            .await
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Write canceled".into())))
    }

    /// Close the stream asynchronously
    pub async fn close(&self) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.stream
            .command_tx
            .clone()
            .send(StreamCommand::Close { completion: tx })
            .await
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Close canceled".into())))
    }

    /// Abort the stream asynchronously with an optional reason
    pub async fn abort(&self, reason: Option<String>) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.stream
            .command_tx
            .clone()
            .send(StreamCommand::Abort {
                reason,
                completion: tx,
            })
            .await
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Abort canceled".into())))
    }

    /// Get the desired size asynchronously (how much data the stream can accept)
    pub async fn desired_size(&self) -> Option<usize> {
        let (tx, rx) = oneshot::channel();

        let _ = self
            .stream
            .command_tx
            .clone()
            .send(StreamCommand::GetDesiredSize { completion: tx })
            .await;

        rx.await.ok().flatten()
    }
}

/*async fn _drain_queue<T, Sink>(inner: &mut WritableStreamInner<T, Sink>) -> StreamResult<()>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    while let Some(pw) = inner.queue.pop_front() {
        if inner.state != StreamState::Writable {
            let _ = pw
                .completion_tx
                .send(Err(StreamError::Custom("Stream is not writable".into())));
            return Err(StreamError::Custom("Stream is not writable".into()));
        }

        if let Some(sink) = inner.sink.as_mut() {
            let mut controller = WritableStreamDefaultController::new(inner);
            let res = sink.write(pw.chunk, &mut controller).await;

            if res.is_err() {
                inner.state = StreamState::Errored;
                inner.sink = None;
                inner.ready_wakers.wake_all();
                inner.closed_wakers.wake_all();

                let _ = pw.completion_tx.send(res.clone());
                return res;
            }

            inner.queue_total_size = inner
                .queue
                .iter()
                .map(|w| inner.strategy.size(&w.chunk))
                .sum();

            let old_backpressure = inner.backpressure;
            inner.backpressure = inner.queue_total_size >= inner.strategy.high_water_mark();

            if old_backpressure && !inner.backpressure {
                inner.ready_wakers.wake_all();
            }

            let _ = pw.completion_tx.send(Ok(()));
        } else {
            let _ = pw
                .completion_tx
                .send(Err(StreamError::Custom("Sink missing".into())));
            return Err(StreamError::Custom("Sink missing".into()));
        }
    }

    Ok(())
}*/

async fn drain_queue<T, Sink>(inner: &mut WritableStreamInner<T, Sink>) -> StreamResult<()>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    // Important: Use a loop that allows modifying the VecDeque,
    // and process one item at a time.
    // If you need to break the loop or return early, pop the item first.

    // Using `inner.queue.pop_front()` outside the `if let Some(sink)` block ensures
    // the item is removed from the queue even if the sink isn't available.

    while let Some(pw) = inner.queue.pop_front() {
        // pw is now owned
        if inner.state != StreamState::Writable {
            let _ = pw
                .completion_tx
                .send(Err(StreamError::Custom("Stream is not writable".into())));
            // We've already popped, so just return.
            return Err(StreamError::Custom("Stream is not writable".into()));
        }

        // --- Crucial part: Separate the mutable borrows ---

        // 1. Take mutable ownership of the sink if available.
        //    We must take ownership here (or re-borrow) because the controller
        //    also needs a mutable reference to 'inner'.
        let sink_option = inner.sink.take(); // Temporarily take ownership of the sink

        if let Some(mut sink) = sink_option {
            // mutable 'sink' now
            // Now, 'inner' is available for borrowing by the controller
            let mut controller = WritableStreamDefaultController::new(inner);

            // Perform the write operation
            let res = sink.write(pw.chunk, &mut controller).await;

            // After `sink.write` and `controller` are no longer needed for this iteration,
            // put the sink back into `inner`.
            inner.sink = Some(sink); // Put the sink back

            // --- Continue with error handling and state updates ---
            if res.is_err() {
                inner.state = StreamState::Errored;
                inner.sink = None; // If errored, permanently remove the sink
                inner.ready_wakers.wake_all();
                inner.closed_wakers.wake_all();

                let _ = pw.completion_tx.send(res.clone());
                return res; // Stop draining on error
            }

            // Update queue size (recalculating is safer after a pop)
            inner.queue_total_size = inner
                .queue
                .iter()
                .map(|w| inner.strategy.size(&w.chunk))
                .sum(); // This recalculates total size based on remaining items

            let old_backpressure = inner.backpressure;
            inner.backpressure = inner.queue_total_size >= inner.strategy.high_water_mark();

            if old_backpressure && !inner.backpressure {
                inner.ready_wakers.wake_all();
            }

            let _ = pw.completion_tx.send(Ok(()));
        } else {
            // Sink was already missing (e.g., errored or closed already)
            let _ = pw
                .completion_tx
                .send(Err(StreamError::Custom("Sink missing".into())));
            // Since we already popped, this is a clean exit for this item.
            // If the stream is in an errored/closed state, the outer loop will handle it.
            return Err(StreamError::Custom("Sink missing during drain".into())); // Indicate an issue and stop.
        }
    }

    Ok(())
}

use std::task::Waker;

pub struct WritableStreamInner<T, Sink> {
    state: StreamState,
    queue: VecDeque<PendingWrite<T>>,
    queue_total_size: usize,
    strategy: Box<dyn QueuingStrategy<T> + Send + Sync + 'static>,
    sink: Option<Sink>,

    backpressure: bool,
    locked: bool,
    close_requested: bool,

    abort_reason: Option<String>,

    ready_wakers: WakerSet,
    closed_wakers: WakerSet,
}

pub struct WritableStreamDefaultController<'a, T, Sink> {
    inner: &'a mut WritableStreamInner<T, Sink>,
}

impl<'a, T, Sink> WritableStreamDefaultController<'a, T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    pub fn new(inner: &'a mut WritableStreamInner<T, Sink>) -> Self {
        Self { inner }
    }

    /// Signal an error on the stream
    pub fn error(&mut self, error: StreamError) {
        if self.inner.state == StreamState::Closed || self.inner.state == StreamState::Errored {
            return; // no-op if already closed or errored
        }
        self.inner.state = StreamState::Errored;
        self.inner.abort_reason = Some(format!("Controller error: {:?}", error));
        self.inner.queue.clear();
        self.inner.queue_total_size = 0;

        // Wake all waiting futures
        self.inner.ready_wakers.wake_all();
        self.inner.closed_wakers.wake_all();
    }

    /// Update backpressure state and wake ready futures if needed
    pub fn set_backpressure(&mut self, backpressure: bool) {
        let old = self.inner.backpressure;
        self.inner.backpressure = backpressure;
        if old && !backpressure {
            self.inner.ready_wakers.wake_all();
        }
    }
}

/*use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct ReadyFuture<T, Sink> {
    stream: WritableStream<T, Sink>,
}

impl<T, Sink> ReadyFuture<T, Sink> {
    pub fn new(stream: WritableStream<T, Sink>) -> Self {
        Self { stream }
    }
}

impl<T, Sink> Future for ReadyFuture<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    type Output = StreamResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.stream.errored.load(Ordering::SeqCst) {
            return Poll::Ready(Err(StreamError::Custom("Stream is errored".into())));
        }
        if self.stream.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Err(StreamError::Custom("Stream is closed".into())));
        }
        if !self.stream.backpressure.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        // If backpressure, register waker - this requires additional channel or mechanism
        // For brevity, we wake to retry polling later
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

pub struct ClosedFuture<T, Sink> {
    stream: WritableStream<T, Sink>,
}

impl<T, Sink> ClosedFuture<T, Sink> {
    pub fn new(stream: WritableStream<T, Sink>) -> Self {
        Self { stream }
    }
}

impl<T, Sink> Future for ClosedFuture<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    type Output = StreamResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.stream.errored.load(Ordering::SeqCst) {
            return Poll::Ready(Err(StreamError::Custom("Stream is errored".into())));
        }
        if self.stream.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        // Not closed yet, register waker similarly (simplified)
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl<T, Sink> WritableStream<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    pub fn ready(&self) -> impl Future<Output = StreamResult<()>> + Send {
        ReadyFuture::new(self.clone())
    }

    pub fn closed(&self) -> impl Future<Output = StreamResult<()>> + Send {
        ClosedFuture::new(self.clone())
    }
}
*/
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub struct ReadyFuture<T, Sink> {
    stream: WritableStream<T, Sink>,
}

impl<T, Sink> ReadyFuture<T, Sink> {
    pub fn new(stream: WritableStream<T, Sink>) -> Self {
        Self { stream }
    }
}

impl<T, Sink> Future for ReadyFuture<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    type Output = StreamResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.stream.errored.load(Ordering::SeqCst) {
            return Poll::Ready(Err(StreamError::Custom("Stream is errored".into())));
        }
        if self.stream.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Err(StreamError::Custom("Stream is closed".into())));
        }
        if !self.stream.backpressure.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        // Not ready, register waker:
        let waker = cx.waker().clone();
        let mut sender = self.stream.command_tx.clone();
        // Spawn a task to send the waker asynchronously because poll must be sync
        // Note: in real executor, consider futures::task::spawn_local or other task spawner
        std::thread::spawn(move || {
            futures::executor::block_on(sender.send(StreamCommand::RegisterReadyWaker { waker }))
                .ok();
        });
        Poll::Pending
    }
}

pub struct ClosedFuture<T, Sink> {
    stream: WritableStream<T, Sink>,
}

impl<T, Sink> ClosedFuture<T, Sink> {
    pub fn new(stream: WritableStream<T, Sink>) -> Self {
        Self { stream }
    }
}

impl<T, Sink> Future for ClosedFuture<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    type Output = StreamResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.stream.errored.load(Ordering::SeqCst) {
            return Poll::Ready(Err(StreamError::Custom("Stream is errored".into())));
        }
        if self.stream.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        let waker = cx.waker().clone();
        let mut sender = self.stream.command_tx.clone();
        std::thread::spawn(move || {
            futures::executor::block_on(sender.send(StreamCommand::RegisterClosedWaker { waker }))
                .ok();
        });
        Poll::Pending
    }
}

impl<T, Sink> WritableStream<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    pub fn ready(&self) -> ReadyFuture<T, Sink> {
        ReadyFuture::new(self.clone())
    }

    pub fn closed(&self) -> ClosedFuture<T, Sink> {
        ClosedFuture::new(self.clone())
    }
}

/// A lightweight, thread-safe set storing multiple wakers.
/// It ensures wakers are stored without duplicates (based on `will_wake`).
#[derive(Clone, Default)]
pub struct WakerSet(Arc<Mutex<Vec<Waker>>>);

impl WakerSet {
    /// Creates a new, empty `WakerSet`.
    pub fn new() -> Self {
        WakerSet(Arc::new(Mutex::new(Vec::new())))
    }

    /// Adds a waker to the set.
    /// If a waker that would wake the same task is already present, it does not add a duplicate.
    pub fn register(&self, waker: &Waker) {
        let mut wakers = self.0.lock().unwrap();
        if !wakers.iter().any(|w| w.will_wake(waker)) {
            wakers.push(waker.clone());
        }
    }

    /// Wake all registered wakers and clear the set.
    pub fn wake_all(&self) {
        let mut wakers = self.0.lock().unwrap();
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }

    /// Clear all wakers without waking them.
    pub fn clear(&self) {
        let mut wakers = self.0.lock().unwrap();
        wakers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dlc::ideation::d::CountQueuingStrategy;
    use futures::executor::block_on;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct CountingSink {
        // Shared counter for how many chunks were written
        write_count: Arc<Mutex<usize>>,
    }

    impl CountingSink {
        fn new() -> Self {
            CountingSink {
                write_count: Arc::new(Mutex::new(0)),
            }
        }

        fn get_count(&self) -> usize {
            *self.write_count.lock().unwrap()
        }
    }

    impl WritableSink<Vec<u8>> for CountingSink {
        fn write(
            &mut self,
            _chunk: Vec<u8>,
            _controller: &mut WritableStreamDefaultController<Vec<u8>, Self>,
        ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
            let count = Arc::clone(&self.write_count);
            async move {
                // Simulate async write
                let mut guard = count.lock().unwrap();
                *guard += 1;
                Ok(())
            }
        }
    }

    #[tokio::test] // Or #[async_std::test]
    async fn basic_write_test() {
        let sink = CountingSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(2));

        let stream = WritableStream::new(sink.clone(), strategy);
        let (locked_stream, writer) = stream.get_writer().await.expect("get_writer");

        // Write some chunks
        writer.write(vec![1, 2, 3]).await.expect("write");
        writer.write(vec![4, 5]).await.expect("write");

        // Close the stream
        writer.close().await.expect("close");

        // Check underlying sink received write calls
        assert_eq!(sink.get_count(), 2);
    }

    #[tokio::test]
    async fn tokio_spawn_basic_write_test() {
        let sink = CountingSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(2));

        // Using your executor injection idea - spawn stream task via Tokio's runtime:
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |future| {
            tokio::spawn(future);
        });
        let (_locked_stream, writer) = stream.get_writer().await.expect("get_writer");

        // Write data chunks asynchronously
        writer.write(vec![1, 2, 3]).await.expect("write 1");
        writer.write(vec![4, 5]).await.expect("write 2");

        // Gracefully close the stream
        writer.close().await.expect("close");

        // Confirm that each write was counted
        assert_eq!(sink.get_count(), 2);
    }
}
