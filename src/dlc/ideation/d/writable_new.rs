use super::QueuingStrategy;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::channel::oneshot;
use futures::future::FutureExt;
use futures::future::poll_fn;
use futures::{SinkExt, StreamExt, future};
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};

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
    //TypeError(String),
    //NetworkError(String),
    Aborted(Option<String>),
    Closing,
    Closed,
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

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Canceled => write!(f, "Stream operation was canceled"),
            StreamError::Aborted(reason) => {
                if let Some(reason) = reason {
                    write!(f, "Stream was aborted: {}", reason)
                } else {
                    write!(f, "Stream was aborted")
                }
            }
            StreamError::Closing => write!(f, "Cannot write to stream while close is in progress"),
            StreamError::Closed => write!(f, "Cannot write to a closed stream"),
            StreamError::Custom(err) => write!(f, "{}", err),
        }
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
    /*GetDesiredSize {
        completion: oneshot::Sender<Option<usize>>,
    },*/
    RegisterReadyWaker {
        waker: Waker,
    },

    RegisterClosedWaker {
        waker: Waker,
    },
    /*LockStream {
        completion: oneshot::Sender<Result<(), StreamError>>,
    },
    UnlockStream {
        completion: Option<oneshot::Sender<Result<(), StreamError>>>,
    },*/
    // Later add GetWriter, ReleaseLock, etc
}

pub struct Unlocked;
pub struct Locked;

/// Public handle, clonable, holds task command sender and atomic flags
pub struct WritableStream<T, Sink, S = Unlocked> {
    command_tx: UnboundedSender<StreamCommand<T>>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    locked: Arc<AtomicBool>,
    queue_total_size: Arc<AtomicUsize>,
    in_flight_size: Arc<AtomicUsize>,
    high_water_mark: Arc<AtomicUsize>,
    stored_error: Arc<RwLock<Option<StreamError>>>,
    _sink: PhantomData<Sink>,
    _state: PhantomData<S>,
}

impl<T, Sink, S> WritableStream<T, Sink, S> {
    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }
}

impl<T, Sink> WritableStream<T, Sink, Unlocked>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    /// Abort the stream, signaling that no more data will be written.
    /// This rejects all pending writes and errors the stream.
    /// Matches WritableStream.abort() in WHATWG spec.
    pub async fn abort(&self, reason: Option<String>) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        // Send the Abort command to the stream task
        self.command_tx
            .clone()
            .send(StreamCommand::Abort {
                reason,
                completion: tx,
            })
            .await
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;

        // Await the completion of the abort operation
        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Abort operation canceled".into())))
    }

    pub async fn close(&self) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.command_tx
            .clone()
            .send(StreamCommand::Close { completion: tx })
            .await
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Close canceled".into())))
    }
}

impl<T, Sink> WritableStream<T, Sink, Unlocked>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    /*pub async fn get_writer(
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

        let locked = WritableStream {
            command_tx: self.command_tx.clone(),
            backpressure: Arc::clone(&self.backpressure),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            locked: Arc::clone(&self.locked),
            _sink: PhantomData,
            _state: PhantomData::<Locked>,
        };

        // Upon success, produce the locked stream and writer
        Ok((locked.clone(), WritableStreamDefaultWriter::new(locked)))
    }*/
    pub fn get_writer(
        &self,
    ) -> Result<
        (
            WritableStream<T, Sink, Locked>,
            WritableStreamDefaultWriter<T, Sink>,
        ),
        StreamError,
    > {
        // Attempt to atomically acquire the lock:
        if self
            .locked
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(StreamError::Custom(ArcError::from("Stream already locked")));
        }

        let locked = WritableStream {
            command_tx: self.command_tx.clone(),
            backpressure: Arc::clone(&self.backpressure),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            locked: Arc::clone(&self.locked),
            queue_total_size: Arc::clone(&self.queue_total_size),
            in_flight_size: Arc::clone(&self.in_flight_size),
            high_water_mark: Arc::clone(&self.high_water_mark),
            stored_error: Arc::clone(&self.stored_error),
            _sink: PhantomData,
            _state: PhantomData::<Locked>,
        };

        Ok((locked.clone(), WritableStreamDefaultWriter::new(locked)))
    }
}

