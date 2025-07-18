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
        controller: &mut WritableStreamDefaultController,
    ) -> impl Future<Output = StreamResult<()>> + Send {
        future::ready(Ok(())) // default no-op
    }

    /// Write a chunk to the sink
    fn write(
        &mut self,
        chunk: T,
        controller: &mut WritableStreamDefaultController,
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
            in_flight_size: 0,
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
            in_flight_size: 0,
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

use futures::{SinkExt, Stream, StreamExt, future};

/*async fn _stream_task<T, Sink>(
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
                println!(
                    "[stream_task] after process_write: in_flight={}, queued={}, backpressure={}",
                    inner.in_flight_size, inner.queue_total_size, inner.backpressure
                );

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
                    let current = inner.queue_total_size + inner.in_flight_size;
                    if current >= high_water_mark {
                        Some(0)
                    } else {
                        Some(high_water_mark - current)
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
*/

use futures::future::FutureExt;

/*async fn stream_task<T, Sink>(
    mut command_rx: mpsc::Receiver<StreamCommand<T>>,
    mut inner: WritableStreamInner<T, Sink>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    // Represents an in-progress sink write.
    let mut sink_write_fut: Option<BoxFuture<'static, SinkWriteResult>> = None;
    // Store completion for the active pending write
    let mut active_completion: Option<oneshot::Sender<StreamResult<()>>> = None;

    loop {
        select! {
            // 1. Receive new command (always available even if sink is slow)
            cmd = command_rx.next().fuse() => {
                match cmd {
                    Some(StreamCommand::Write { chunk, completion }) => {
                        inner.queue.push_back(PendingWrite { chunk, completion_tx: completion });
                        let chunk_size = inner.strategy.size(&inner.queue.back().unwrap().chunk);
                        inner.queue_total_size += chunk_size;
                        inner.update_backpressure();
                        update_flags(&inner, &backpressure, &closed, &errored);
                    }
                    Some(StreamCommand::Close { completion }) => {
                        let result = process_close(&mut inner).await;
                        update_flags(&inner, &backpressure, &closed, &errored);
                        let _ = completion.send(result);
                    }
                    Some(StreamCommand::Abort { reason, completion }) => {
                        let result = process_abort(&mut inner, reason).await;
                        update_flags(&inner, &backpressure, &closed, &errored);
                        let _ = completion.send(result);
                    }
                    Some(StreamCommand::GetDesiredSize { completion }) => {
                        let high_water_mark = inner.strategy.high_water_mark();
                        let total = inner.queue_total_size + inner.in_flight_size;
                        let size = if inner.state == StreamState::Writable {
                            if total >= high_water_mark {
                                Some(0)
                            } else {
                                Some(high_water_mark - total)
                            }
                        } else { None };
                        let _ = completion.send(size);
                    }
                    Some(StreamCommand::RegisterReadyWaker { waker }) => {
                        inner.ready_wakers.register(&waker);
                    }
                    Some(StreamCommand::RegisterClosedWaker { waker }) => {
                        inner.closed_wakers.register(&waker);
                    }
                    Some(StreamCommand::LockStream { completion }) => {
                        if inner.locked {
                            let _ = completion.send(Err(StreamError::Custom("Stream already locked".into())));
                        } else {
                            inner.locked = true;
                            let _ = completion.send(Ok(()));
                        }
                    }
                    Some(StreamCommand::UnlockStream { completion }) => {
                        if !inner.locked {
                            let _ = completion.send(Err(StreamError::Custom("Stream not locked".into())));
                        } else {
                            inner.locked = false;
                            let _ = completion.send(Ok(()));
                        }
                    }
                    None => break, // Channel closed, exit loop
                }
            },

            // 2. If there's an active sink write in progress, await its completion
            res = async {
                if let Some(fut) = &mut sink_write_fut {
                    fut.await
                } else {
                    SinkWriteResult::NotActive
                }
            }.fuse()=> {
                if let Some(res) = res {
                // Sink write finished!
                if let Some(completion) = active_completion.take() {
                    let _ = completion.send(res.into_result());
                }
                // After completion, update the stream state as needed
                if let SinkWriteResult::Errored = res {
                    inner.state = StreamState::Errored;
                    inner.sink = None;
                    inner.ready_wakers.wake_all();
                    inner.closed_wakers.wake_all();
                }
                inner.in_flight_size -= res.chunk_size();
                inner.update_backpressure();
                update_flags(&inner, &backpressure, &closed, &errored);
                sink_write_fut = None;
            }}
        }

        // After ANY select! branch: If we're not currently writing to sink, kick off the next queued write (if any)
        if sink_write_fut.is_none() {
            if let Some(pw) = inner.queue.pop_front() {
                let chunk_size = inner.strategy.size(&pw.chunk);
                // Remove from queue stats
                inner.queue_total_size -= chunk_size;
                inner.in_flight_size += chunk_size;
                inner.update_backpressure();
                update_flags(&inner, &backpressure, &closed, &errored);

                if let Some(sink) = inner.sink.as_mut() {
                    // Prepare for new async sink write (not tied to runtime)
                    let mut ctrl = WritableStreamDefaultController::new(&mut inner);
                    let fut = async move {
                        let result = sink.write(pw.chunk, &mut ctrl).await;
                        if let Err(_) = result {
                            SinkWriteResult::Errored(chunk_size)
                        } else {
                            SinkWriteResult::Complete(chunk_size)
                        }
                    };
                    sink_write_fut = Some(fut.boxed());
                    active_completion = Some(pw.completion_tx);
                } else {
                    // If sink is missing, stream is errored.
                    let _ = pw
                        .completion_tx
                        .send(Err(StreamError::Custom("Sink missing".into())));
                    inner.state = StreamState::Errored;
                }
            }
        }
    }
}

/// Internal enum to represent result of a sink write, plus its chunk_size for accounting
enum SinkWriteResult {
    Complete(usize), // Success, size written
    Errored(usize),  // Error, size written
    NotActive,       // Used when no sink write is active
}

impl SinkWriteResult {
    fn chunk_size(&self) -> usize {
        match *self {
            SinkWriteResult::Complete(n) | SinkWriteResult::Errored(n) => n,
            SinkWriteResult::NotActive => 0,
        }
    }
    fn into_result(self) -> StreamResult<()> {
        match self {
            SinkWriteResult::Errored(_) => Err(StreamError::Custom("Sink write errored".into())),
            SinkWriteResult::Complete(_) => Ok(()),
            SinkWriteResult::NotActive => Err(StreamError::Custom("No write active".into())),
        }
    }
}*/

// Helper enum to track an in-flight write
/*enum SinkWriteState<T> {
    None,
    Active {
        chunk_size: usize,
        completion: Option<oneshot::Sender<StreamResult<()>>>,
        fut: futures::future::BoxFuture<'static, StreamResult<()>>,
    },
}

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
    use SinkWriteState::*;

    let mut write_state = SinkWriteState::None;

    loop {
        // 1. Prefer to spawn a new sink write if not active and there’s work.
        if let None = write_state {
            if let Some(pw) = inner.queue.pop_front() {
                let chunk_size = inner.strategy.size(&pw.chunk);
                inner.queue_total_size -= chunk_size;
                inner.in_flight_size += chunk_size;
                inner.update_backpressure();
                update_flags(&inner, &backpressure, &closed, &errored);

                if let Some(ref mut sink) = inner.sink {
                    // We're careful to scope this mutable borrow to just this block!
                    let write_fut = {
                        // Scope the controller/borrow as tightly as possible.
                        let mut ctrl = WritableStreamDefaultController::new(&mut inner);
                        sink.write(pw.chunk, &mut ctrl).boxed()
                    };
                    write_state = SinkWriteState::Active {
                        chunk_size,
                        completion: Some(pw.completion_tx),
                        fut: write_fut,
                    };
                } else {
                    let _ = pw
                        .completion_tx
                        .send(Err(StreamError::Custom("Sink missing".into())));
                    inner.state = StreamState::Errored;
                    continue;
                }
            }
        }

        // 2. Concurrently wait for next command OR completion of in-flight write.
        futures::select! {
            cmd = command_rx.next().fuse() => {
                match cmd {
                    Some(StreamCommand::Write { chunk, completion }) => {
                        let chunk_size = inner.strategy.size(&chunk);
                        inner.queue.push_back(PendingWrite { chunk, completion_tx: completion });
                        inner.queue_total_size += chunk_size;
                        inner.update_backpressure();
                        update_flags(&inner, &backpressure, &closed, &errored);
                    }
                    Some(StreamCommand::Close { completion }) => {
                        let result = process_close(&mut inner).await;
                        update_flags(&inner, &backpressure, &closed, &errored);
                        let _ = completion.send(result);
                    }
                    Some(StreamCommand::Abort { reason, completion }) => {
                        let result = process_abort(&mut inner, reason).await;
                        update_flags(&inner, &backpressure, &closed, &errored);
                        let _ = completion.send(result);
                    }
                    Some(StreamCommand::GetDesiredSize { completion }) => {
                        let high_water_mark = inner.strategy.high_water_mark();
                        let total = inner.queue_total_size + inner.in_flight_size;
                        let size = if inner.state == StreamState::Writable {
                            if total >= high_water_mark { Some(0) }
                            else { Some(high_water_mark - total) }
                        } else {
                            None
                        };
                        let _ = completion.send(size);
                    }
                    Some(StreamCommand::RegisterReadyWaker { waker }) => { inner.ready_wakers.register(&waker); }
                    Some(StreamCommand::RegisterClosedWaker { waker }) => { inner.closed_wakers.register(&waker); }
                    Some(StreamCommand::LockStream { completion }) => {
                        let _ = if inner.locked {
                            completion.send(Err(StreamError::Custom("Stream already locked".into())))
                        } else {
                            inner.locked = true;
                            completion.send(Ok(()))
                        };
                    }
                    Some(StreamCommand::UnlockStream { completion }) => {
                        let _ = if !inner.locked {
                            completion.send(Err(StreamError::Custom("Stream not locked".into())))
                        } else {
                            inner.locked = false;
                            completion.send(Ok(()))
                        };
                    }
                    None => break, // Channel closed, exit loop
                }
            }
            // Only poll the current write if present
            default => break,
        }

        // 3. If there's an active sink write, advance it and process the result.
        if let SinkWriteState::Active {
            chunk_size,
            mut completion,
            fut,
        } = &mut write_state
        {
            let result = fut.await;
            inner.in_flight_size -= *chunk_size;
            inner.update_backpressure();
            update_flags(&inner, &backpressure, &closed, &errored);

            if let Some(tx) = completion.take() {
                let _ = tx.send(result.clone());
            }

            if result.is_err() {
                inner.state = StreamState::Errored;
                inner.sink = None;
                inner.ready_wakers.wake_all();
                inner.closed_wakers.wake_all();
                write_state = SinkWriteState::None;
                continue;
            }
            write_state = SinkWriteState::None;
        }
    }
}*/

use futures::future::poll_fn;
use std::task::{Context, Poll};

/*pub async fn _stream_task<T, Sink>(
    mut command_rx: mpsc::Receiver<StreamCommand<T>>,
    mut inner: WritableStreamInner<T, Sink>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    let (ctrl_tx, mut ctrl_rx) = unbounded::<ControllerMsg>();

    // Track the in-flight future and its completion channel, if any.
    let mut inflight: Option<InFlightWrite<Sink>> = None;

    poll_fn(|cx| {
        eprintln!("poll_fn executing...");
        process_controller_msgs(&mut inner, &mut ctrl_rx);
        // 1. Spawn a sink write if none is active and the queue is not empty
        if inflight.is_none() && inner.state == StreamState::Writable {
            if let Some(pw) = inner.queue.pop_front() {
                let chunk_size = inner.strategy.size(&pw.chunk);
                inner.queue_total_size -= chunk_size;
                inner.in_flight_size += chunk_size;
                inner.update_backpressure();
                update_flags(&inner, &backpressure, &closed, &errored);

                // TAKE (move out) the sink, to give full ownership to the future
                if let Some(mut sink) = inner.sink.take() {
                    let mut ctrl = WritableStreamDefaultController::new(ctrl_tx.clone());

                    // Move everything into the async block!
                    let chunk = pw.chunk;
                    let completion = pw.completion_tx;
                    let fut = async move {
                        // Perform the write with owned sink/controller. This block has all ownership.
                        let result = sink.write(chunk, &mut ctrl).await;
                        // Return sink for putting back in struct, plus result and completion channel
                        (sink, result, completion, chunk_size)
                    }
                    .boxed();

                    inflight = Some(InFlightWrite { fut });
                } else {
                    let _ = pw
                        .completion_tx
                        .send(Err(StreamError::Custom("Sink missing".into())));
                    inner.state = StreamState::Errored;
                }
            }
        }

        // 2. Poll the command receiver (never blocks)
        match command_rx.poll_next_unpin(cx) {
            Poll::Ready(Some(cmd)) => {
                eprintln!("Got StreamCommand:");
                // eprintln!("Got StreamCommand: {:?}", &cmd);
                process_command(cmd, &mut inner, &backpressure, &closed, &errored, cx);
                // We'll always continue polling this future on any event
            }
            Poll::Ready(None) => return Poll::Ready(()), // Channel closed, finish
            Poll::Pending => {}
        }

        // 3. Poll the sink write if it's active
        if let Some(inflight_write) = &mut inflight {
            match inflight_write.fut.as_mut().poll(cx) {
                Poll::Ready((sink, result, completion, chunk_size)) => {
                    inner.sink = Some(sink);
                    inner.in_flight_size -= chunk_size;
                    inner.update_backpressure();
                    update_flags(&inner, &backpressure, &closed, &errored);

                    let _ = completion.send(result.clone());

                    if result.is_err() {
                        inner.state = StreamState::Errored;
                        inner.sink = None;
                        inner.ready_wakers.wake_all();
                        inner.closed_wakers.wake_all();
                    }
                    inflight = None;
                    // We'll poll again immediately, to start the next write if needed!
                }
                Poll::Pending => {}
            }
        }

        // Always keep polling until receiver is closed and all work is done!
        Poll::Pending
    })
    .await;
}*/

/*type SinkWriteOutput<Sink> = (
    Sink,
    StreamResult<()>,
    oneshot::Sender<StreamResult<()>>,
    usize,
);

struct InFlightWrite<Sink> {
    fut: futures::future::BoxFuture<'static, SinkWriteOutput<Sink>>,
}*/

// Helper to process each command. Break out to keep task flat.
fn process_command<T, Sink>(
    cmd: StreamCommand<T>,
    inner: &mut WritableStreamInner<T, Sink>,
    backpressure: &Arc<AtomicBool>,
    closed: &Arc<AtomicBool>,
    errored: &Arc<AtomicBool>,
    _cx: &mut Context<'_>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    match cmd {
        StreamCommand::Write { chunk, completion } => {
            let chunk_size = inner.strategy.size(&chunk);
            inner.queue.push_back(PendingWrite {
                chunk,
                completion_tx: completion,
            });
            inner.queue_total_size += chunk_size;
            inner.update_backpressure();
            update_flags(&inner, backpressure, closed, errored);
        }
        StreamCommand::Close { completion } => {
            let result = futures::executor::block_on(process_close(inner)); // sync block for brevity
            update_flags(&inner, backpressure, closed, errored);
            let _ = completion.send(result);
        }
        StreamCommand::Abort { reason, completion } => {
            let result = futures::executor::block_on(process_abort(inner, reason));
            update_flags(&inner, backpressure, closed, errored);
            let _ = completion.send(result);
        }
        StreamCommand::GetDesiredSize { completion } => {
            let high_water_mark = inner.strategy.high_water_mark();
            let total = inner.queue_total_size + inner.in_flight_size;
            let size = if inner.state == StreamState::Writable {
                if total >= high_water_mark {
                    Some(0)
                } else {
                    Some(high_water_mark - total)
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
            let _ = if inner.locked {
                completion.send(Err(StreamError::Custom("Stream already locked".into())))
            } else {
                inner.locked = true;
                completion.send(Ok(()))
            };
        }
        StreamCommand::UnlockStream { completion } => {
            let _ = if !inner.locked {
                completion.send(Err(StreamError::Custom("Stream not locked".into())))
            } else {
                inner.locked = false;
                completion.send(Ok(()))
            };
        }
    }
}

/*async fn process_write<T, Sink>(
    inner: &mut WritableStreamInner<T, Sink>,
    chunk: T,
    ctrl_tx: &UnboundedSender<ControllerMsg>,
) -> StreamResult<()>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    if inner.state != StreamState::Writable {
        return Err(StreamError::Custom("Stream is not writable".into()));
    }

    let chunk_size = inner.strategy.size(&chunk);
    let total = inner.queue_total_size + inner.in_flight_size;
    let high_water_mark = inner.strategy.high_water_mark();

    if total >= high_water_mark {
        // Create a oneshot channel so the caller can await completion when write is flushed
        let (tx, rx) = oneshot::channel();

        // Enqueue the write request
        inner.queue.push_back(PendingWrite {
            chunk,
            completion_tx: tx,
        });

        inner.queue_total_size += chunk_size;
        //inner.backpressure = inner.queue_total_size >= inner.strategy.high_water_mark();
        inner.update_backpressure();

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
        // Inflight write is starting
        inner.in_flight_size += chunk_size;
        inner.update_backpressure();

        let mut controller = WritableStreamDefaultController::new(ctrl_tx.clone());

        //let mut controller = WritableStreamDefaultController::new(inner);

        let result = sink.write(chunk, &mut controller).await;

        // Write finished, inflight complete
        inner.in_flight_size -= chunk_size;
        inner.update_backpressure();

        // Put the sink back after write
        inner.sink = Some(sink);

        if let Err(_) = result {
            inner.state = StreamState::Errored;
            inner.sink = None;
            inner.ready_wakers.wake_all();
            inner.closed_wakers.wake_all();
            return result;
        }

        //inner.queue_total_size += chunk_size;
        //inner.backpressure = inner.queue_total_size >= inner.strategy.high_water_mark();
        //inner.update_backpressure();

        if !inner.backpressure {
            inner.ready_wakers.wake_all();
        }

        Ok(())
    } else {
        Err(StreamError::Custom("Sink missing".into()))
    }
}
*/

use std::mem::replace;
use std::pin::Pin;

// Inflight operations being driven
/*enum InFlight<Sink> {
    Write {
        fut: Pin<Box<dyn futures::Future<Output = StreamResult<()>> + Send>>,
        completion: oneshot::Sender<StreamResult<()>>,
        chunk_size: usize,
        sink: Sink,
    },
    Close {
        fut: Pin<Box<dyn futures::Future<Output = StreamResult<()>> + Send>>,
        completion: oneshot::Sender<StreamResult<()>>,
        sink: Sink,
    },
    Abort {
        fut: Pin<Box<dyn futures::Future<Output = StreamResult<()>> + Send>>,
        completion: oneshot::Sender<StreamResult<()>>,
        sink: Sink,
    },
}*/

enum InFlight<Sink> {
    Write {
        fut: Pin<Box<dyn Future<Output = (Sink, StreamResult<()>)> + Send>>,
        completion: Option<oneshot::Sender<StreamResult<()>>>,
        chunk_size: usize,
    },
    Close {
        //fut: Pin<Box<dyn Future<Output = (Sink, StreamResult<()>)> + Send>>,
        fut: Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>,
        completion: Option<oneshot::Sender<StreamResult<()>>>,
    },
    Abort {
        //fut: Pin<Box<dyn Future<Output = (Sink, StreamResult<()>)> + Send>>,
        fut: Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>,
        completion: Option<oneshot::Sender<StreamResult<()>>>,
    },
}

pub async fn stream_task<T, Sink>(
    mut command_rx: mpsc::Receiver<StreamCommand<T>>,
    mut inner: WritableStreamInner<T, Sink>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    let mut inflight: Option<InFlight<Sink>> = None;
    let (ctrl_tx, mut ctrl_rx): (
        UnboundedSender<ControllerMsg>,
        UnboundedReceiver<ControllerMsg>,
    ) = unbounded();

    /*poll_fn(|cx| {
        process_controller_msgs(&mut inner, &mut ctrl_rx);
        // Process in-flight write/close/abort
        if let Some(op) = &mut inflight {
            match op {
                InFlight::Write {
                    fut,
                    completion,
                    chunk_size,
                    //sink,
                } => match fut.as_mut().poll(cx) {
                    Poll::Ready((mut sink, res)) => {
                        inner.sink = Some(replace(&mut sink, panic!("sink used")));
                        let _ = completion.send(res);
                        inner.in_flight_size -= *chunk_size;
                        inner.update_backpressure();
                        update_flags(&inner, &backpressure, &closed, &errored);
                        inflight = None;
                    }
                    Poll::Pending => return Poll::Pending,
                },
                InFlight::Close {
                    fut, completion, ..
                } => match fut.as_mut().poll(cx) {
                    Poll::Ready(res) => {
                        //let _ = completion.send(res);
                        if let Some(sender) = completion.take() {
                            let _ = sender.send(res);
                        }
                        inner.state = StreamState::Closed;
                        update_flags(&inner, &backpressure, &closed, &errored);
                        inflight = None;
                    }
                    Poll::Pending => return Poll::Pending,
                },
                InFlight::Abort {
                    fut, completion, ..
                } => match fut.as_mut().poll(cx) {
                    Poll::Ready(res) => {
                        //let _ = completion.send(res);
                        if let Some(sender) = completion.take() {
                            let _ = sender.send(res);
                        }
                        inner.state = StreamState::Errored;
                        update_flags(&inner, &backpressure, &closed, &errored);
                        inflight = None;
                    }
                    Poll::Pending => return Poll::Pending,
                },
            }
        }

        // Drain queued writes if none in-flight
        if inflight.is_none() && inner.state == StreamState::Writable {
            if let Some(pw) = inner.queue.pop_front() {
                if let Some(mut sink) = inner.sink.take() {
                    let chunk_size = inner.strategy.size(&pw.chunk);
                    let mut ctrl = WritableStreamDefaultController::new(ctrl_tx.clone());

                    let fut = async move {
                        let res = sink.write(pw.chunk, &mut ctrl).await;
                        (sink, res)
                    }
                    .boxed();
                    inflight = Some(InFlight::Write {
                        fut,
                        completion: pw.completion_tx,
                        chunk_size,
                    });
                    inner.in_flight_size += chunk_size;
                    inner.update_backpressure();
                    update_flags(&inner, &backpressure, &closed, &errored);
                } else {
                    let _ = pw
                        .completion_tx
                        .send(Err(StreamError::Custom("Sink missing".into())));
                    inner.state = StreamState::Errored;
                    update_flags(&inner, &backpressure, &closed, &errored);
                }
            }
        }

        // Handle command queue if idle
        if inflight.is_none() {
            match Pin::new(&mut command_rx).poll_next(cx) {
                Poll::Ready(Some(cmd)) => {
                    match cmd {
                        StreamCommand::Write { chunk, completion } => {
                            if let Some(mut sink) = inner.sink.take() {
                                let chunk_size = inner.strategy.size(&chunk);
                                let mut ctrl =
                                    WritableStreamDefaultController::new(ctrl_tx.clone());

                                let fut = async move {
                                    let res = sink.write(chunk, &mut ctrl).await;
                                    (sink, res)
                                }
                                .boxed();
                                inflight = Some(InFlight::Write {
                                    fut,
                                    completion,
                                    chunk_size,
                                });
                                inner.in_flight_size += chunk_size;
                                inner.update_backpressure();
                                update_flags(&inner, &backpressure, &closed, &errored);
                            } else {
                                let _ = completion
                                    .send(Err(StreamError::Custom("Sink missing".into())));
                                inner.state = StreamState::Errored;
                                update_flags(&inner, &backpressure, &closed, &errored);
                            }
                        }
                        StreamCommand::Close { completion } => {
                            if let Some(sink) = inner.sink.take() {
                                let fut = async move { sink.close().await }.boxed();
                                inflight = Some(InFlight::Close {
                                    fut,
                                    completion: Some(completion),
                                });
                            } else {
                                let _ = completion.send(Ok(()));
                                inner.state = StreamState::Closed;
                                update_flags(&inner, &backpressure, &closed, &errored);
                            }
                        }
                        StreamCommand::Abort { reason, completion } => {
                            if let Some(sink) = inner.sink.take() {
                                let fut = async move {
                                    let res = sink.abort(reason).await;
                                    res
                                }
                                .boxed();
                                inflight = Some(InFlight::Abort {
                                    fut,
                                    completion: Some(completion),
                                });
                            } else {
                                let _ = completion.send(Ok(()));
                                inner.state = StreamState::Errored;
                                update_flags(&inner, &backpressure, &closed, &errored);
                            }
                        }
                        // Handle any other command variants from your protocol.
                        _ => {
                            // Add additional command handling logic as needed.
                        }
                    }
                }
                Poll::Ready(None) => return Poll::Ready(()), // Channel closed, task exits
                Poll::Pending => {}
            }
        }

        // Continue polling until explicit finish
        Poll::Pending
    })
    .await;*/

    poll_fn(|cx| {
        eprintln!("poll_fn executing...");

        process_controller_msgs(&mut inner, &mut ctrl_rx);
        let mut pending_cmd: Option<StreamCommand<T>> = None;

        // 1. ---- Always handle as many admin commands as possible right away ----
        // This prevents deadlock on LockStream, UnlockStream, GetDesiredSize, waker registration.
        loop {
            match command_rx.poll_next_unpin(cx) {
                Poll::Ready(Some(cmd)) => {
                    match &cmd {
                        StreamCommand::LockStream { .. }
                        | StreamCommand::UnlockStream { .. }
                        | StreamCommand::RegisterReadyWaker { .. }
                        | StreamCommand::RegisterClosedWaker { .. }
                        | StreamCommand::GetDesiredSize { .. } => {
                            process_command(cmd, &mut inner, &backpressure, &closed, &errored, cx);
                            continue; // keep polling for more commands!
                        }
                        // Work-producing commands handled below, only if inflight is None
                        _ => {
                            // break and leave cmd for work logic below.
                            // must be a Write, Close, or Abort
                            pending_cmd = Some(cmd);
                            break;
                        }
                    }
                }
                Poll::Ready(None) => return Poll::Ready(()), // channel closed, finish
                Poll::Pending => break,
            }
        }

        // 2. ---- If not currently working, handle one "work" command ----
        //let mut pending_cmd: Option<StreamCommand<T>> = None; // add this declaration outside poll_fn if you want a true static; else make it mutable at appropriate scope above.
        // (Best practice: move pending_cmd into an outer scope if you want to buffer only one.)

        if inflight.is_none() {
            if let Some(cmd) = unsafe { pending_cmd.take() } {
                match cmd {
                    StreamCommand::Write { chunk, completion } => {
                        let chunk_size = inner.strategy.size(&chunk);
                        let pw = PendingWrite {
                            chunk,
                            completion_tx: completion,
                        };
                        inner.queue.push_back(pw);
                        inner.queue_total_size += chunk_size;
                        inner.update_backpressure();
                        update_flags(&inner, &backpressure, &closed, &errored);
                    }
                    StreamCommand::Close { completion } => {
                        if let Some(sink) = inner.sink.take() {
                            let fut = async move { sink.close().await }.boxed();
                            inflight = Some(InFlight::Close {
                                fut,
                                completion: Some(completion),
                            });
                        } else {
                            let _ = completion.send(Ok(()));
                            inner.state = StreamState::Closed;
                            update_flags(&inner, &backpressure, &closed, &errored);
                        }
                    }
                    StreamCommand::Abort { reason, completion } => {
                        if let Some(sink) = inner.sink.take() {
                            let fut = async move { sink.abort(reason).await }.boxed();
                            inflight = Some(InFlight::Abort {
                                fut,
                                completion: Some(completion),
                            });
                        } else {
                            let _ = completion.send(Ok(()));
                            inner.state = StreamState::Errored;
                            update_flags(&inner, &backpressure, &closed, &errored);
                        }
                    }
                    _ => {}
                }
            }
        }

        // 3. ---- Start a pending Write if possible ----
        if inflight.is_none() && inner.state == StreamState::Writable {
            if let Some(pw) = inner.queue.pop_front() {
                let chunk_size = inner.strategy.size(&pw.chunk);
                inner.queue_total_size -= chunk_size;
                inner.in_flight_size += chunk_size;
                inner.update_backpressure();
                update_flags(&inner, &backpressure, &closed, &errored);

                if let Some(mut sink) = inner.sink.take() {
                    let mut ctrl = WritableStreamDefaultController::new(ctrl_tx.clone());
                    let chunk = pw.chunk;
                    let completion = pw.completion_tx;
                    inflight = Some(InFlight::Write {
                        fut: Box::pin(async move {
                            let result = sink.write(chunk, &mut ctrl).await;
                            (sink, result)
                        }),
                        completion: Some(completion),
                        chunk_size,
                    });
                } else {
                    let _ = pw
                        .completion_tx
                        .send(Err(StreamError::Custom("Sink missing".into())));
                    inner.state = StreamState::Errored;
                }
            }
        }

        // 4. ---- Poll the in-flight future if present ----
        /*if let Some(inflight_write) = &mut inflight {
            match inflight_write.fut.as_mut().poll(cx) {
                Poll::Ready((maybe_sink, result, completion, chunk_size)) => {
                    if let Some(sink) = maybe_sink {
                        inner.sink = Some(sink);
                    }
                    if chunk_size > 0 {
                        inner.in_flight_size -= chunk_size;
                    }
                    inner.update_backpressure();
                    update_flags(&inner, &backpressure, &closed, &errored);

                    let _ = completion.send(result.clone());

                    if result.is_err() {
                        inner.state = StreamState::Errored;
                        inner.sink = None;
                        inner.ready_wakers.wake_all();
                        inner.closed_wakers.wake_all();
                    }
                    inflight = None;
                    cx.waker().wake_by_ref(); // ensure continued draining
                }
                Poll::Pending => {}
            }
        }*/
        // 4. ---- Poll the in-flight future if present ----
        if let Some(inflight_op) = &mut inflight {
            match inflight_op {
                InFlight::Write {
                    fut,
                    completion,
                    chunk_size,
                } => {
                    match fut.as_mut().poll(cx) {
                        Poll::Ready((sink, result)) => {
                            inner.sink = Some(sink);
                            inner.in_flight_size -= *chunk_size;
                            inner.update_backpressure();
                            update_flags(&inner, &backpressure, &closed, &errored);

                            //let _ = completion.send(result.clone());
                            if let Some(sender) = completion.take() {
                                let _ = sender.send(result.clone());
                            }

                            if result.is_err() {
                                inner.state = StreamState::Errored;
                                inner.sink = None;
                                //inner.ready_wakers.wake_all();
                                //inner.closed_wakers.wake_all();
                                update_flags(&mut inner, &backpressure, &closed, &errored);
                            }
                            inflight = None;
                            cx.waker().wake_by_ref(); // ensure continued draining
                        }
                        Poll::Pending => {}
                    }
                }
                InFlight::Close { fut, completion } => match fut.as_mut().poll(cx) {
                    Poll::Ready(res) => {
                        if let Some(sender) = completion.take() {
                            let _ = sender.send(res);
                        }
                        inner.state = StreamState::Closed;
                        update_flags(&inner, &backpressure, &closed, &errored);
                        inflight = None;
                        cx.waker().wake_by_ref();
                    }
                    Poll::Pending => {}
                },
                InFlight::Abort { fut, completion } => match fut.as_mut().poll(cx) {
                    Poll::Ready(res) => {
                        if let Some(sender) = completion.take() {
                            let _ = sender.send(res);
                        }
                        inner.state = StreamState::Errored;
                        update_flags(&inner, &backpressure, &closed, &errored);
                        inflight = None;
                        cx.waker().wake_by_ref();
                    }
                    Poll::Pending => {}
                },
            }
        }

        Poll::Pending
    })
    .await;
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
    let old_state = if closed.load(Ordering::SeqCst) {
        StreamState::Closed
    } else if errored.load(Ordering::SeqCst) {
        StreamState::Errored
    } else {
        StreamState::Writable
    };
    let new_state = inner.state;

    backpressure.store(inner.backpressure, Ordering::SeqCst);
    closed.store(new_state == StreamState::Closed, Ordering::SeqCst);
    errored.store(new_state == StreamState::Errored, Ordering::SeqCst);

    // Wake when state transitions to Closed or Errored
    if (old_state != StreamState::Closed && new_state == StreamState::Closed)
        || (old_state != StreamState::Errored && new_state == StreamState::Errored)
    {
        inner.ready_wakers.wake_all();
        inner.closed_wakers.wake_all();
    }
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

/*async fn drain_queue<T, Sink>(
    inner: &mut WritableStreamInner<T, Sink>,
    ctrl_tx: &UnboundedSender<ControllerMsg>,
) -> StreamResult<()>
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
            let chunk_size = inner.strategy.size(&pw.chunk);

            // Remove from queued total
            inner.queue_total_size -= chunk_size;
            inner.update_backpressure();

            // Mark as in-flight
            inner.in_flight_size += chunk_size;
            inner.update_backpressure();

            // mutable 'sink' now
            // Now, 'inner' is available for borrowing by the controller
            //let mut controller = WritableStreamDefaultController::new(inner);
            let mut controller = WritableStreamDefaultController::new(ctrl_tx.clone());

            // Perform the write operation
            let res = sink.write(pw.chunk, &mut controller).await;

            // When sink write is done, update in-flight
            inner.in_flight_size -= chunk_size;
            inner.update_backpressure();

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
            /*inner.queue_total_size = inner
            .queue
            .iter()
            .map(|w| inner.strategy.size(&w.chunk))
            .sum(); // This recalculates total size based on remaining items*/

            //let old_backpressure = inner.backpressure;
            //inner.backpressure = inner.queue_total_size >= inner.strategy.high_water_mark();
            //inner.update_backpressure();

            /*if old_backpressure && !inner.backpressure {
                inner.ready_wakers.wake_all();
            }*/
            // Wake .ready() if backpressure cleared
            if !inner.backpressure {
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
*/

use std::task::Waker;

pub struct WritableStreamInner<T, Sink> {
    state: StreamState,
    queue: VecDeque<PendingWrite<T>>,
    queue_total_size: usize,
    in_flight_size: usize,
    strategy: Box<dyn QueuingStrategy<T> + Send + Sync + 'static>,
    sink: Option<Sink>,

    backpressure: bool,
    locked: bool,
    close_requested: bool,

    abort_reason: Option<String>,

    ready_wakers: WakerSet,
    closed_wakers: WakerSet,
}

impl<T, Sink> WritableStreamInner<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    // ...other methods...

    /// Update the stream's backpressure flag to reflect the current load.
    fn update_backpressure(&mut self) {
        let total = self.queue_total_size + self.in_flight_size;
        let prev = self.backpressure;
        self.backpressure = total >= self.strategy.high_water_mark();
        if prev && !self.backpressure {
            // Backpressure just cleared: wake all pending ready futures!
            self.ready_wakers.wake_all();
        }
    }
}

pub enum ControllerMsg {
    /// Trigger a stream error (controller.error(...))
    Error(StreamError),
    /// Set/clear backpressure (if you expose this—which in WHATWG is not required, but up to you)
    SetBackpressure(bool),
    // Add more actions if your controller needs (close, abort, etc)
    // Maybe signal custom logic for specific sinks
}

use futures::channel::mpsc::UnboundedReceiver;

/// Handle all controller-to-driver messages in a single step.
/// Call this in your poll_fn, before or after handling work.
fn process_controller_msgs<T, Sink>(
    inner: &mut WritableStreamInner<T, Sink>,
    ctrl_rx: &mut UnboundedReceiver<ControllerMsg>,
) {
    while let Ok(Some(msg)) = ctrl_rx.try_next() {
        match msg {
            ControllerMsg::Error(err) => {
                inner.state = StreamState::Errored;
                inner.abort_reason = Some(format!("Controller error: {:?}", err));
                inner.queue.clear();
                inner.queue_total_size = 0;
                inner.ready_wakers.wake_all();
                inner.closed_wakers.wake_all();
            }
            ControllerMsg::SetBackpressure(bp) => {
                let old = inner.backpressure;
                inner.backpressure = bp;
                if old && !bp {
                    inner.ready_wakers.wake_all();
                }
            } // Add custom pattern matches for any other messages
        }
    }
}

use futures::channel::mpsc::{UnboundedSender, unbounded};

pub struct WritableStreamDefaultController {
    tx: UnboundedSender<ControllerMsg>,
}

impl WritableStreamDefaultController {
    pub fn new(sender: UnboundedSender<ControllerMsg>) -> Self {
        Self { tx: sender }
    }

    /// Signal an error on the stream
    pub fn error(&self, error: StreamError) {
        // ignore send failure if receiver is dropped
        let _ = self.tx.unbounded_send(ControllerMsg::Error(error));
    }

    /// Update backpressure state and wake ready futures if needed
    pub fn set_backpressure(&self, backpressure: bool) {
        let _ = self
            .tx
            .unbounded_send(ControllerMsg::SetBackpressure(backpressure));
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
use std::future::Future;

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
    use std::sync::{Arc, Mutex, atomic::AtomicUsize};

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
            _controller: &mut WritableStreamDefaultController,
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

    #[tokio::test]
    async fn basic_write_test() {
        let sink = CountingSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(2));

        let stream = WritableStream::new(sink.clone(), strategy);
        /*let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });*/
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

    #[tokio::test]
    async fn basic_write_close_test() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct CountingSink {
            write_count: Arc<Mutex<usize>>,
        }

        impl CountingSink {
            fn new() -> Self {
                Self {
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
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let count = self.write_count.clone();
                async move {
                    let mut guard = count.lock().unwrap();
                    *guard += 1;
                    Ok(())
                }
            }
        }

        let sink = CountingSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(10));
        let stream = WritableStream::new(sink.clone(), strategy);
        let (_locked_stream, writer) = stream.get_writer().await.expect("get_writer");

        writer.write(vec![1]).await.expect("write 1");
        writer.write(vec![2]).await.expect("write 2");

        writer.close().await.expect("close");

        assert_eq!(sink.get_count(), 2);
    }

    #[tokio::test]
    async fn backpressure_trigger_and_relief() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use tokio::sync::Notify;

        #[derive(Clone)]
        struct DelayedSink {
            notify: Arc<Notify>,
            write_count: Arc<Mutex<usize>>,
        }

        impl DelayedSink {
            fn new() -> (Self, Arc<Notify>) {
                let notify = Arc::new(Notify::new());
                (
                    Self {
                        notify: notify.clone(),
                        write_count: Arc::new(Mutex::new(0)),
                    },
                    notify,
                )
            }

            fn get_count(&self) -> usize {
                *self.write_count.lock().unwrap()
            }
        }

        impl WritableSink<Vec<u8>> for DelayedSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let notify = self.notify.clone();
                let count = self.write_count.clone();
                async move {
                    {
                        let mut guard = count.lock().unwrap();
                        *guard += 1;
                    }
                    // Simulate slow write that awaits notification
                    notify.notified().await;
                    Ok(())
                }
            }
        }

        let (sink, notify) = DelayedSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(1)); // low HWM forces backpressure early
        let stream = WritableStream::new(sink.clone(), strategy);
        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

        let writer = Arc::new(writer);

        let writer_clone1 = writer.clone();
        // First write triggers async sink.write, queue_total_size = 1, no backpressure yet
        let write1 = tokio::spawn(async move { writer_clone1.write(vec![1]).await });

        let writer_clone2 = writer.clone();
        // Second write exceeds high water mark, should get queued and backpressure applied
        //let write2_fut = writer_clone2.write(vec![2]);
        //let write2_result = tokio::spawn(write2_fut);
        let write2_result = tokio::spawn(async move { writer_clone2.write(vec![2]).await });

        // Wait a short time - write2 should be pending because of backpressure
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The ready() future resolves when backpressure clears
        let ready_fut = stream.ready();
        tokio::pin!(ready_fut);

        // Poll once, should be pending due to backpressure
        use futures::task::noop_waker_ref;
        use std::task::{Context, Poll};
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        assert!(matches!(ready_fut.as_mut().poll(&mut cx), Poll::Pending));

        // Finish first write by notifying sink
        notify.notify_one();

        let _ = write1.await.expect("write1 done");
        let _ = write2_result.await.expect("write2 done");

        // Now backpressure relieved; ready() should resolve
        let ready_result = ready_fut.await;
        assert!(ready_result.is_ok());

        // Confirm both chunks written
        assert_eq!(sink.get_count(), 2);
    }

    #[tokio::test]
    async fn close_abort_error_test() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct TestSink {
            closed: Arc<Mutex<bool>>,
            aborted: Arc<Mutex<Option<String>>>,
        }

        impl TestSink {
            fn new() -> Self {
                Self {
                    closed: Arc::new(Mutex::new(false)),
                    aborted: Arc::new(Mutex::new(None)),
                }
            }

            fn is_closed(&self) -> bool {
                *self.closed.lock().unwrap()
            }

            fn abort_reason(&self) -> Option<String> {
                self.aborted.lock().unwrap().clone()
            }
        }

        impl WritableSink<Vec<u8>> for TestSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                async { Ok(()) }
            }

            fn close(mut self) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let closed = self.closed.clone();
                async move {
                    *closed.lock().unwrap() = true;
                    Ok(())
                }
            }

            fn abort(
                mut self,
                reason: Option<String>,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let aborted = self.aborted.clone();
                async move {
                    *aborted.lock().unwrap() = reason;
                    Ok(())
                }
            }
        }

        let sink = TestSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(1));
        let stream = WritableStream::new(sink.clone(), strategy);

        let (_locked_stream, writer) = stream.get_writer().await.expect("get_writer");

        // Abort the stream with a reason
        writer
            .abort(Some("failure".to_string()))
            .await
            .expect("abort");

        assert_eq!(sink.abort_reason(), Some("failure".to_string()));

        // Note: after abort, write/close calls should fail

        let write_result = writer.write(vec![1]).await;

        assert!(write_result.is_err());

        // Closing after abort should be error

        let close_result = writer.close().await;

        assert!(close_result.is_err());
    }

    #[tokio::test]
    async fn lock_acquire_release_test() {
        let sink = CountingSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(10));
        let stream = WritableStream::new(sink.clone(), strategy);

        // Get first writer (locks stream)
        let (_locked_stream, writer1) = stream.clone().get_writer().await.expect("get_writer");

        // Try to get second writer — should fail because locked
        let write_future = stream.clone().get_writer();

        match write_future.await {
            Err(e) => assert!(matches!(e, StreamError::Custom(_))),
            Ok(_) => panic!("Acquired second writer while stream is locked!"),
        }

        // Release lock
        let unlocked_stream = writer1.release_lock().await;

        // Now get_writer should succeed
        let (_locked_stream2, _writer2) = unlocked_stream
            .get_writer()
            .await
            .expect("get_writer again");
    }

    #[tokio::test]
    async fn backpressure_behavior() {
        use futures::Future;
        use std::pin::Pin;
        use std::sync::{Arc, Mutex};

        // Custom sink that simulates slow writes by never resolving immediately
        struct SlowSink {
            calls: Arc<Mutex<usize>>,
            unblock_notify: Arc<tokio::sync::Notify>,
        }

        impl SlowSink {
            fn new() -> (Self, Arc<tokio::sync::Notify>) {
                let notify = Arc::new(tokio::sync::Notify::new());
                (
                    Self {
                        calls: Arc::new(Mutex::new(0)),
                        unblock_notify: notify.clone(),
                    },
                    notify,
                )
            }

            fn get_call_count(&self) -> usize {
                *self.calls.lock().unwrap()
            }
        }

        impl WritableSink<Vec<u8>> for SlowSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
                let calls_clone = self.calls.clone();
                let notify = self.unblock_notify.clone();

                async move {
                    {
                        let mut guard = calls_clone.lock().unwrap();
                        *guard += 1;
                    }

                    // Wait until notified to unblock (simulate slow write)
                    notify.notified().await;

                    Ok(())
                }
            }
        }

        // Use a low water mark to force backpressure quickly
        let (sink, unblock_notify) = SlowSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(1));

        let stream = WritableStream::new(sink, strategy);
        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

        let writer = Arc::new(writer);

        let writer_clone = writer.clone();
        // Issue first write: triggers sink.write, runs async but blocked inside sink
        let write1 = tokio::spawn(async move { writer_clone.write(vec![1]).await });

        // Perform a quick check that no writes completed yet (since they block)
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert_eq!(
            stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst),
            false,
            "Backpressure false after first write still in progress"
        );

        let writer_clone_2 = writer.clone();
        // Second write triggers backpressure because high_water_mark is 1, so queue it
        let write2 = tokio::spawn(async move { writer_clone_2.write(vec![2]).await });

        // Allow some time — write2 should be pending due to backpressure queueing
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Check that backpressure is now applied
        assert_eq!(
            stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst),
            true,
            "Backpressure should be true as write2 queued"
        );

        // `ready()` should be pending while backpressure applies
        let mut ready_fut = stream.ready();
        tokio::pin!(ready_fut);
        use futures::task::{Context, Poll};
        use std::task::Waker;

        let waker = futures::task::noop_waker_ref();
        let mut cx = Context::from_waker(waker);

        match ready_fut.as_mut().poll(&mut cx) {
            Poll::Pending => {} // Expected, since backpressure is active
            Poll::Ready(_) => panic!("ready() resolved early despite backpressure"),
        }

        // Now simulate sink unblocking to process pending writes
        unblock_notify.notify_one();

        // Await first write completion
        write1.await.expect("write1 done");

        // Lower backpressure flag should clear after draining once first write completes;
        // Sink should start draining queued writes now.
        // Notify again to unblock second write
        unblock_notify.notify_one();

        // Await second write completion
        write2.await.expect("write2 done");

        // After draining queue, backpressure flag must be false
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await; // let state stabilize
        assert_eq!(
            stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst),
            false,
            "Backpressure should be cleared after draining queue"
        );

        // Now ready() future must resolve successfully
        let ready_result = ready_fut.await;
        assert!(ready_result.is_ok());

        // Confirm underlying sink saw both writes
        assert_eq!(
            stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst),
            false
        );
    }

    //spec compliance
    #[tokio::test]
    async fn ready_future_resolves_only_after_backpressure_clears() {
        use futures::FutureExt;
        use std::sync::{Arc, Mutex};

        // Setup: sink that holds writes until notified
        #[derive(Clone)]
        struct BlockSink {
            notify: Arc<tokio::sync::Notify>,
            write_calls: Arc<Mutex<usize>>,
        }

        impl BlockSink {
            fn new() -> (Self, Arc<tokio::sync::Notify>) {
                let notify = Arc::new(tokio::sync::Notify::new());
                (
                    Self {
                        notify: notify.clone(),
                        write_calls: Arc::new(Mutex::new(0)),
                    },
                    notify,
                )
            }

            fn count(&self) -> usize {
                *self.write_calls.lock().unwrap()
            }
        }

        impl WritableSink<Vec<u8>> for BlockSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _ctrl: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let notify = self.notify.clone();
                let count_clone = self.write_calls.clone();
                async move {
                    *count_clone.lock().unwrap() += 1;
                    notify.notified().await;
                    Ok(())
                }
            }
        }

        // Use low high_water_mark = 1 to trigger backpressure easily
        let (sink, notify) = BlockSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(1usize));
        let stream = WritableStream::new(sink, strategy);
        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

        let writer = Arc::new(writer);
        let writer_clone = writer.clone();
        // 1st write triggers sink.write and gets "stuck"
        let write1 = tokio::spawn(async move { writer_clone.write(vec![1]).await });

        // Wait a brief moment to let sink.write be actively blocked
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // 2nd write triggers backpressure
        //let write2_fut = writer.write(vec![2]);

        // Spawn 2nd write task but do NOT notify sink yet
        //let write2 = tokio::spawn(write2_fut);

        let writer_clone = writer.clone();
        let write2 = tokio::spawn(async move { writer_clone.write(vec![2]).await });

        // At this point, backpressure should be true
        assert!(
            stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst),
            "Backpressure is set while first write blocked and second queued"
        );

        // Poll `ready()` future; should be Pending while backpressure is active
        let mut ready = stream.ready();

        use futures::task::noop_waker_ref;
        use futures::task::{Context, Poll};
        use std::pin::Pin;

        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        let mut pinned = Pin::new(&mut ready);

        assert!(
            matches!(pinned.as_mut().poll(&mut cx), Poll::Pending),
            "ready() must be Pending when backpressure"
        );

        // Notify sink to unblock first write
        notify.notify_one();

        // Await completion of first write
        write1.await.expect("first write");

        // Await completion of second write (which was queued)
        notify.notify_one();
        write2.await.expect("second write");

        // Now backpressure should clear
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert!(
            !stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst),
            "Backpressure cleared after draining queue"
        );

        // The ready future should now resolve
        let ready_res = ready.await;
        assert!(
            ready_res.is_ok(),
            "ready() future must resolve Ok() after backpressure clears"
        );
    }

    #[derive(Clone, Default)]
    struct DummySink;

    impl WritableSink<Vec<u8>> for DummySink {
        fn write(
            &mut self,
            _chunk: Vec<u8>,
            _controller: &mut WritableStreamDefaultController,
        ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
            futures::future::ready(Ok(()))
        }
    }

    // #[derive(Clone)]
    // struct CountQueuingStrategy {
    //     high_water_mark: usize,
    // }

    // impl CountQueuingStrategy {
    //     fn new(high_water_mark: usize) -> Self {
    //         CountQueuingStrategy { high_water_mark }
    //     }
    // }

    // impl QueuingStrategy<Vec<u8>> for CountQueuingStrategy {
    //     fn size(&self, _chunk: &Vec<u8>) -> usize {
    //         1
    //     }
    //     fn high_water_mark(&self) -> usize {
    //         self.high_water_mark
    //     }
    // }

    #[tokio::test]
    async fn desired_size_returns_none_when_closed_or_errored() {
        // Setup dummy sink
        #[derive(Clone, Default)]
        struct DummySink;

        impl WritableSink<Vec<u8>> for DummySink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _ctrl: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                futures::future::ready(Ok(()))
            }
        }

        let strategy = Box::new(CountQueuingStrategy::new(10));
        let sink = DummySink::default();
        let stream = WritableStream::new(sink, strategy);

        // Immediately close the stream by sending Close command through writer
        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");
        writer.close().await.expect("close");

        // desired_size *must* return None after close
        let after_close = writer.desired_size().await;
        assert_eq!(after_close, None, "desired_size after close must be None");

        writer.release_lock().await;
        // Simulate errored state by calling abort
        let (_locked_stream, writer) = stream.get_writer().await.expect("get_writer");
        writer.abort(None).await.expect("abort");

        let after_error = writer.desired_size().await;
        assert_eq!(after_error, None, "desired_size after error must be None");
    }

    //proper spec compliance
    #[tokio::test]
    async fn test_writer_locking_exclusivity() {
        let sink = DummySink;
        let strategy = Box::new(CountQueuingStrategy::new(10));
        let stream = WritableStream::new(sink, strategy);

        let (_locked_stream, writer1) = stream.clone().get_writer().await.expect("get_writer");

        // Try to get second writer; should fail with a lock error
        let result2 = stream.clone().get_writer().await;
        assert!(
            matches!(result2, Err(_)),
            "Second get_writer should fail when locked"
        );

        // Release the lock in writer1
        let unlocked_stream = writer1.release_lock().await;

        // Now new writer acquisition must succeed
        let (_locked_stream2, writer2) = unlocked_stream
            .get_writer()
            .await
            .expect("get_writer after release");
        drop(writer2);
    }

    #[tokio::test]
    async fn test_backpressure_and_ready_future() {
        #[derive(Clone)]
        struct CountingSink(Arc<AtomicUsize>);

        impl WritableSink<Vec<u8>> for CountingSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _ctrl: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let counter = self.0.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }
        }

        let sink = CountingSink(Arc::new(AtomicUsize::new(0)));
        let strategy = Box::new(CountQueuingStrategy::new(1)); // Low HWM to test backpressure

        let stream = WritableStream::new(sink.clone(), strategy);
        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

        // First write triggers async write, no backpressure yet
        writer.write(vec![1]).await.expect("write 1");

        // High water mark is 1, second write triggers queue and backpressure
        let write2_fut = writer.write(vec![2]);

        // At this point, backpressure flag should be true
        assert!(
            stream.backpressure.load(Ordering::SeqCst),
            "Backpressure should be applied"
        );

        // ready() future is pending due to backpressure
        let mut ready = stream.ready();
        use futures::task::noop_waker_ref;
        use std::pin::Pin;
        use std::task::{Context, Poll};
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        let mut pinned = Pin::new(&mut ready);
        assert!(matches!(pinned.as_mut().poll(&mut cx), Poll::Pending));

        // Clear backpressure by draining queue — which happens only after the second write completes
        let _ = write2_fut.await;

        // Backpressure should clear
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            !stream.backpressure.load(Ordering::SeqCst),
            "Backpressure cleared after draining"
        );

        // ready() future now resolves
        let res = stream.ready().await;
        assert!(res.is_ok(), "ready() must resolve when backpressure clears");
    }

    #[tokio::test]
    async fn test_desired_size_after_close_and_error() {
        let sink = DummySink;
        let strategy = Box::new(CountQueuingStrategy::new(10));
        let stream = WritableStream::new(sink, strategy);
        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

        writer.close().await.expect("close");

        let ds_after_close = writer.desired_size().await;
        assert_eq!(
            ds_after_close, None,
            "desired_size after close must be None"
        );

        writer.release_lock().await;
        let (_locked_stream2, writer2) = stream.get_writer().await.expect("get_writer");
        writer2.abort(None).await.expect("abort");

        let ds_after_abort = writer2.desired_size().await;
        assert_eq!(
            ds_after_abort, None,
            "desired_size after abort must be None"
        );
    }

    #[tokio::test]
    async fn test_close_and_abort() {
        let sink = DummySink;
        let strategy = Box::new(CountQueuingStrategy::new(10));
        let stream = WritableStream::new(sink, strategy);
        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

        // Close stream - closed future resolves ok
        writer.close().await.expect("close");

        let closed_res = stream.closed().await;
        assert!(closed_res.is_ok(), "closed future must resolve after close");

        // Abort an opened stream errors correctly
        let stream2 = WritableStream::new(DummySink, Box::new(CountQueuingStrategy::new(10)));
        let (_locked_stream2, writer2) = stream2.clone().get_writer().await.expect("get_writer");
        writer2
            .abort(Some("reason".to_string()))
            .await
            .expect("abort");

        let closed_res2 = stream2.closed().await;
        assert!(
            closed_res2.is_err(),
            "closed future must reject after abort/error"
        );
    }

    #[tokio::test]
    async fn test_sink_error_propagates_to_stream() {
        use std::sync::{Arc, Mutex};

        // Sink that fails on write, close, and abort
        #[derive(Clone)]
        struct FailingSink {
            error_flag: Arc<Mutex<bool>>,
        }

        impl FailingSink {
            fn new() -> Self {
                Self {
                    error_flag: Arc::new(Mutex::new(false)),
                }
            }

            fn did_error(&self) -> bool {
                *self.error_flag.lock().unwrap()
            }
        }

        impl WritableSink<Vec<u8>> for FailingSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _ctrl: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let flag = self.error_flag.clone();
                async move {
                    *flag.lock().unwrap() = true;
                    Err(StreamError::Custom("write failure".into()))
                }
            }

            fn close(self) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let flag = self.error_flag.clone();
                async move {
                    *flag.lock().unwrap() = true;
                    Err(StreamError::Custom("close failure".into()))
                }
            }

            fn abort(
                self,
                _reason: Option<String>,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let flag = self.error_flag.clone();
                async move {
                    *flag.lock().unwrap() = true;
                    Err(StreamError::Custom("abort failure".into()))
                }
            }
        }

        let sink = FailingSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(10));
        let stream = WritableStream::new(sink.clone(), strategy);

        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

        // Writing a chunk causes sink.write error => stream errors
        let write_result = writer.write(vec![1]).await;
        assert!(write_result.is_err(), "write should error on sink failure");
        assert!(sink.did_error(), "sink error flag set");

        // After error, stream.backpressure flag should clear (cannot accept writes)
        assert!(stream.errored.load(Ordering::SeqCst));
        assert!(!stream.backpressure.load(Ordering::SeqCst));

        // Attempt to close after error returns error
        let close_result = writer.close().await;
        assert!(
            close_result.is_err(),
            "close should error when stream errored"
        );

        // Check that closed future rejects due to error
        let closed_future_result = stream.closed().await;
        assert!(
            closed_future_result.is_err(),
            "closed future should reject on error"
        );
    }

    #[tokio::test]
    async fn test_ready_and_closed_wakers_behavior() {
        use std::sync::{Arc, Mutex};
        use tokio::sync::Notify;

        struct SlowSink {
            notify: Arc<Notify>,
            write_count: Arc<Mutex<usize>>,
        }

        impl SlowSink {
            fn new() -> (Self, Arc<Notify>) {
                let notify = Arc::new(Notify::new());
                (
                    Self {
                        notify: notify.clone(),
                        write_count: Arc::new(Mutex::new(0)),
                    },
                    notify,
                )
            }

            fn get_write_count(&self) -> usize {
                *self.write_count.lock().unwrap()
            }
        }

        impl WritableSink<Vec<u8>> for SlowSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _ctrl: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let notify = self.notify.clone();
                let write_count = self.write_count.clone();
                async move {
                    {
                        let mut count = write_count.lock().unwrap();
                        *count += 1;
                    }
                    // Wait for external unblock signal
                    notify.notified().await;
                    Ok(())
                }
            }
        }

        let (sink, notify) = SlowSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(2));
        //let stream = WritableStream::new(sink, strategy);
        let stream = WritableStream::new_with_spawn(sink, strategy, |future| {
            tokio::spawn(future);
        });
        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

        let writer = Arc::new(writer);
        let writer_clone = writer.clone();
        // Issue a write that will block (slow sink)
        let write1 = tokio::spawn(async move { writer_clone.write(vec![0]).await });

        // At this point, `backpressure` is not applied yet (only one chunk)
        assert!(!stream.backpressure.load(Ordering::SeqCst));

        let writer_clone = writer.clone();
        // Write another chunk, which queues and triggers backpressure
        let write2 = tokio::spawn(async move { writer_clone.write(vec![1]).await });

        // Wait shortly, then verify backpressure is applied
        //tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        //assert!(stream.backpressure.load(Ordering::SeqCst));
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);
        loop {
            if stream.backpressure.load(Ordering::SeqCst) {
                break;
            }
            if start.elapsed() > timeout {
                panic!("Timeout waiting for backpressure to become true!");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            println!("Atomic: {}", stream.backpressure.load(Ordering::SeqCst),);
        }

        // Get ready future: should be pending due to backpressure
        // Poll twice to simulate waker registration behavior
        let mut ready_fut = stream.ready();
        futures::pin_mut!(ready_fut);

        use std::task::{Context, Poll};
        let waker = futures::task::noop_waker_ref();
        let mut cx = Context::from_waker(waker);

        assert!(matches!(ready_fut.as_mut().poll(&mut cx), Poll::Pending));

        // Unblock sink to drain write queue
        notify.notify_waiters();

        write1.await.expect("write1 completed");
        write2.await.expect("write2 completed");

        // After queue drains, backpressure flag clears
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!stream.backpressure.load(Ordering::SeqCst));

        // ready() future now resolves successfully
        let res = ready_fut.await;
        assert!(
            res.is_ok(),
            "ready() must resolve after backpressure clears"
        );
    }

    #[tokio::test]
    async fn test_lock_acquisition_release() {
        let stream = WritableStream::new(DummySink, Box::new(CountQueuingStrategy::new(10)));

        // Acquire first writer lock
        let (_locked_stream, writer1) = stream.clone().get_writer().await.expect("get_writer");

        // Attempt to get second writer lock concurrently must fail
        let res2 = stream.clone().get_writer().await;
        assert!(
            res2.is_err(),
            "second get_writer acquisition while locked should fail"
        );

        // Release lock via writer1
        let unlocked_stream = writer1.release_lock().await;

        // Now getting writer must succeed
        let (_locked_stream2, writer2) = unlocked_stream
            .get_writer()
            .await
            .expect("get_writer after release");
        drop(writer2);
    }

    #[tokio::test]
    async fn test_close_and_closed_future() {
        let stream = WritableStream::new(DummySink, Box::new(CountQueuingStrategy::new(10)));
        let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

        writer.close().await.expect("close");

        // closed() future resolves successfully
        let closed_res = stream.closed().await;
        assert!(
            closed_res.is_ok(),
            "closed() should resolve after stream closes"
        );

        // Further writes after close fail
        let write_err = writer.write(vec![1]).await;
        assert!(write_err.is_err(), "write after close must fail");
    }
}