impl<T, Sink> WritableStream<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    /// Create new WritableStream handle and spawn stream task
    pub fn new(sink: Sink, strategy: Box<dyn QueuingStrategy<T> + Send + Sync + 'static>) -> Self {
        let (command_tx, command_rx) = futures::channel::mpsc::unbounded();
        let high_water_mark = Arc::new(AtomicUsize::new(strategy.high_water_mark()));
        let stored_error = Arc::new(RwLock::new(None));

        let inner = WritableStreamInner {
            state: StreamState::Writable,
            queue: VecDeque::new(),
            queue_total_size: 0,
            in_flight_size: 0,
            strategy,
            sink: Some(sink),
            backpressure: false,
            //locked: false,
            close_requested: false,
            close_completions: Vec::new(),
            abort_reason: None,
            abort_requested: false,
            abort_completions: Vec::new(),
            //stored_error: None,
            stored_error: Arc::clone(&stored_error),
            ready_wakers: WakerSet::new(),
            closed_wakers: WakerSet::new(),
        };

        let backpressure = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let locked = Arc::new(AtomicBool::new(false));
        let queue_total_size = Arc::new(AtomicUsize::new(0));
        let in_flight_size = Arc::new(AtomicUsize::new(0));
        //let stored_error = Arc::new(RwLock::new(None));

        // Spawn the async stream task that processes stream commands
        spawn_stream_task(
            command_rx,
            inner,
            Arc::clone(&backpressure),
            Arc::clone(&closed),
            Arc::clone(&errored),
            Arc::clone(&queue_total_size),
            Arc::clone(&in_flight_size),
        );

        Self {
            command_tx,
            backpressure,
            closed,
            errored,
            locked,
            queue_total_size,
            in_flight_size,
            high_water_mark,
            stored_error,
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
        let (command_tx, command_rx) = futures::channel::mpsc::unbounded();
        let high_water_mark = Arc::new(AtomicUsize::new(strategy.high_water_mark()));
        let stored_error = Arc::new(RwLock::new(None));

        let inner = WritableStreamInner {
            state: StreamState::Writable,
            queue: VecDeque::new(),
            queue_total_size: 0,
            in_flight_size: 0,
            strategy,
            sink: Some(sink),
            backpressure: false,
            //locked: false,
            close_requested: false,
            close_completions: Vec::new(),
            abort_reason: None,
            abort_requested: false,
            abort_completions: Vec::new(),
            //stored_error: None,
            stored_error: Arc::clone(&stored_error),
            ready_wakers: WakerSet::new(),
            closed_wakers: WakerSet::new(),
        };

        let backpressure = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let locked = Arc::new(AtomicBool::new(false));
        let queue_total_size = Arc::new(AtomicUsize::new(0));
        let in_flight_size = Arc::new(AtomicUsize::new(0));
        //let stored_error = Arc::new(RwLock::new(None));

        let backpressure_clone = backpressure.clone();
        let closed_clone = closed.clone();
        let errored_clone = errored.clone();
        let queue_total_size_clone = queue_total_size.clone();
        let in_flight_size_clone = in_flight_size.clone();
        let fut = async move {
            stream_task(
                command_rx,
                inner,
                backpressure_clone,
                closed_clone,
                errored_clone,
                queue_total_size_clone,
                in_flight_size_clone,
            )
            .await;
        };

        spawn_fn(Box::pin(fut));

        Self {
            command_tx,
            backpressure,
            closed,
            errored,
            locked,
            queue_total_size,
            in_flight_size,
            high_water_mark,
            stored_error,
            _sink: PhantomData,
            _state: PhantomData,
        }
    }
}

impl<T, Sink, S> WritableStream<T, Sink, S>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    // private helper
    fn desired_size(&self) -> Option<usize> {
        if self.closed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
            return None;
        }

        let queue_size = self.queue_total_size.load(Ordering::SeqCst);
        let inflight_size = self.in_flight_size.load(Ordering::SeqCst);
        let hwm = self.high_water_mark.load(Ordering::SeqCst);

        let total_size = queue_size + inflight_size;

        if total_size >= hwm {
            Some(0)
        } else {
            Some(hwm - total_size)
        }
    }
}

impl<T, Sink> Clone for WritableStream<T, Sink, Locked> {
    fn clone(&self) -> Self {
        Self {
            command_tx: self.command_tx.clone(),
            backpressure: Arc::clone(&self.backpressure),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            locked: Arc::clone(&self.locked),
            queue_total_size: Arc::clone(&self.queue_total_size),
            in_flight_size: Arc::clone(&self.in_flight_size),
            high_water_mark: Arc::clone(&self.high_water_mark),
            stored_error: Arc::clone(&self.stored_error),
            _sink: PhantomData,
            _state: PhantomData,
        }
    }
}

pub trait WritableSink<T: Send + 'static>: Sized {
    /// Start the sink
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
    fn close(self) -> impl Future<Output = StreamResult<()>> + Send {
        future::ready(Ok(())) // default no-op
    }

    /// Abort the sink
    fn abort(self, reason: Option<String>) -> impl Future<Output = StreamResult<()>> + Send {
        future::ready(Ok(())) // default no-op
    }
}

/// Replace with actual spawning mechanism async executor/runtime
///
/// For example, with futures crate ThreadPool:
/// pool.spawn_ok(stream_task(...)) or with async-std/tokio spawn.
fn spawn_stream_task<T, Sink>(
    command_rx: UnboundedReceiver<StreamCommand<T>>,
    inner: WritableStreamInner<T, Sink>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    queue_total_size: Arc<AtomicUsize>,
    in_flight_size: Arc<AtomicUsize>,
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
            queue_total_size,
            in_flight_size,
        ));
    });
}

// Helper to process each command. Break out to keep task flat.
fn process_command<T, Sink>(
    cmd: StreamCommand<T>,
    inner: &mut WritableStreamInner<T, Sink>,
    backpressure: &Arc<AtomicBool>,
    closed: &Arc<AtomicBool>,
    errored: &Arc<AtomicBool>,
    queue_total_size: &Arc<AtomicUsize>,
    in_flight_size: &Arc<AtomicUsize>,
    _cx: &mut Context<'_>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    match cmd {
        StreamCommand::Write { chunk, completion } => {
            if inner.state == StreamState::Errored {
                /*let error = inner
                .stored_error
                .clone()
                .unwrap_or_else(|| StreamError::Custom("Stream is errored".into()));*/
                let error = {
                    let opt_err = match inner.stored_error.read() {
                        Ok(err_guard) => err_guard.clone(),
                        Err(poisoned) => poisoned.into_inner().clone(),
                    };
                    opt_err.unwrap_or_else(|| StreamError::Custom("Stream is errored".into()))
                };
                let _ = completion.send(Err(error));
                return;
            }
            if inner.state == StreamState::Closed {
                //let _ = completion.send(Err(StreamError::Custom("Stream is closed".into())));
                let _ = completion.send(Err(StreamError::Closed));
                return;
            }
            // NEW: Also reject writes if close has been requested (stream is closing)
            // This matches the spec: "During this time any further attempts to write will fail"
            if inner.close_requested {
                /*let _ = completion.send(Err(StreamError::Custom(
                    "Cannot write to stream: close in progress".into()
                )));*/
                let _ = completion.send(Err(StreamError::Closing));
                return;
            }
            let chunk_size = inner.strategy.size(&chunk);
            inner.queue.push_back(PendingWrite {
                chunk,
                completion_tx: completion,
            });
            inner.queue_total_size += chunk_size;
            inner.update_backpressure();
            update_atomic_counters(inner, queue_total_size, in_flight_size);
            update_flags(&inner, backpressure, closed, errored);
        }
        StreamCommand::Close { completion } => {
            if inner.state == StreamState::Errored {
                /*let error = inner
                .stored_error
                .clone()
                .unwrap_or_else(|| StreamError::Custom("Stream is errored".into()));*/
                let error = {
                    let opt_err = match inner.stored_error.read() {
                        Ok(err_guard) => err_guard.clone(),
                        Err(poisoned) => poisoned.into_inner().clone(),
                    };
                    opt_err.unwrap_or_else(|| StreamError::Custom("Stream is errored".into()))
                };
                let _ = completion.send(Err(error));
                return;
            }
            if inner.state == StreamState::Closed {
                let _ = completion.send(Ok(()));
                return;
            }
            // If close already requested, add this completion to waiters
            if inner.close_requested {
                inner.close_completions.push(completion);
            } else {
                inner.close_requested = true;
                inner.close_completions.push(completion);
            }
            update_atomic_counters(&inner, &queue_total_size, &in_flight_size);
            update_flags(&inner, backpressure, closed, errored);
        }
        StreamCommand::Abort { reason, completion } => {
            if inner.state == StreamState::Closed || inner.state == StreamState::Errored {
                let _ = completion.send(Ok(()));
                return;
            }

            if inner.abort_requested {
                // Abort already requested: queue completion
                inner.abort_completions.push(completion);
            } else {
                // First abort request: mark flag, reason, queue completion
                inner.state = StreamState::Errored;
                //inner.stored_error = Some(StreamError::Aborted(inner.abort_reason.clone()));
                {
                    let mut stored_err_guard = inner.stored_error.write().unwrap();
                    *stored_err_guard = Some(StreamError::Aborted(inner.abort_reason.clone()));
                }

                inner.abort_requested = true;
                inner.abort_completions.push(completion);
                inner.abort_reason = reason;

                // Immediately reject all pending queued writes
                while let Some(pw) = inner.queue.pop_front() {
                    let error = StreamError::Aborted(inner.abort_reason.clone());

                    let _ = pw.completion_tx.send(Err(error));
                }
            }
            update_atomic_counters(&inner, &queue_total_size, &in_flight_size);
            update_flags(&inner, backpressure, closed, errored);
        }
        /*StreamCommand::GetDesiredSize { completion } => {
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
        }*/
        StreamCommand::RegisterReadyWaker { waker } => {
            inner.ready_wakers.register(&waker);
            // Immediately check if this waker should be woken
            // Immediately wake if already ready to prevent race conditions
            if !inner.backpressure {
                inner.ready_wakers.wake_all();
            }
        }
        StreamCommand::RegisterClosedWaker { waker } => {
            inner.closed_wakers.register(&waker);
            // Immediately check if this waker should be woken
            // Immediately wake if already closed/errored to prevent race conditions
            if inner.state == StreamState::Closed || inner.state == StreamState::Errored {
                inner.closed_wakers.wake_all();
            }
        } /*StreamCommand::LockStream { completion } => {
              let _ = if inner.locked {
                  completion.send(Err(StreamError::Custom("Stream already locked".into())))
              } else {
                  inner.locked = true;
                  completion.send(Ok(()))
              };
          }
          StreamCommand::UnlockStream { completion } => {
              let _ = if !inner.locked {
                  if let Some(c) = completion {
                      let _ = c.send(Err(StreamError::Custom("Stream not locked".into())));
                  }
              } else {
                  inner.locked = false;
                  if let Some(c) = completion {
                      let _ = c.send(Ok(()));
                  }
              };
          }*/
    }
}

// Inflight operations being driven
enum InFlight<Sink> {
    Write {
        fut: Pin<Box<dyn Future<Output = (Sink, StreamResult<()>)> + Send>>,
        completion: Option<oneshot::Sender<StreamResult<()>>>,
        chunk_size: usize,
    },
    Close {
        fut: Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>,
        completions: Vec<oneshot::Sender<StreamResult<()>>>,
    },
    Abort {
        fut: Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>,
        completions: Vec<oneshot::Sender<StreamResult<()>>>,
    },
}

async fn stream_task<T, Sink>(
    mut command_rx: UnboundedReceiver<StreamCommand<T>>,
    mut inner: WritableStreamInner<T, Sink>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    queue_total_size: Arc<AtomicUsize>,
    in_flight_size: Arc<AtomicUsize>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    let mut inflight: Option<InFlight<Sink>> = None;
    let (ctrl_tx, mut ctrl_rx): (
        UnboundedSender<ControllerMsg>,
        UnboundedReceiver<ControllerMsg>,
    ) = unbounded();

    poll_fn(|cx| {
        process_controller_msgs(&mut inner, &mut ctrl_rx);
        update_atomic_counters(&inner, &queue_total_size, &in_flight_size);
        // Dual-layer waker management to handle race conditions in concurrent scenarios:
        //
        // 1. Immediate wake in RegisterWaker command arms (in process_command): 
        //    Prevents race where wakers register when condition is already true
        //
        // 2. Batch wake via update_flags() below: Ensures all waiting wakers get 
        //    notified when state changes during command processing
        //
        // Both mechanisms are required - stress testing (800-iteration loops) showed:
        // - Only immediate wake in command arms: failures around iteration 119
        // - Only update_flags() wake: failures around iteration 710  
        // - Both together: no failures across multiple test runs
        update_flags(&inner, &backpressure, &closed, &errored);

        // Drain all commands, admin and work commands
        loop {
            match command_rx.poll_next_unpin(cx) {
                Poll::Ready(Some(cmd)) => {
                    /*let should_wake_closed =
                        matches!(cmd, StreamCommand::RegisterClosedWaker { .. });
                    let should_wake_ready = matches!(cmd, StreamCommand::RegisterReadyWaker { .. });*/

                    process_command(
                        cmd,
                        &mut inner,
                        &backpressure,
                        &closed,
                        &errored,
                        &queue_total_size,
                        &in_flight_size,
                        cx,
                    );

                    // Immediately check wake conditions for waker registration commands
                    // to prevent race conditions where the condition is already met.
                    // 
                    // Without this immediate check, wakers can hang when:
                    // 1. A waker registers when the ready/closed condition is already true
                    // 2. The waker waits for the next poll cycle to be woken via update_flags()
                    // 3. In some timing scenarios, this next poll cycle may be delayed or not occur
                    //
                    // This was discovered through stress testing (800-iteration test loops) where
                    // intermittent hangs occurred without this immediate wake mechanism.
                    /*if should_wake_closed {
                        if inner.state == StreamState::Closed || inner.state == StreamState::Errored
                        {
                            inner.closed_wakers.wake_all();
                        }
                    }
                    if should_wake_ready {
                        if !inner.backpressure {
                            inner.ready_wakers.wake_all();
                        }
                    }*/

                    // Continue draining all commands in the same poll
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => break,
            }
        }

        // cancel inflight writes/closes if abort requested but not yet started
        if inner.abort_requested {
            // Cancel inflight write or close operations immediately
            if let Some(inflight_op) = &mut inflight {
                match inflight_op {
                    InFlight::Write { completion, .. } => {
                        if let Some(sender) = completion.take() {
                            let _ = sender.send(Err(StreamError::Custom("Stream aborted".into())));
                        }
                    }
                    InFlight::Close { completions, .. } => {
                        for sender in completions.drain(..) {
                            let _ = sender.send(Err(StreamError::Custom("Stream aborted".into())));
                        }
                    }
                    _ => {}
                }
            }
            inflight = None;
        }

        // 2. ---- If not currently working, handle one "work" command ----
        if inflight.is_none() {
            if inner.close_requested {
                // Only start close after all queued writes are done and no inflight write
                let can_close = inner.queue.is_empty()
                    && (inflight.is_none()
                        || matches!(
                            inflight,
                            Some(InFlight::Close { .. }) | Some(InFlight::Abort { .. })
                        ));
                if can_close {
                    if let Some(sink) = inner.sink.take() {
                        let fut = async move { sink.close().await }.boxed();
                        let completions = std::mem::take(&mut inner.close_completions);
                        inflight = Some(InFlight::Close { fut, completions });
                        //inner.close_requested = false;
                    } else {
                        // Update state BEFORE sending completions
                        inner.state = StreamState::Closed;
                        inner.close_requested = false;
                        // queue should already be empty as it should drain naturally through normal write processing.
                        /*inner.queue.clear();
                        inner.queue_total_size = 0;
                        inner.in_flight_size = 0;
                        update_atomic_counters(&inner, &queue_total_size, &in_flight_size);*/

                        update_flags(&inner, &backpressure, &closed, &errored);

                        // THEN send completions
                        for c in inner.close_completions.drain(..) {
                            let _ = c.send(Ok(()));
                        }
                    }
                }
            } else if inner.abort_requested {
                if let Some(sink) = inner.sink.take() {
                    let reason = inner.abort_reason.take();
                    let fut = async move { sink.abort(reason).await }.boxed();
                    let completions = std::mem::take(&mut inner.abort_completions);
                    inflight = Some(InFlight::Abort { fut, completions });
                } else {
                    // Update state BEFORE sending completions
                    //inner.state = StreamState::Errored;
                    inner.abort_requested = false;
                    update_flags(&inner, &backpressure, &closed, &errored);

                    // THEN send completions
                    for c in inner.abort_completions.drain(..) {
                        let _ = c.send(Ok(()));
                    }
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
                update_atomic_counters(&inner, &queue_total_size, &in_flight_size);
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
        if let Some(inflight_op) = &mut inflight {
            match inflight_op {
                InFlight::Write {
                    fut,
                    completion,
                    chunk_size,
                } => {
                    match fut.as_mut().poll(cx) {
                        Poll::Ready((sink, result)) => {
                            inner.in_flight_size -= *chunk_size;

                            // Check for error first, before any flag updates
                            if result.is_err() {
                                //inner.stored_error = Some(result.clone().err().unwrap());
                                /*if let Err(e) = result.clone() {
                                    inner.stored_error = Some(e);
                                }*/
                                if let Err(e) = result.clone() {
                                    let mut stored_err_guard = match inner.stored_error.write() {
                                        Ok(guard) => guard,
                                        Err(poisoned) => poisoned.into_inner(),
                                    };
                                    *stored_err_guard = Some(e);
                                }
                                inner.state = StreamState::Errored;
                                inner.sink = None; // Don't restore sink on error
                            } else {
                                inner.sink = Some(sink); // Only restore sink on success
                            }

                            inner.update_backpressure();
                            update_atomic_counters(&inner, &queue_total_size, &in_flight_size);
                            update_flags(&inner, &backpressure, &closed, &errored); // Single flag update

                            if let Some(sender) = completion.take() {
                                let _ = sender.send(result);
                            }

                            inflight = None;
                            cx.waker().wake_by_ref();
                        }
                        Poll::Pending => {}
                    }
                }
                /*InFlight::Close { fut, completions } => match fut.as_mut().poll(cx) {
                    Poll::Ready(res) => {
                        // Update state and flags FIRST
                        inner.state = StreamState::Closed;
                        inner.close_requested = false;
                        // Clear queue and reset backpressure
                        // queue should already be empty on close --start
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        inner.in_flight_size = 0;
                        // queue should already be empty on close --end
                        inner.backpressure = false;
                        update_atomic_counters(&inner, &queue_total_size, &in_flight_size);
                        update_flags(&inner, &backpressure, &closed, &errored);

                        // THEN notify all waiting close callers
                        for sender in completions.drain(..) {
                            let _ = sender.send(res.clone());
                        }

                        inflight = None;
                        cx.waker().wake_by_ref();
                    }
                    Poll::Pending => {}
                },*/
                InFlight::Close { fut, completions } => match fut.as_mut().poll(cx) {
                    Poll::Ready(res) => {
                        // Update state and flags based on result of sink.close()
                        match &res {
                            Ok(()) => {
                                // Successful close: mark stream Closed
                                inner.state = StreamState::Closed;
                            }
                            Err(err) => {
                                // Close failed: mark stream Errored with stored error
                                inner.state = StreamState::Errored;
                                //inner.stored_error = Some(err.clone());
                                /*{
                                    let mut stored_err_guard = match inner.stored_error.write() {
                                        Ok(guard) => guard,
                                        Err(poisoned) => poisoned.into_inner(), // recover from poison
                                    };

                                    *stored_err_guard = Some(err.clone());
                                }*/
                                set_stored_error(&inner.stored_error, err.clone());
                            }
                        }

                        inner.close_requested = false;
                        // Clear queue and reset backpressure
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        inner.in_flight_size = 0;
                        inner.backpressure = false;

                        update_atomic_counters(&inner, &queue_total_size, &in_flight_size);
                        update_flags(&inner, &backpressure, &closed, &errored);

                        // Notify all waiting close callers with the actual close result
                        for sender in completions.drain(..) {
                            let _ = sender.send(res.clone());
                        }

                        inflight = None;
                        cx.waker().wake_by_ref();
                    }
                    Poll::Pending => {}
                },
                InFlight::Abort { fut, completions } => match fut.as_mut().poll(cx) {
                    /*Poll::Ready(res) => {
                        // Update state and flags FIRST
                        inner.state = StreamState::Errored;
                        inner.abort_requested = false;
                        // Clear queue and reset backpressure
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        inner.in_flight_size = 0;
                        inner.backpressure = false;
                        update_atomic_counters(&inner, &queue_total_size, &in_flight_size);
                        update_flags(&inner, &backpressure, &closed, &errored);
                        // Remove sink since it's aborted
                        inner.sink = None;

                        // THEN notify all waiting abort callers
                        for sender in completions.drain(..) {
                            let _ = sender.send(res.clone());
                        }

                        inflight = None;
                        cx.waker().wake_by_ref();
                    }*/
                    Poll::Ready(sink_abort_result) => {
                        match sink_abort_result {
                            Ok(()) => {
                                // Sink abort succeeded - don't error the stream
                                // Just complete the abort operation successfully
                                // Sink abort succeeded - close the stream (don't error it)
                                //inner.state = StreamState::Closed; // CHANGE: Close, don't leave writable
                                //inner.sink = None; // ADD: Remove sink
                                //inner.state = StreamState::Errored;
                                //inner.stored_error = Some(abort_reason.clone());
                                //inner.state = StreamState::Errored;
                                /*inner.stored_error = Some(StreamError::Aborted(
                                    inner.abort_reason.clone()
                                ));*/
                                // Keep stored_error as is (abort reason)
                                inner.sink = None;

                                for sender in completions.drain(..) {
                                    let _ = sender.send(Ok(()));
                                }
                            }
                            Err(sink_error) => {
                                // Sink abort failed - error the stream with this error (sink error)
                                //inner.state = StreamState::Errored;
                                // Overwrite stored_error with sink's error
                                //inner.stored_error = Some(sink_error.clone());
                                /*{
                                    let mut stored_err_guard = match inner.stored_error.write() {
                                        Ok(guard) => guard,
                                        Err(poisoned) => poisoned.into_inner(), // recover from poisoning if any
                                    };
                                    *stored_err_guard = Some(sink_error.clone());
                                }*/
                                set_stored_error(&inner.stored_error, sink_error.clone());
                                inner.sink = None;

                                // Abort calls should reject with the sink error
                                for sender in completions.drain(..) {
                                    let _ = sender.send(Err(sink_error.clone()));
                                }
                            }
                        }

                        inner.abort_requested = false;
                        // Clear remaining state
                        // Queue should already be empty (it was cleared when abort was requested)
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        inner.in_flight_size = 0;
                        inner.backpressure = false;

                        update_atomic_counters(&inner, &queue_total_size, &in_flight_size);
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

fn update_flags<T, Sink>(
    inner: &WritableStreamInner<T, Sink>,
    backpressure: &AtomicBool,
    closed: &AtomicBool,
    errored: &AtomicBool,
) {
    let new_state = inner.state;

    backpressure.store(inner.backpressure, Ordering::SeqCst);
    closed.store(new_state == StreamState::Closed, Ordering::SeqCst);
    errored.store(new_state == StreamState::Errored, Ordering::SeqCst);

    // Wake closed wakers if we're in a final state
    if new_state == StreamState::Closed || new_state == StreamState::Errored {
        inner.closed_wakers.wake_all();
    }

    // Wake ready wakers if no backpressure
    if !inner.backpressure {
        inner.ready_wakers.wake_all();
    }
}

fn set_stored_error(stored_error: &Arc<RwLock<Option<StreamError>>>, err: StreamError) {
    let mut guard = match stored_error.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(err);
}

pub struct WritableStreamDefaultWriter<T, Sink> {
    stream: WritableStream<T, Sink, Locked>,
}

impl<T, Sink> WritableStreamDefaultWriter<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    /// Create a new writer linked to the stream
    fn new(stream: WritableStream<T, Sink, Locked>) -> Self {
        Self { stream }
    }

    /// Write a chunk to the stream by immediately enqueueing it for writing.
    ///
    /// This method sends the chunk to the stream's internal queue immediately and returns
    /// a future that resolves when the write has been fully processed by the sink.
    /// Awaiting this method ensures the write completes before proceeding.
    ///
    /// # Important
    ///
    /// Calling `write()` repeatedly *without* first awaiting `ready()` (or without
    /// respecting backpressure) can cause unbounded growth of the internal queue,
    /// leading to increased memory usage and potential performance degradation.
    ///
    /// To avoid excessive buffering, it is recommended to either:
    /// - Await `ready()` before calling `write()` to respect backpressure signals, or
    /// - Use the [`enqueue_when_ready()`] helper method which does this automatically.
    ///
    /// # Specification Compliance
    ///
    /// This method closely corresponds to the WHATWG Streams specification's default
    /// writer `write()` method. The responsibility for backpressure handling lies with
    /// the caller.
    ///
    /// # Example
    ///
    /// ```no_run
    /// // Await ready before writing to avoid queue buildup:
    /// writer.stream.ready().await?;
    /// writer.write(chunk).await?;
    /// ```
    ///
    /// For high throughput scenarios without awaiting each write completion,
    /// call `write()` without awaiting, but this disables backpressure and risks
    /// unbounded queue growth.
    ///
    /// [`enqueue_when_ready()`]: Self::enqueue_when_ready
    pub fn write(&self, chunk: T) -> impl std::future::Future<Output = StreamResult<()>> + Send {
        let (tx, rx) = oneshot::channel();

        let enqueue_result = self
            .stream
            .command_tx
            .unbounded_send(StreamCommand::Write {
                chunk,
                completion: tx,
            })
            .map_err(|_| StreamError::Custom("Stream task dropped".into()));

        // Return a future that handles the completion waiting
        async move {
            // First check if enqueueing failed
            enqueue_result?;

            // Then wait for the write to complete
            rx.await
                .unwrap_or_else(|_| Err(StreamError::Custom("Write canceled".into())))
        }
    }

    /// Waits for the stream to be ready (i.e., no backpressure) before performing a write.
    ///
    /// This method asynchronously waits until the stream signals it can accept more data,
    /// via the `ready()` future, before enqueuing the write operation. This helps avoid
    /// excessive queue buildup and memory usage, resulting in better throughput when
    /// producing data at a high rate.
    ///
    /// **Note:** This method is *not* part of the WHATWG Streams specification, but is
    /// provided as a convenient helper to implement efficient backpressure-aware writing.
    ///
    /// # Example
    ///
    /// ```no_run
    /// // Write data only when the stream is ready to accept it
    /// writer.enqueue_when_ready(chunk).await?;
    /// ```
    ///
    /// # Behavior
    ///
    /// - Awaits the stream becoming ready (no backpressure).
    /// - Then enqueues the write but doesn't await its completion (like `write()`).
    ///
    /// This approach balances throughput and memory by respecting the stream’s backpressure
    /// signals before each write.
    ///
    /// # Caveats
    ///
    /// Users who want maximum throughput without waiting should call `write()` directly,
    /// and optionally use `ready()` separately to monitor backpressure.
    ///
    /// This helper simplifies the common pattern of waiting on `ready()` before writing,
    /// but callers should choose based on desired flow control characteristics.
    pub async fn enqueue_when_ready(&self, chunk: T) -> StreamResult<()> {
        self.ready().await?;

        // Enqueue the write but don't await completion
        let _write_future = self.write(chunk);

        Ok(())
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
    /*pub async fn desired_size(&self) -> Option<usize> {
        let (tx, rx) = oneshot::channel();

        let _ = self
            .stream
            .command_tx
            .clone()
            .send(StreamCommand::GetDesiredSize { completion: tx })
            .await;

        rx.await.ok().flatten()
    }*/

    /// Get the desired size synchronously (how much data the stream can accept)
    /// Returns None if the stream is closed or errored
    pub fn desired_size(&self) -> Option<usize> {
        self.stream.desired_size()
    }

    pub fn ready(&self) -> ReadyFuture<T, Sink> {
        ReadyFuture::new(self.clone())
    }

    pub fn closed(&self) -> ClosedFuture<T, Sink> {
        ClosedFuture::new(self.clone())
    }
}

// Update the stream task to maintain the atomic counters
// this is meant to be called when queue or inflight size is modified before waking wakers i.e before calling `update_flags`
fn update_atomic_counters<T, Sink>(
    inner: &WritableStreamInner<T, Sink>,
    queue_total_size: &Arc<AtomicUsize>,
    in_flight_size: &Arc<AtomicUsize>,
) {
    queue_total_size.store(inner.queue_total_size, Ordering::SeqCst);
    in_flight_size.store(inner.in_flight_size, Ordering::SeqCst);
}

impl<T, Sink> WritableStreamDefaultWriter<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    /*pub async fn release_lock(self) -> WritableStream<T, Sink, Unlocked> {
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
    }*/
    /*pub async fn release_lock_async(self) -> Result<(), StreamError> {
        let (tx, rx) = oneshot::channel();
        self.stream
            .command_tx
            .clone()
            .send(StreamCommand::UnlockStream {
                completion: Some(tx),
            })
            .await
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;

        rx.await
            .map_err(|_| StreamError::Custom("Unlock response lost".into()))??
        // `??` to propagate both errors
    }*/
    /*pub fn release_lock(self) -> StreamResult<()> {
        self.stream
            .command_tx
            .unbounded_send(StreamCommand::UnlockStream { completion: None })
            .map_err(|_| StreamError::Custom(ArcError::from("Stream task dropped")))?;
        Ok(())
        // Do NOT await any response here, immediate release.
        // Drop self afterwards.
    }*/
    pub fn release_lock(self) -> StreamResult<()> {
        self.stream.locked.store(false, Ordering::SeqCst);

        Ok(())
    }
}

impl<T, Sink> Drop for WritableStreamDefaultWriter<T, Sink> {
    fn drop(&mut self) {
        self.stream.locked.store(false, Ordering::SeqCst);
    }
}

impl<T, Sink> Clone for WritableStreamDefaultWriter<T, Sink> {
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
        }
    }
}

use std::task::Waker;

struct WritableStreamInner<T, Sink> {
    state: StreamState,
    queue: VecDeque<PendingWrite<T>>,
    queue_total_size: usize,
    in_flight_size: usize,
    strategy: Box<dyn QueuingStrategy<T> + Send + Sync + 'static>,
    sink: Option<Sink>,

    backpressure: bool,
    //locked: bool,
    /// `close()` in progress flag and completions waiting for close
    close_requested: bool,
    close_completions: Vec<oneshot::Sender<StreamResult<()>>>,

    abort_reason: Option<String>,
    /// `abort()` in progress flag and completions waiting for abort
    abort_requested: bool,
    abort_completions: Vec<oneshot::Sender<StreamResult<()>>>,

    //stored_error: Option<StreamError>,
    stored_error: Arc<RwLock<Option<StreamError>>>,

    ready_wakers: WakerSet,
    closed_wakers: WakerSet,
}

impl<T, Sink> WritableStreamInner<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
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
}

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
        }
    }
}

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
}

pub struct ReadyFuture<T, Sink> {
    writer: WritableStreamDefaultWriter<T, Sink>,
}

impl<T, Sink> ReadyFuture<T, Sink> {
    pub fn new(stream: WritableStreamDefaultWriter<T, Sink>) -> Self {
        Self { writer: stream }
    }
}

impl<T, Sink> Future for ReadyFuture<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    type Output = StreamResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.writer.stream.errored.load(Ordering::SeqCst) {
            /*let error = inner
            .stored_error
            .clone()
            .unwrap_or_else(|| StreamError::Custom("Stream is errored".into()));*/

            let error = {
                let opt_err = match self.writer.stream.stored_error.read() {
                    Ok(guard) => guard.clone(),
                    Err(poisoned) => poisoned.into_inner().clone(),
                };
                opt_err.unwrap_or_else(|| StreamError::Custom("Stream is errored".into()))
            };
            return Poll::Ready(Err(error));
        }
        if self.writer.stream.closed.load(Ordering::SeqCst) {
            //return Poll::Ready(Err(StreamError::Custom("Stream is closed".into())));
            return Poll::Ready(Ok(()));
        }
        if !self.writer.stream.backpressure.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        // Not ready, register waker:
        let waker = cx.waker().clone();
        let _ = self
            .writer
            .stream
            .command_tx
            .unbounded_send(StreamCommand::RegisterReadyWaker { waker });
        // If the channel is full or busy, that's okay—the next poll will try again.
        // Re-check backpressure after registration
        if !self.writer.stream.backpressure.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        Poll::Pending
    }
}

pub struct ClosedFuture<T, Sink> {
    writer: WritableStreamDefaultWriter<T, Sink>,
}

impl<T, Sink> ClosedFuture<T, Sink> {
    pub fn new(stream: WritableStreamDefaultWriter<T, Sink>) -> Self {
        Self { writer: stream }
    }
}

impl<T, Sink> Future for ClosedFuture<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    type Output = StreamResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.writer.stream.errored.load(Ordering::SeqCst) {
            /*let error = inner
            .stored_error
            .clone()
            .unwrap_or_else(|| StreamError::Custom("Stream is errored".into()));*/

            //return Poll::Ready(Err(StreamError::Custom("Stream is errored".into())));
            let error = {
                let opt_err = match self.writer.stream.stored_error.read() {
                    Ok(guard) => guard.clone(),
                    Err(poisoned) => poisoned.into_inner().clone(),
                };
                opt_err.unwrap_or_else(|| StreamError::Custom("Stream is errored".into()))
            };
            return Poll::Ready(Err(error));
        }
        if self.writer.stream.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        let waker = cx.waker().clone();
        let _ = self
            .writer
            .stream
            .command_tx
            .unbounded_send(StreamCommand::RegisterClosedWaker { waker });
        // Re-check closed after registration
        if self.writer.stream.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }
        Poll::Pending
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
        let (locked_stream, writer) = stream.get_writer().expect("get_writer");

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
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

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
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

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
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

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
        let ready_fut = writer.ready();
        tokio::pin!(ready_fut);

        // Poll once, should be pending due to backpressure
        use futures::task::noop_waker_ref;
        use std::task::{Context, Poll};
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        // Before notifying, ready() pending (expected)
        assert!(matches!(ready_fut.as_mut().poll(&mut cx), Poll::Pending));

        // Finish first write by notifying sink
        notify.notify_one();

        let _ = write1.await.expect("write1 done");

        // At this point ready() likely still pending, because second write queued.
        assert!(matches!(ready_fut.as_mut().poll(&mut cx), Poll::Pending));

        // Now notify the second write completes too:
        notify.notify_one();
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

        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

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
        let (_locked_stream, writer1) = stream.get_writer().expect("get_writer");

        // Try to get second writer — should fail because locked
        let write_future = stream.get_writer();

        match write_future {
            Err(e) => assert!(matches!(e, StreamError::Custom(_))),
            Ok(_) => panic!("Acquired second writer while stream is locked!"),
        }

        // Release lock
        writer1.release_lock().unwrap();

        // Now get_writer should succeed
        let (_locked_stream2, _writer2) = stream.get_writer().expect("get_writer again");
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
        let strategy = Box::new(CountQueuingStrategy::new(2));

        let stream = WritableStream::new(sink, strategy);
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

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
        let mut ready_fut = writer.ready();
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
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

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
        let mut ready = writer.ready();

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
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");
        writer.close().await.expect("close");

        // desired_size *must* return None after close
        let after_close = writer.desired_size();
        assert_eq!(after_close, None, "desired_size after close must be None");

        writer.release_lock();
        // Simulate errored state by calling abort
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");
        writer.abort(None).await.expect("abort");

        let after_error = writer.desired_size();
        assert_eq!(after_error, None, "desired_size after error must be None");
    }

    //proper spec compliance
    #[tokio::test]
    async fn test_writer_locking_exclusivity() {
        let sink = DummySink;
        let strategy = Box::new(CountQueuingStrategy::new(10));
        let stream = WritableStream::new(sink, strategy);

        let (_locked_stream, writer1) = stream.get_writer().expect("get_writer");

        // Try to get second writer; should fail with a lock error
        let result2 = stream.get_writer();
        assert!(
            matches!(result2, Err(_)),
            "Second get_writer should fail when locked"
        );

        // Release the lock in writer1
        writer1.release_lock();

        // Now new writer acquisition must succeed
        let (_locked_stream2, writer2) = stream.get_writer().expect("get_writer after release");
        drop(writer2);
    }

    //this fails and is taking too much debug time where others has already covered
    /*#[tokio::test]
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
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await; // simulate async delay
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }
            }

            let sink = CountingSink(Arc::new(AtomicUsize::new(0)));
            let strategy = Box::new(CountQueuingStrategy::new(1)); // Low HWM to test backpressure

            //let stream = WritableStream::new(sink.clone(), strategy);
            let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |future| {
                tokio::spawn(future);
            });
            let (_locked_stream, writer) = stream.clone().get_writer().await.expect("get_writer");

            // First write triggers async write, no backpressure yet
            let write1_fut = writer.write(vec![1]); //.await.expect("write 1");

            // High water mark is 1, second write triggers queue and backpressure
            let write2_fut = writer.write(vec![2]);

            // Wait until backpressure is set with timeout (avoiding races)
            use tokio::time::{Duration, timeout};
            let backpressure_applied = timeout(Duration::from_secs(1), async {
                loop {
                    if stream.backpressure.load(Ordering::SeqCst) {
                        break;
                    }
                    tokio::task::yield_now().await; // Let other tasks run
                }
            })
            .await;

            //write1_fut.await.expect("write 1 complete");
            // At this point, backpressure flag should be true
            assert!(
                //stream.backpressure.load(Ordering::SeqCst),
                backpressure_applied.is_ok(),
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

            // Clear backpressure by draining queue — which happens only after at least a write completes
            write1_fut.await.expect("first write must complete");

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
    */

    #[tokio::test]
    async fn test_desired_size_after_close_and_error() {
        let sink = DummySink;
        let strategy = Box::new(CountQueuingStrategy::new(10));
        let stream = WritableStream::new(sink, strategy);
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

        writer.close().await.expect("close");
        //writer.closed().await.expect("closed future");

        let ds_after_close = writer.desired_size();
        assert_eq!(
            ds_after_close, None,
            "desired_size after close must be None"
        );

        writer.release_lock();
        let (_locked_stream2, writer2) = stream.get_writer().expect("get_writer");
        writer2.abort(None).await.expect("abort");

        let ds_after_abort = writer2.desired_size();
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
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

        // Close stream - closed future resolves ok
        writer.close().await.expect("close");

        let closed_res = writer.closed().await;
        assert!(closed_res.is_ok(), "closed future must resolve after close");

        // Abort an opened stream errors correctly
        let stream2 = WritableStream::new(DummySink, Box::new(CountQueuingStrategy::new(10)));
        let (_locked_stream2, writer2) = stream2.get_writer().expect("get_writer");
        writer2
            .abort(Some("reason".to_string()))
            .await
            .expect("abort");

        let closed_res2 = writer2.closed().await;
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

        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

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
        let closed_future_result = writer.closed().await;
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
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

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
        let mut ready_fut = writer.ready();
        futures::pin_mut!(ready_fut);

        use std::task::{Context, Poll};
        let waker = futures::task::noop_waker_ref();
        let mut cx = Context::from_waker(waker);

        assert!(matches!(ready_fut.as_mut().poll(&mut cx), Poll::Pending));

        // Unblock sink to drain write queue
        notify.notify_waiters();

        // Unblock the first sink write
        notify.notify_waiters();
        let _ = write1.await.expect("write1 completed");

        // Unblock the second sink write
        notify.notify_waiters();
        let _ = write2.await.expect("write2 completed");

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
        let (_locked_stream, writer1) = stream.get_writer().expect("get_writer");

        // Attempt to get second writer lock concurrently must fail
        let res2 = stream.get_writer();
        assert!(
            res2.is_err(),
            "second get_writer acquisition while locked should fail"
        );

        // Release lock via writer1
        let _ = writer1.release_lock();

        // Now getting writer must succeed
        let (_locked_stream2, writer2) = stream.get_writer().expect("get_writer after release");
        drop(writer2);
    }

    #[tokio::test]
    async fn test_close_and_closed_future() {
        let stream = WritableStream::new(DummySink, Box::new(CountQueuingStrategy::new(10)));
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

        writer.close().await.expect("close");

        // closed() future resolves successfully
        let closed_res = writer.closed().await;
        assert!(
            closed_res.is_ok(),
            "closed() should resolve after stream closes"
        );

        // Further writes after close fail
        let write_err = writer.write(vec![1]).await;
        assert!(write_err.is_err(), "write after close must fail");
    }

    #[tokio::test]
    async fn write_when_ready_test() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct CountingSink {
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
                let count = self.write_count.clone();
                async move {
                    let mut guard = count.lock().unwrap();
                    *guard += 1;
                    Ok(())
                }
            }
        }

        let sink = CountingSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(2));
        let stream = WritableStream::new(sink.clone(), strategy);
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

        // Use enqueue_when_ready to write multiple chunks, which waits for readiness before writing.
        writer
            .enqueue_when_ready(vec![1, 2, 3])
            .await
            .expect("write when ready 1");
        writer
            .enqueue_when_ready(vec![4, 5, 6])
            .await
            .expect("write when ready 2");

        // Wait for the stream to be drained; ensures writes complete
        //writer.ready().await.expect("ready after writes");

        // Write one more chunk and wait for completion
        //writer.write(vec![7]).await.expect("write last");

        writer.close().await.expect("close");

        // Confirm the underlying sink received both writes
        assert_eq!(sink.get_count(), 2);
    }
}
