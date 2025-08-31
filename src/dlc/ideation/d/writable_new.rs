use super::{Locked, QueuingStrategy, Unlocked, errors::StreamError};
use futures::FutureExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::channel::oneshot;
use futures::future::poll_fn;
use futures::{AsyncWrite, SinkExt, StreamExt, future};
use futures::{channel::mpsc::UnboundedReceiver, task::AtomicWaker};
use futures_util::future::AbortHandle;
use pin_project::pin_project;
use std::collections::VecDeque;
use std::future::Future;
use std::io::{Error as IoError, ErrorKind};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};

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
    Flush {
        completion: oneshot::Sender<StreamResult<()>>,
    },
    Close {
        completion: oneshot::Sender<StreamResult<()>>,
    },
    Abort {
        reason: Option<String>,
        completion: oneshot::Sender<StreamResult<()>>,
    },
    RegisterReadyWaker {
        waker: Waker,
    },
    RegisterClosedWaker {
        waker: Waker,
    },
}

/// Public handle, clonable, holds task command sender and atomic flags
#[pin_project]
pub struct WritableStream<T, Sink, S = Unlocked> {
    command_tx: UnboundedSender<StreamCommand<T>>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    locked: Arc<AtomicBool>,
    queue_total_size: Arc<AtomicUsize>,
    high_water_mark: Arc<AtomicUsize>,
    stored_error: Arc<RwLock<Option<StreamError>>>,
    _sink: PhantomData<Sink>,
    _state: PhantomData<S>,
    #[pin]
    flush_receiver: Option<oneshot::Receiver<StreamResult<()>>>,
    #[pin]
    close_receiver: Option<oneshot::Receiver<Result<(), StreamError>>>,
    #[pin]
    write_receiver: Option<oneshot::Receiver<Result<(), StreamError>>>,
    pending_write_len: Option<usize>,
    pub(crate) controller: Arc<WritableStreamDefaultController>,
}

impl<T, Sink, S> WritableStream<T, Sink, S> {
    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }

    fn get_stored_error(&self) -> StreamError {
        self.stored_error
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| StreamError::Custom("Stream is errored".into()))
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
            return Err(StreamError::Custom("Stream already locked".into()));
        }

        let locked = WritableStream {
            command_tx: self.command_tx.clone(),
            backpressure: Arc::clone(&self.backpressure),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            locked: Arc::clone(&self.locked),
            queue_total_size: Arc::clone(&self.queue_total_size),
            high_water_mark: Arc::clone(&self.high_water_mark),
            stored_error: Arc::clone(&self.stored_error),
            _sink: PhantomData,
            _state: PhantomData::<Locked>,
            flush_receiver: None,
            close_receiver: None,
            write_receiver: None,
            pending_write_len: None,
            controller: self.controller.clone(),
        };

        Ok((locked.clone(), WritableStreamDefaultWriter::new(locked)))
    }
}

impl<T, Sink> WritableStream<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    /// Create a new WritableStream and spawn a dedicated thread for the stream task.
    ///
    /// For custom executor control, use [`new_with_spawn()`] instead.
    pub fn new(sink: Sink, strategy: Box<dyn QueuingStrategy<T> + Send + Sync + 'static>) -> Self {
        Self::new_with_spawn(sink, strategy, |fut| {
            std::thread::spawn(move || {
                futures::executor::block_on(fut);
            });
        })
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
            strategy,
            sink: Some(sink),
            backpressure: false,
            close_requested: false,
            close_completions: Vec::new(),
            abort_reason: None,
            abort_requested: false,
            abort_completions: Vec::new(),
            stored_error: Arc::clone(&stored_error),
            ready_wakers: WakerSet::new(),
            closed_wakers: WakerSet::new(),
            flush_completions: Vec::new(),
            pending_flush_commands: Vec::new(),
        };

        let backpressure = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let locked = Arc::new(AtomicBool::new(false));
        let queue_total_size = Arc::new(AtomicUsize::new(0));

        let (ctrl_tx, ctrl_rx): (
            UnboundedSender<ControllerMsg>,
            UnboundedReceiver<ControllerMsg>,
        ) = unbounded();
        let controller = WritableStreamDefaultController::new(ctrl_tx.clone());

        let fut = stream_task(
            command_rx,
            inner,
            Arc::clone(&backpressure),
            Arc::clone(&closed),
            Arc::clone(&errored),
            Arc::clone(&queue_total_size),
            controller.clone(),
            ctrl_rx,
        );

        spawn_fn(Box::pin(fut));

        Self {
            command_tx,
            backpressure,
            closed,
            errored,
            locked,
            queue_total_size,
            high_water_mark,
            stored_error,
            _sink: PhantomData,
            _state: PhantomData,
            flush_receiver: None,
            close_receiver: None,
            write_receiver: None,
            pending_write_len: None,
            controller: controller.into(),
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
        let hwm = self.high_water_mark.load(Ordering::SeqCst);

        if queue_size >= hwm {
            Some(0)
        } else {
            Some(hwm - queue_size)
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
            high_water_mark: Arc::clone(&self.high_water_mark),
            stored_error: Arc::clone(&self.stored_error),
            _sink: PhantomData,
            _state: PhantomData,
            flush_receiver: None,
            close_receiver: None,
            write_receiver: None,
            pending_write_len: None,
            controller: self.controller.clone(),
        }
    }
}

impl<T, SinkType> futures::Sink<T> for WritableStream<T, SinkType, Unlocked>
where
    T: Send + 'static,
    SinkType: WritableSink<T> + Send + 'static,
{
    type Error = StreamError;
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Check if stream is in an error state
        if self.errored.load(Ordering::SeqCst) {
            return Poll::Ready(Err(self.get_stored_error()));
        }

        // Check if stream is closed
        if self.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Err(StreamError::Closed));
        }

        // Check if there's backpressure
        if !self.backpressure.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            // Register waker to get notified when backpressure clears:
            let waker = cx.waker().clone();
            let _ = self
                .command_tx
                .unbounded_send(StreamCommand::RegisterReadyWaker { waker });

            // Double-check backpressure after registering waker to avoid race conditions
            if !self.backpressure.load(Ordering::SeqCst) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        // Pre-flight checks before sending
        if self.errored.load(Ordering::SeqCst) {
            return Err(self.get_stored_error());
        }

        if self.closed.load(Ordering::SeqCst) {
            return Err(StreamError::Closed);
        }

        // Check if backpressure is active - Sink contract says start_send should only
        // be called after poll_ready returns Ready(Ok(()))
        if self.backpressure.load(Ordering::SeqCst) {
            return Err(StreamError::Custom(
                "start_send called while backpressure is active - call poll_ready first".into(),
            ));
        }

        let (tx, _rx) = oneshot::channel();
        self.command_tx
            .unbounded_send(StreamCommand::Write {
                chunk: item,
                completion: tx,
            })
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;

        // For the Sink trait, we return immediately after enqueueing.
        // The actual write completion is handled asynchronously by the stream task.
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Project the pinned fields
        let mut this = self.project();

        // Check for errors
        if this.errored.load(Ordering::SeqCst) {
            let error = {
                let opt_err = match this.stored_error.read() {
                    Ok(guard) => guard.clone(),
                    Err(poisoned) => poisoned.into_inner().clone(),
                };
                opt_err.unwrap_or_else(|| StreamError::Custom("Stream is errored".into()))
            };
            return Poll::Ready(Err(error));
        }

        // If there's no flush_receiver yet, initiate a flush and store the receiver
        if this.flush_receiver.is_none() {
            let (tx, rx) = oneshot::channel();
            if this
                .command_tx
                .unbounded_send(StreamCommand::Flush { completion: tx })
                .is_err()
            {
                return Poll::Ready(Err(StreamError::Custom("Stream task dropped".into())));
            }

            *this.flush_receiver = Some(rx);
        }

        // Poll the flush_receiver
        if let Some(rx) = this.flush_receiver.as_mut().as_pin_mut() {
            match rx.poll(cx) {
                Poll::Ready(Ok(result)) => {
                    *this.flush_receiver = None;
                    Poll::Ready(result)
                }
                Poll::Ready(Err(_)) => {
                    *this.flush_receiver = None;
                    Poll::Ready(Err(StreamError::Custom("Flush operation canceled".into())))
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(Err(StreamError::Custom("Flush receiver missing".into())))
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Project the pinned fields
        let mut this = self.project();

        // Already closed?
        if this.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }

        // If errored, return error
        if this.errored.load(Ordering::SeqCst) {
            let error = {
                let opt_err = match this.stored_error.read() {
                    Ok(guard) => guard.clone(),
                    Err(poisoned) => poisoned.into_inner().clone(),
                };
                opt_err.unwrap_or_else(|| StreamError::Custom("Stream is errored".into()))
            };
            return Poll::Ready(Err(error));
        }

        // Initiate close if not already started
        if this.close_receiver.is_none() {
            let (tx, rx) = oneshot::channel();
            if this
                .command_tx
                .unbounded_send(StreamCommand::Close { completion: tx })
                .is_err()
            {
                return Poll::Ready(Err(StreamError::Custom("Stream task dropped".into())));
            }
            *this.close_receiver = Some(rx);
        }

        // Poll the close_receiver future
        if let Some(rx) = this.close_receiver.as_mut().as_pin_mut() {
            match rx.poll(cx) {
                Poll::Ready(Ok(result)) => {
                    *this.close_receiver = None;
                    Poll::Ready(result)
                }
                Poll::Ready(Err(_)) => {
                    *this.close_receiver = None;
                    Poll::Ready(Err(StreamError::Custom("Close operation canceled".into())))
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(Err(StreamError::Custom("Close receiver missing".into())))
        }
    }
}

impl<T, Sink> AsyncWrite for WritableStream<T, Sink, Unlocked>
where
    T: for<'a> From<&'a [u8]> + Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, IoError>> {
        let mut this = self.project();

        // Early return for empty writes
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // Check error state
        if this.errored.load(Ordering::SeqCst) {
            let error_msg = this
                .stored_error
                .read()
                .ok()
                .and_then(|guard| guard.as_ref().cloned())
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Stream is errored".into());
            return Poll::Ready(Err(IoError::new(ErrorKind::Other, error_msg)));
        }

        // Check closed state
        if this.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Err(IoError::new(ErrorKind::BrokenPipe, "Stream is closed")));
        }

        // Backpressure check
        if this.backpressure.load(Ordering::SeqCst) {
            let waker = cx.waker().clone();
            let _ = this
                .command_tx
                .unbounded_send(StreamCommand::RegisterReadyWaker { waker });

            if this.backpressure.load(Ordering::SeqCst) {
                return Poll::Pending;
            }
        }

        // If no write in progress, start one
        if this.write_receiver.is_none() {
            let chunk: T = T::from(buf);
            let (tx, rx) = oneshot::channel();

            if this
                .command_tx
                .unbounded_send(StreamCommand::Write {
                    chunk,
                    completion: tx,
                })
                .is_err()
            {
                return Poll::Ready(Err(IoError::new(
                    ErrorKind::BrokenPipe,
                    "Stream task dropped",
                )));
            }

            this.write_receiver.set(Some(rx));
            *this.pending_write_len = Some(buf.len());
        }

        // Poll the stored write receiver
        if let Some(rx) = this.write_receiver.as_mut().as_pin_mut() {
            match rx.poll(cx) {
                Poll::Ready(Ok(Ok(()))) => {
                    let written = this.pending_write_len.take().unwrap_or(0);
                    this.write_receiver.set(None);
                    Poll::Ready(Ok(written))
                }
                Poll::Ready(Ok(Err(stream_err))) => {
                    this.write_receiver.set(None);
                    this.pending_write_len.take();
                    let io_err = match stream_err {
                        StreamError::Canceled => {
                            IoError::new(ErrorKind::Interrupted, "Write canceled")
                        }
                        StreamError::Aborted(_) => {
                            IoError::new(ErrorKind::Interrupted, stream_err.to_string())
                        }
                        StreamError::Closing => {
                            IoError::new(ErrorKind::BrokenPipe, "Stream is closing")
                        }
                        StreamError::Closed => {
                            IoError::new(ErrorKind::BrokenPipe, "Stream is closed")
                        }
                        StreamError::Custom(_) => {
                            IoError::new(ErrorKind::Other, stream_err.to_string())
                        }
                    };
                    Poll::Ready(Err(io_err))
                }
                Poll::Ready(Err(_)) => {
                    this.write_receiver.set(None);
                    this.pending_write_len.take();
                    Poll::Ready(Err(IoError::new(
                        ErrorKind::Interrupted,
                        "Write completion channel canceled",
                    )))
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(Err(IoError::new(
                ErrorKind::Other,
                "Write receiver missing",
            )))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        let mut this = self.project();

        if this.errored.load(Ordering::SeqCst) {
            let error_msg = this
                .stored_error
                .read()
                .ok()
                .and_then(|guard| guard.as_ref().cloned())
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Stream is errored".into());
            return Poll::Ready(Err(IoError::new(ErrorKind::Other, error_msg)));
        }

        // Create flush future if needed
        if this.flush_receiver.is_none() {
            let (tx, rx) = oneshot::channel();
            if this
                .command_tx
                .unbounded_send(StreamCommand::Flush { completion: tx })
                .is_err()
            {
                return Poll::Ready(Err(IoError::new(
                    ErrorKind::BrokenPipe,
                    "Stream task dropped",
                )));
            }
            *this.flush_receiver = Some(rx);
        }

        // Poll the stored flush receiver
        if let Some(rx) = this.flush_receiver.as_mut().as_pin_mut() {
            match rx.poll(cx) {
                Poll::Ready(Ok(result)) => {
                    this.flush_receiver.set(None); // clear the receiver
                    match result {
                        Ok(()) => Poll::Ready(Ok(())),
                        Err(e) => {
                            Poll::Ready(Err(IoError::new(ErrorKind::Other, format!("{}", e))))
                        }
                    }
                }
                Poll::Ready(Err(_)) => {
                    this.flush_receiver.set(None);
                    Poll::Ready(Err(IoError::new(
                        ErrorKind::Other,
                        "Flush operation canceled",
                    )))
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(Err(IoError::new(
                ErrorKind::Other,
                "Flush receiver missing",
            )))
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        let mut this = self.project();

        // If already closed, return success
        if this.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(()));
        }

        // If errored, return the error
        if this.errored.load(Ordering::SeqCst) {
            let error_msg = this
                .stored_error
                .read()
                .ok()
                .and_then(|guard| guard.as_ref().cloned())
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Stream is errored".into());
            return Poll::Ready(Err(IoError::new(ErrorKind::Other, error_msg)));
        }

        // Create the close receiver if not already created
        if this.close_receiver.is_none() {
            let (tx, rx) = oneshot::channel();
            if this
                .command_tx
                .unbounded_send(StreamCommand::Close { completion: tx })
                .is_err()
            {
                return Poll::Ready(Err(IoError::new(
                    ErrorKind::BrokenPipe,
                    "Stream task dropped",
                )));
            }
            this.close_receiver.set(Some(rx));
        }

        // Poll the stored close receiver
        if let Some(rx) = this.close_receiver.as_mut().as_pin_mut() {
            match rx.poll(cx) {
                Poll::Ready(Ok(result)) => {
                    this.close_receiver.set(None);
                    match result {
                        Ok(()) => Poll::Ready(Ok(())),
                        Err(e) => {
                            Poll::Ready(Err(IoError::new(ErrorKind::Other, format!("{}", e))))
                        }
                    }
                }
                Poll::Ready(Err(_)) => {
                    this.close_receiver.set(None);
                    Poll::Ready(Err(IoError::new(
                        ErrorKind::Other,
                        "Close operation canceled",
                    )))
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            // Should never happen
            Poll::Ready(Err(IoError::new(
                ErrorKind::Other,
                "Close receiver missing",
            )))
        }
    }
}

pub trait WritableSink<T: Send + 'static>: Sized {
    /// Start the sink
    fn start(
        &mut self,
        controller: &mut WritableStreamDefaultController,
    ) -> impl Future<Output = StreamResult<()>> + Send {
        let _ = controller;
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
    fn abort(&mut self, reason: Option<String>) -> impl Future<Output = StreamResult<()>> + Send {
        let _ = reason;
        future::ready(Ok(())) // default no-op
    }
}

// Helper to process each command. Break out to keep task flat.
fn process_command<T, Sink>(
    cmd: StreamCommand<T>,
    inner: &mut WritableStreamInner<T, Sink>,
    backpressure: &Arc<AtomicBool>,
    closed: &Arc<AtomicBool>,
    errored: &Arc<AtomicBool>,
    queue_total_size: &Arc<AtomicUsize>,
    _cx: &mut Context<'_>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    match cmd {
        StreamCommand::Write { chunk, completion } => {
            if inner.state == StreamState::Errored {
                let _ = completion.send(Err(inner.get_stored_error()));
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
            update_atomic_counters(inner, queue_total_size);
            update_flags(&inner, backpressure, closed, errored);
        }
        StreamCommand::Close { completion } => {
            if inner.state == StreamState::Errored {
                let _ = completion.send(Err(inner.get_stored_error()));
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
            update_atomic_counters(&inner, &queue_total_size);
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
            update_atomic_counters(&inner, &queue_total_size);
            update_flags(&inner, backpressure, closed, errored);
        }
        StreamCommand::Flush { completion } => {
            if inner.state == StreamState::Errored {
                let _ = completion.send(Err(inner.get_stored_error()));
                return;
            }

            inner.pending_flush_commands.push(completion);
        }
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
        }
    }
}

// Inflight operations being driven
enum InFlight<Sink> {
    Write {
        fut: Pin<Box<dyn Future<Output = (Sink, StreamResult<()>)> + Send>>,
        completion: Option<oneshot::Sender<StreamResult<()>>>,
        chunk_size: usize,
        abort_handle: Option<AbortHandle>,
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
    mut controller: WritableStreamDefaultController,
    mut ctrl_rx: UnboundedReceiver<ControllerMsg>,
) where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
    let mut inflight: Option<InFlight<Sink>> = None;

    if let Some(mut sink) = inner.sink.take() {
        let start_result = sink.start(&mut controller).await;

        match start_result {
            Ok(()) => {
                // Success: restore the sink to inner
                inner.sink = Some(sink);
            }
            Err(error) => {
                // Failed to start: mark errored state
                inner.state = StreamState::Errored;
                inner.set_stored_error(error);
                inner.sink = None; // Drop the sink on failure
                // Optionally you may want to wake closed and ready wakers here
                update_flags(&inner, &backpressure, &closed, &errored);
                // Since start failed, allow the loop to run and reject commands naturally as they would check for the error state.
            }
        }
    }

    poll_fn(|cx| {
        process_controller_msgs(&mut inner, &mut ctrl_rx);
        update_atomic_counters(&inner, &queue_total_size);
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
                    process_command(
                        cmd,
                        &mut inner,
                        &backpressure,
                        &closed,
                        &errored,
                        &queue_total_size,
                        cx,
                    );
                    while let Some(completion) = inner.pending_flush_commands.pop() {
                        process_flush_command(completion, &mut inner, &inflight);
                    }
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => break,
            }
        }

        // cancel inflight writes/closes if abort requested but not yet started
        if inner.abort_requested {
            eprintln!("Stream task: abort requested, processing...");
            controller.request_abort(); // Signal to any running writes
            // Cancel inflight write or close operations immediately
            if let Some(inflight_op) = &mut inflight {
                eprintln!("Stream task: cancelling inflight operation");
                match inflight_op {
                    InFlight::Write { completion, .. } => {
                        if let Some(sender) = completion.take() {
                            //let _ = sender.send(Err(StreamError::Custom("Stream aborted".into())));
                            let _ = sender.send(Err(StreamError::Aborted(None)));
                        }
                    }
                    InFlight::Close { completions, .. } => {
                        for sender in completions.drain(..) {
                            //let _ = sender.send(Err(StreamError::Custom("Stream aborted".into())));
                            let _ = sender.send(Err(StreamError::Aborted(None)));
                        }
                    }
                    _ => {}
                }
            }
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
                if let Some(mut sink) = inner.sink.take() {
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
                inner.update_backpressure();
                update_atomic_counters(&inner, &queue_total_size);
                update_flags(&inner, &backpressure, &closed, &errored);

                if let Some(mut sink) = inner.sink.take() {
                    let mut ctrl = controller.clone();
                    let chunk = pw.chunk;
                    let completion = pw.completion_tx;

                    inflight = Some(InFlight::Write {
                        fut: Box::pin(async move {
                            let result = sink.write(chunk, &mut ctrl).await;
                            (sink, result)
                        }),
                        completion: Some(completion),
                        chunk_size,
                        abort_handle: None, // Remove this entirely
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
                    fut, completion, ..
                } => {
                    match fut.as_mut().poll(cx) {
                        Poll::Ready((mut sink, result)) => {
                            decrement_flush_counters(&mut inner);

                            if inner.abort_requested {
                                // Transition to abort with recovered sink
                                let reason = inner.abort_reason.take();
                                let abort_fut = async move { sink.abort(reason).await }.boxed();
                                let completions = std::mem::take(&mut inner.abort_completions);

                                // Notify write completion with abort error
                                if let Some(sender) = completion.take() {
                                    let _ = sender.send(Err(StreamError::Aborted(None)));
                                }

                                inflight = Some(InFlight::Abort {
                                    fut: abort_fut,
                                    completions,
                                });
                            } else {
                                // Normal case: restore sink
                                if result.is_err() {
                                    if let Err(e) = result.clone() {
                                        inner.set_stored_error(e);
                                    }
                                    inner.state = StreamState::Errored;
                                    // don’t restore sink on error
                                } else {
                                    inner.sink = Some(sink);
                                }

                                inner.update_backpressure();
                                update_atomic_counters(&inner, &queue_total_size);
                                update_flags(&inner, &backpressure, &closed, &errored);

                                if let Some(sender) = completion.take() {
                                    let _ = sender.send(result);
                                }

                                inflight = None;
                                cx.waker().wake_by_ref();
                            }
                        }
                        Poll::Pending => {}
                    }
                }
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
                                inner.set_stored_error(err.clone());
                            }
                        }

                        inner.close_requested = false;
                        // Clear queue and reset backpressure
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        inner.backpressure = false;

                        update_atomic_counters(&inner, &queue_total_size);
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
                    Poll::Ready(sink_abort_result) => {
                        let notify_result = match sink_abort_result {
                            Ok(()) => Ok(()),
                            Err(sink_error) => {
                                // Sink abort failed - error the stream with this error (sink error)
                                inner.set_stored_error(sink_error.clone());
                                Err(sink_error)
                            }
                        };

                        inner.abort_requested = false;
                        // Clear remaining state
                        // Queue should already be empty (it was cleared when abort was requested)
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        inner.backpressure = false;

                        update_atomic_counters(&inner, &queue_total_size);
                        update_flags(&inner, &backpressure, &closed, &errored);

                        for sender in completions.drain(..) {
                            let _ = sender.send(notify_result.clone());
                        }

                        inflight = None;
                        cx.waker().wake_by_ref();

                        //None
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
    /// Calling `write()` repeatedly *without* awaiting or *without* awaiting `ready()` (i.e without
    /// respecting backpressure) can cause unbounded growth of the internal queue,
    /// leading to increased memory usage and potential performance degradation.
    ///
    /// To avoid excessive buffering, it is recommended to either:
    /// - Await each `write()` call or
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
) {
    queue_total_size.store(inner.queue_total_size, Ordering::SeqCst);
}

impl<T, Sink> WritableStreamDefaultWriter<T, Sink>
where
    T: Send + 'static,
    Sink: WritableSink<T> + Send + 'static,
{
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

    /// Track flush operations waiting for specific write counts
    flush_completions: Vec<(oneshot::Sender<StreamResult<()>>, usize)>,
    pending_flush_commands: Vec<oneshot::Sender<StreamResult<()>>>,
}

impl<T, Sink> WritableStreamInner<T, Sink> {
    /// Update the stream's backpressure flag to reflect the current load.
    fn update_backpressure(&mut self) {
        let prev = self.backpressure;
        self.backpressure = self.queue_total_size >= self.strategy.high_water_mark();
        if prev && !self.backpressure {
            self.ready_wakers.wake_all();
        }
    }

    fn get_stored_error(&self) -> StreamError {
        self.stored_error
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| StreamError::Custom("Stream is errored".into()))
    }

    fn set_stored_error(&self, err: StreamError) {
        let mut guard = match self.stored_error.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(err);
    }
}

// Stream task processing for flush - count writes at this moment!
fn process_flush_command<T, Sink>(
    completion: oneshot::Sender<StreamResult<()>>,
    inner: &mut WritableStreamInner<T, Sink>,
    inflight: &Option<InFlight<Sink>>,
) {
    if inner.state == StreamState::Errored {
        let _ = completion.send(Err(inner.get_stored_error()));
        return;
    }

    // Count writes that exist RIGHT NOW when flush is called
    //let writes_to_wait_for = inner.queue.len() + if inflight.is_some() { 1 } else { 0 };
    let inflight_writes = match inflight {
        Some(InFlight::Write { .. }) => 1,
        _ => 0,
    };
    let writes_to_wait_for = inner.queue.len() + inflight_writes;

    if writes_to_wait_for == 0 {
        // No writes to wait for - flush complete immediately!
        let _ = completion.send(Ok(()));
    } else {
        // Queue this flush to wait for exactly this many write completions
        inner
            .flush_completions
            .push((completion, writes_to_wait_for));
    }
}

// When ANY write completes, decrement ALL pending flush counters
fn decrement_flush_counters<T, Sink>(inner: &mut WritableStreamInner<T, Sink>) {
    let mut i = 0;
    while i < inner.flush_completions.len() {
        let (_, count) = &mut inner.flush_completions[i];
        *count -= 1;

        if *count == 0 {
            // This flush is complete!
            let (sender, _) = inner.flush_completions.swap_remove(i);
            let _ = sender.send(Ok(()));
            // Don't increment i since we removed an element
        } else {
            i += 1;
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

#[derive(Clone)]
pub struct WritableStreamDefaultController {
    tx: UnboundedSender<ControllerMsg>,
    //abort_reg: Arc<AbortRegistration>,
    abort_requested: Arc<AtomicBool>,
    abort_waker: Arc<AtomicWaker>,
}

impl WritableStreamDefaultController {
    /*pub fn new(sender: UnboundedSender<ControllerMsg>, abort_reg: Arc<AbortRegistration>) -> Self {
        Self { tx: sender,abort_reg }
    }*/
    pub fn new(sender: UnboundedSender<ControllerMsg>) -> Self {
        Self {
            tx: sender,
            abort_requested: Arc::new(AtomicBool::new(false)),
            abort_waker: Arc::new(AtomicWaker::new()),
        }
    }

    /// Signal an error on the stream
    pub fn error(&self, error: StreamError) {
        // ignore send failure if receiver is dropped
        let _ = self.tx.unbounded_send(ControllerMsg::Error(error));
    }

    /// Expose a reference to the abort registration so sinks can use it
    /*pub fn abort_registration(&self) -> &AbortRegistration {
        &self.abort_reg
    }*/

    /// Returns `true` if the stream has been aborted.
    ///
    /// This is a synchronous check of the abort flag.
    pub fn is_aborted(&self) -> bool {
        self.abort_requested.load(Ordering::SeqCst)
    }

    /// Internal: request that the stream be aborted.
    ///
    /// Sets the abort flag and wakes any futures created by [`abort_future()`].
    fn request_abort(&self) {
        self.abort_requested.store(true, Ordering::SeqCst);
        self.abort_waker.wake();
        //let _ = self.tx.unbounded_send(ControllerMsg::AbortRequested);
    }

    /// Returns a future that resolves once the stream is aborted.
    ///
    /// # Usage
    ///
    /// Sink implementors should `select!` or `tokio::select!` on this future
    /// alongside their actual write work, so they can stop promptly if
    /// the stream aborts:
    ///
    /// ```no_run
    /// async fn write(
    ///     &mut self,
    ///     chunk: Vec<u8>,
    ///     controller: &mut WritableStreamDefaultController,
    /// ) -> StreamResult<()> {
    ///     tokio::select! {
    ///         _ = controller.abort_future() => {
    ///             Err(StreamError::Aborted(None))
    ///         }
    ///         _ = async {
    ///             // do actual I/O
    ///         } => {
    ///             Ok(())
    ///         }
    ///     }
    /// }
    /// ```
    pub fn abort_future(&self) -> impl std::future::Future<Output = ()> {
        let waker = self.abort_waker.clone();
        let flag = self.abort_requested.clone();
        poll_fn(move |cx| {
            if flag.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                // register waker so it will be woken when request_abort() calls wake()
                waker.register(cx.waker());
                Poll::Pending
            }
        })
    }

    /// Races a future against the abort signal.
    ///
    /// If the abort fires first, returns `Err(StreamError::Aborted)`.
    /// Otherwise, returns the result of the future wrapped in `Ok`.
    ///
    /// # Example
    ///
    /// ```rust
    /// async fn write(
    ///     &mut self,
    ///     chunk: Vec<u8>,
    ///     controller: &mut WritableStreamDefaultController,
    /// ) -> StreamResult<()> {
    ///     controller.with_abort(async move {
    ///         // simulate slow write
    ///         tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    ///         Ok::<_, StreamError>(())
    ///     }).await
    /// }
    /// ```
    pub fn with_abort<F, T>(&self, fut: F) -> impl Future<Output = Result<T, StreamError>> + Send
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let abort_fut = self.abort_future();

        // Box::pin makes it Unpin
        let fut = Box::pin(fut);
        let abort_fut = Box::pin(abort_fut);

        futures::future::select(fut, abort_fut).map(|either| match either {
            futures::future::Either::Left((value, _)) => Ok(value),
            futures::future::Either::Right((_unit, _)) => Err(StreamError::Aborted(None)),
        })
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
            return Poll::Ready(Err(self.writer.stream.get_stored_error()));
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
            return Poll::Ready(Err(self.writer.stream.get_stored_error()));
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
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");

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

            fn _is_closed(&self) -> bool {
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

            fn close(self) -> impl std::future::Future<Output = StreamResult<()>> + Send {
                let closed = self.closed.clone();
                async move {
                    *closed.lock().unwrap() = true;
                    Ok(())
                }
            }

            fn abort(
                &mut self,
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

            fn _get_call_count(&self) -> usize {
                *self.calls.lock().unwrap()
            }
        }

        impl WritableSink<Vec<u8>> for SlowSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl Future<Output = StreamResult<()>> + Send {
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
        let ready_fut = writer.ready();
        tokio::pin!(ready_fut);
        use futures::task::{Context, Poll};

        let waker = futures::task::noop_waker_ref();
        let mut cx = Context::from_waker(waker);

        match ready_fut.as_mut().poll(&mut cx) {
            Poll::Pending => {} // Expected, since backpressure is active
            Poll::Ready(_) => panic!("ready() resolved early despite backpressure"),
        }

        // Now simulate sink unblocking to process pending writes
        unblock_notify.notify_one();

        // Await first write completion
        let _ = write1.await.expect("write1 done");

        // Lower backpressure flag should clear after draining once first write completes;
        // Sink should start draining queued writes now.
        // Notify again to unblock second write
        unblock_notify.notify_one();

        // Await second write completion
        let _ = write2.await.expect("write2 done");

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

            fn _count(&self) -> usize {
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

        // Use HWM = 1 to trigger backpressure with 2 queued writes
        let (sink, notify) = BlockSink::new();
        let strategy = Box::new(CountQueuingStrategy::new(1usize));
        let stream = WritableStream::new(sink, strategy);
        let (_locked_stream, writer) = stream.get_writer().expect("get_writer");
        let writer = Arc::new(writer);

        // 1st write: dequeued immediately, starts processing in sink
        let writer_clone = writer.clone();
        let write1 = tokio::spawn(async move { writer_clone.write(vec![1]).await });

        // Wait for first write to start processing
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // At this point: queue_size = 0, no backpressure yet
        assert!(
            !stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst),
            "No backpressure yet - queue is empty"
        );

        // 2nd write: gets queued (queue_size = 1, equals HWM)
        let writer_clone = writer.clone();
        let write2 = tokio::spawn(async move { writer_clone.write(vec![2]).await });

        // 3rd write: would exceed HWM, triggers backpressure
        let writer_clone = writer.clone();
        let write3 = tokio::spawn(async move { writer_clone.write(vec![3]).await });

        // Wait for writes to be queued
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Now backpressure should be true (queue_size >= HWM)
        assert!(
            stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst),
            "Backpressure should be active with 2+ items queued when HWM=1"
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

        // Notify sink to unblock and process writes
        notify.notify_waiters(); // Unblock first write
        let _ = write1.await.expect("first write");

        notify.notify_waiters(); // Unblock second write  
        let _ = write2.await.expect("second write");

        notify.notify_waiters(); // Unblock third write
        let _ = write3.await.expect("third write");

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

        let _ = writer.release_lock();
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
        let _ = writer1.release_lock();

        // Now new writer acquisition must succeed
        let (_locked_stream2, writer2) = stream.get_writer().expect("get_writer after release");
        drop(writer2);
    }

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

        let _ = writer.release_lock();
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
                &mut self,
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

            fn _get_write_count(&self) -> usize {
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
        let strategy = Box::new(CountQueuingStrategy::new(1));
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
            //println!("Atomic: {}", stream.backpressure.load(Ordering::SeqCst),);
        }

        // Get ready future: should be pending due to backpressure
        // Poll twice to simulate waker registration behavior
        let ready_fut = writer.ready();
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

#[cfg(test)]
mod sink_integration_tests {
    use super::*;
    use futures::{SinkExt, StreamExt, stream};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::time::timeout;

    // Test sink implementation that tracks all operations
    #[derive(Debug, Clone)]
    struct TestSink {
        id: String,
        received_items: Arc<Mutex<Vec<String>>>,
        write_delay: Option<Duration>,
        fail_on_write: Option<usize>, // Fail on nth write
        fail_on_close: bool,
        fail_on_abort: bool,
        operation_log: Arc<Mutex<Vec<String>>>,
    }

    impl TestSink {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                received_items: Arc::new(Mutex::new(Vec::new())),
                write_delay: None,
                fail_on_write: None,
                fail_on_close: false,
                fail_on_abort: false,
                operation_log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_write_delay(mut self, delay: Duration) -> Self {
            self.write_delay = Some(delay);
            self
        }

        fn with_write_failure(mut self, fail_on_nth: usize) -> Self {
            self.fail_on_write = Some(fail_on_nth);
            self
        }

        fn with_close_failure(mut self) -> Self {
            self.fail_on_close = true;
            self
        }

        fn _with_abort_failure(mut self) -> Self {
            self.fail_on_abort = true;
            self
        }

        fn get_received_items(&self) -> Vec<String> {
            self.received_items.lock().unwrap().clone()
        }

        fn get_operation_log(&self) -> Vec<String> {
            self.operation_log.lock().unwrap().clone()
        }

        fn log_operation(&self, op: &str) {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("{}: {}", self.id, op));
        }
    }

    impl WritableSink<String> for TestSink {
        async fn start(
            &mut self,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.log_operation("start called");
            Ok(())
        }

        async fn write(
            &mut self,
            chunk: String,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.log_operation(&format!("write called with: {}", chunk));

            // Simulate write delay if configured
            if let Some(delay) = self.write_delay {
                tokio::time::sleep(delay).await;
            }

            // Check if we should fail on this write
            let current_count = self.received_items.lock().unwrap().len();
            if let Some(fail_on) = self.fail_on_write {
                if current_count >= fail_on {
                    self.log_operation(&format!("write failed on item {}", current_count + 1));
                    return Err(StreamError::Custom(
                        format!("Intentional write failure on item {}", current_count + 1).into(),
                    ));
                }
            }

            // Store the item
            self.received_items.lock().unwrap().push(chunk.clone());
            self.log_operation(&format!("write completed: {}", chunk));
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            self.log_operation("close called");

            if self.fail_on_close {
                self.log_operation("close failed");
                return Err(StreamError::Custom("Intentional close failure".into()));
            }

            self.log_operation("close completed");
            Ok(())
        }

        async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
            let reason_str = reason.as_deref().unwrap_or("no reason");
            self.log_operation(&format!("abort called with reason: {}", reason_str));

            if self.fail_on_abort {
                self.log_operation("abort failed");
                return Err(StreamError::Custom("Intentional abort failure".into()));
            }

            self.log_operation("abort completed");
            Ok(())
        }
    }

    // Simple queuing strategy for testing
    struct TestQueuingStrategy {
        high_water_mark: usize,
    }

    impl TestQueuingStrategy {
        fn new(high_water_mark: usize) -> Self {
            Self { high_water_mark }
        }
    }

    impl QueuingStrategy<String> for TestQueuingStrategy {
        fn size(&self, _chunk: &String) -> usize {
            1 // Each string counts as size 1
        }

        fn high_water_mark(&self) -> usize {
            self.high_water_mark
        }
    }

    #[tokio::test]
    async fn test_basic_sink_operations() {
        let sink = TestSink::new("basic");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let mut sink_handle = stream;

        // Test poll_ready and start_send
        assert!(
            sink_handle
                .poll_ready_unpin(&mut std::task::Context::from_waker(
                    &futures::task::noop_waker()
                ))
                .is_ready()
        );

        // Send some items
        sink_handle.start_send_unpin("item1".to_string()).unwrap();
        sink_handle.start_send_unpin("item2".to_string()).unwrap();
        sink_handle.start_send_unpin("item3".to_string()).unwrap();

        //println!("flush called from test");
        // Flush to ensure all writes complete
        sink_handle.flush().await.unwrap();
        //println!("flush call from test returns");

        // Verify items were received
        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2", "item3"]);

        // Close the sink
        sink_handle.close().await.unwrap();

        // Verify operations log
        let log = sink.get_operation_log();
        assert!(log.contains(&"basic: start called".to_string()));
        assert!(log.contains(&"basic: write completed: item1".to_string()));
        assert!(log.contains(&"basic: close completed".to_string()));
    }

    #[tokio::test]
    async fn test_backpressure_handling() {
        let sink = TestSink::new("backpressure");
        let strategy = Box::new(TestQueuingStrategy::new(2)); // Small buffer
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let mut sink_handle = stream;

        // Fill up the buffer
        sink_handle.send("item1".to_string()).await.unwrap();
        sink_handle.send("item2".to_string()).await.unwrap();

        // This should trigger backpressure
        let start = std::time::Instant::now();
        sink_handle.send("item3".to_string()).await.unwrap();

        // Verify backpressure caused some delay (items had to be processed)
        assert!(start.elapsed() > Duration::from_millis(1));

        sink_handle.close().await.unwrap();

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2", "item3"]);
    }

    #[tokio::test]
    async fn test_concurrent_writes() {
        use tokio::sync::Mutex;

        let sink = TestSink::new("concurrent").with_write_delay(Duration::from_millis(10));
        let strategy = Box::new(TestQueuingStrategy::new(10));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        /*let mut sink_handle = stream;

        // Send multiple items concurrently
        let mut futures = Vec::new();
        for i in 0..5 {
            let item = format!("concurrent_item_{}", i);
            futures.push(sink_handle.send(item));
        }

        // Wait for all sends to complete
        futures::future::join_all(futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        sink_handle.close().await.unwrap();
        */

        let sink_handle = Arc::new(Mutex::new(stream));

        let mut futures = Vec::new();
        for i in 0..5 {
            let sink_clone = Arc::clone(&sink_handle);
            let item = format!("concurrent_item_{}", i);
            futures.push(tokio::spawn(async move {
                let mut sink = sink_clone.lock().await;
                sink.send(item).await
            }));
        }

        // Wait for all sends to complete
        let results = futures::future::join_all(futures).await;
        for result in results {
            result.unwrap().unwrap();
        }

        sink_handle.lock().await.close().await.unwrap();

        let received = sink.get_received_items();
        assert_eq!(received.len(), 5);
        // Items should all be present (order might vary due to concurrency)
        for i in 0..5 {
            let expected_item = format!("concurrent_item_{}", i);
            assert!(received.contains(&expected_item));
        }
    }

    #[tokio::test]
    async fn test_write_failure_handling() {
        let sink = TestSink::new("write_fail").with_write_failure(2); // Fail on 3rd write (index 2)
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let mut sink_handle = stream;

        // First two writes should succeed
        sink_handle.send("item1".to_string()).await.unwrap();
        sink_handle.send("item2".to_string()).await.unwrap();

        // Third write should fail
        let result = sink_handle.send("item3".to_string()).await;
        assert!(result.is_err());

        // Verify sink is now in error state
        let ready_result = sink_handle.poll_ready_unpin(&mut std::task::Context::from_waker(
            &futures::task::noop_waker(),
        ));
        match ready_result {
            std::task::Poll::Ready(Err(_)) => {} // Expected
            other => panic!("Expected error state, got: {:?}", other),
        }

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2"]);
    }

    #[tokio::test]
    async fn test_close_failure_handling() {
        let sink = TestSink::new("close_fail").with_close_failure();
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let mut sink_handle = stream;

        // Send some items
        sink_handle.send("item1".to_string()).await.unwrap();
        sink_handle.send("item2".to_string()).await.unwrap();

        // Close should fail
        let close_result = sink_handle.close().await;
        assert!(close_result.is_err());

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2"]);
    }

    #[tokio::test]
    async fn test_operations_after_close() {
        let sink = TestSink::new("after_close");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let mut sink_handle = stream;

        // Send an item and close
        sink_handle.send("item1".to_string()).await.unwrap();
        sink_handle.close().await.unwrap();

        // Further operations should fail
        let send_result = sink_handle.send("item2".to_string()).await;
        assert!(send_result.is_err());

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1"]);
    }

    #[tokio::test]
    async fn test_abort_operation() {
        let sink = TestSink::new("abort_test");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // Get a writer to test abort
        let (_locked_stream, writer) = stream.get_writer().unwrap();

        // Send some items
        writer.write("item1".to_string()).await.unwrap();
        writer.write("item2".to_string()).await.unwrap();

        // Abort with reason
        writer
            .abort(Some("Test abort reason".to_string()))
            .await
            .unwrap();

        // Verify abort was called on sink
        let log = sink.get_operation_log();
        assert!(
            log.iter()
                .any(|entry| entry.contains("abort called with reason: Test abort reason"))
        );
        assert!(log.iter().any(|entry| entry.contains("abort completed")));

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2"]);
    }

    #[tokio::test]
    async fn test_multiple_close_calls() {
        let sink = TestSink::new("multi_close");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let mut sink_handle = stream;

        sink_handle.send("item1".to_string()).await.unwrap();

        // Multiple close calls should all succeed
        sink_handle.close().await.unwrap();
        sink_handle.close().await.unwrap();
        sink_handle.close().await.unwrap();

        // Should only see one close in the log
        let log = sink.get_operation_log();
        let close_count = log
            .iter()
            .filter(|entry| entry.contains("close called"))
            .count();
        assert_eq!(close_count, 1);
    }

    #[tokio::test]
    async fn test_flush_behavior() {
        let sink = TestSink::new("flush_test").with_write_delay(Duration::from_millis(50));
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let mut sink_handle = stream;

        // Start several writes without awaiting
        sink_handle.start_send_unpin("item1".to_string()).unwrap();
        sink_handle.start_send_unpin("item2".to_string()).unwrap();
        sink_handle.start_send_unpin("item3".to_string()).unwrap();

        // At this point, items are queued but may not be written yet
        let _received_before_flush = sink.get_received_items();
        // May be empty or partially filled depending on timing

        // Flush should wait for all writes to complete
        let start = std::time::Instant::now();
        sink_handle.flush().await.unwrap();
        let elapsed = start.elapsed();

        // Should have taken at least some time due to write delays
        assert!(elapsed >= Duration::from_millis(30)); // Some buffer for timing

        // All items should now be written
        let received_after_flush = sink.get_received_items();
        assert_eq!(received_after_flush, vec!["item1", "item2", "item3"]);

        sink_handle.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_stream_integration() {
        let sink = TestSink::new("stream_integration");
        let strategy = Box::new(TestQueuingStrategy::new(3));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let mut sink_handle = stream;

        // Create a stream of items to send
        let mut items = stream::iter(vec!["a", "b", "c", "d", "e"])
            .map(|s| Ok::<String, StreamError>(s.to_string()));

        // Use send_all to send the entire stream
        sink_handle.send_all(&mut items).await.unwrap();

        let received = sink.get_received_items();
        assert_eq!(received, vec!["a", "b", "c", "d", "e"]);
    }

    #[tokio::test]
    async fn test_timeout_behavior() {
        let sink = TestSink::new("timeout_test").with_write_delay(Duration::from_secs(2));
        let strategy = Box::new(TestQueuingStrategy::new(1));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let mut sink_handle = stream;

        // This should timeout because write takes 2 seconds
        let result = timeout(
            Duration::from_millis(500),
            sink_handle.send("slow_item".to_string()),
        )
        .await;
        assert!(result.is_err()); // Should timeout

        // Give it time to complete in background
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Item should eventually be received
        let received = sink.get_received_items();
        assert_eq!(received, vec!["slow_item"]);
    }

    // Stress test for robustness
    #[tokio::test]
    async fn test_high_volume_writes() {
        use tokio::sync::Mutex;

        let sink = TestSink::new("high_volume");
        let strategy = Box::new(TestQueuingStrategy::new(10));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        //let mut sink_handle = stream;
        let sink_handle = Arc::new(Mutex::new(stream));

        let item_count = 1000;
        let mut send_futures = Vec::new();

        /*for i in 0..item_count {
            let future = sink_handle.send(format!("item_{}", i));
            send_futures.push(future);
        }*/
        for i in 0..item_count {
            let sink_clone = Arc::clone(&sink_handle);
            let item = format!("item_{}", i);
            send_futures.push(tokio::spawn(async move {
                let mut sink = sink_clone.lock().await;
                sink.send(item).await
            }));
        }

        // Wait for all to complete
        /*let results: Result<Vec<_>, _> = futures::future::join_all(send_futures)
            .await
            .into_iter()
            .collect();
        results.unwrap();*/
        let results = futures::future::join_all(send_futures).await;
        for r in results {
            r.unwrap().unwrap();
        }

        //sink_handle.close().await.unwrap();
        sink_handle.lock().await.close().await.unwrap();

        let received = sink.get_received_items();
        assert_eq!(received.len(), item_count);

        // Verify all items are present
        for i in 0..item_count {
            let expected = format!("item_{}", i);
            assert!(received.contains(&expected), "Missing item: {}", expected);
        }
    }
}

#[cfg(test)]
mod async_write_integration_tests {
    use super::*;
    use futures::io::AsyncWriteExt;
    use std::io::ErrorKind;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::time::timeout;

    // Test sink that converts bytes to strings for easier verification
    #[derive(Debug, Clone)]
    struct BytesSink {
        id: String,
        received_data: Arc<Mutex<Vec<u8>>>,
        write_delay: Option<Duration>,
        fail_on_write: Option<usize>,
        fail_on_close: bool,
        operation_log: Arc<Mutex<Vec<String>>>,
    }

    impl BytesSink {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                received_data: Arc::new(Mutex::new(Vec::new())),
                write_delay: None,
                fail_on_write: None,
                fail_on_close: false,
                operation_log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_write_delay(mut self, delay: Duration) -> Self {
            self.write_delay = Some(delay);
            self
        }

        fn with_write_failure(mut self, fail_on_nth: usize) -> Self {
            self.fail_on_write = Some(fail_on_nth);
            self
        }

        fn _ith_close_failure(mut self) -> Self {
            self.fail_on_close = true;
            self
        }

        fn get_received_data(&self) -> Vec<u8> {
            self.received_data.lock().unwrap().clone()
        }

        fn get_received_string(&self) -> String {
            String::from_utf8_lossy(&self.get_received_data()).to_string()
        }

        fn get_operation_log(&self) -> Vec<String> {
            self.operation_log.lock().unwrap().clone()
        }

        fn log_operation(&self, op: &str) {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("{}: {}", self.id, op));
        }
    }

    // Implement conversion from &[u8] to Vec<u8> for our test
    /*impl From<&[u8]> for Vec<u8> {
        fn from(bytes: &[u8]) -> Self {
            bytes.to_vec()
        }
    }*/

    impl WritableSink<Vec<u8>> for BytesSink {
        async fn start(
            &mut self,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.log_operation("start called");
            Ok(())
        }

        async fn write(
            &mut self,
            chunk: Vec<u8>,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.log_operation(&format!("write called with {} bytes", chunk.len()));

            // Simulate write delay if configured
            if let Some(delay) = self.write_delay {
                tokio::time::sleep(delay).await;
            }

            // Check if we should fail on this write
            let current_writes = self
                .operation_log
                .lock()
                .unwrap()
                .iter()
                .filter(|entry| entry.contains("write called"))
                .count();

            if let Some(fail_on) = self.fail_on_write {
                if current_writes > fail_on {
                    self.log_operation("write failed");
                    return Err(StreamError::Custom(
                        format!("Intentional write failure on write {}", current_writes).into(),
                    ));
                }
            }

            // Store the data
            self.received_data.lock().unwrap().extend_from_slice(&chunk);
            self.log_operation(&format!("write completed: {} bytes", chunk.len()));
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            self.log_operation("close called");
            if self.fail_on_close {
                self.log_operation("close failed");
                return Err(StreamError::Custom("Intentional close failure".into()));
            }
            self.log_operation("close completed");
            Ok(())
        }

        async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
            let reason_str = reason.as_deref().unwrap_or("no reason");
            self.log_operation(&format!("abort called with reason: {}", reason_str));
            self.log_operation("abort completed");
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_basic_async_write_operations() {
        let sink = BytesSink::new("basic");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // Test writing some data
        let data1 = b"Hello, ";
        let data2 = b"World!";

        stream.write_all(data1).await.unwrap();
        stream.write_all(data2).await.unwrap();
        //stream.flush().await.unwrap();
        AsyncWriteExt::flush(&mut stream).await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "Hello, World!");

        stream.close().await.unwrap();

        let log = sink.get_operation_log();
        assert!(log.iter().any(|entry| entry.contains("start called")));
        assert!(log.iter().any(|entry| entry.contains("close completed")));
    }

    #[tokio::test]
    async fn test_empty_writes() {
        let sink = BytesSink::new("empty");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // Empty write should return Ok(0) immediately
        let result = stream.write(&[]).await.unwrap();
        assert_eq!(result, 0);

        // Regular write after empty write
        stream.write_all(b"test").await.unwrap();
        stream.close().await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "test");
    }

    #[tokio::test]
    async fn test_large_writes() {
        let sink = BytesSink::new("large");
        let strategy = Box::new(TestQueuingStrategy::new(10));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // Write large data
        let large_data = vec![b'X'; 10000];
        stream.write_all(&large_data).await.unwrap();
        //stream.flush().await.unwrap();
        AsyncWriteExt::flush(&mut stream).await.unwrap();

        let received = sink.get_received_data();
        assert_eq!(received.len(), 10000);
        assert!(received.iter().all(|&b| b == b'X'));

        stream.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_multiple_small_writes() {
        let sink = BytesSink::new("multi");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // Write multiple small chunks
        for i in 0..10 {
            let data = format!("{}", i);
            stream.write_all(data.as_bytes()).await.unwrap();
        }
        //stream.flush().await.unwrap();
        AsyncWriteExt::flush(&mut stream).await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "0123456789");

        stream.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_write_error_handling() {
        let sink = BytesSink::new("error").with_write_failure(2);
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // First two writes should succeed
        stream.write_all(b"write1").await.unwrap();
        stream.write_all(b"write2").await.unwrap();

        // Third write should fail
        let result = stream.write_all(b"write3").await;
        assert!(result.is_err());

        // Subsequent operations should also fail due to error state
        let result = stream.write_all(b"write4").await;
        assert!(result.is_err());

        let received = sink.get_received_string();
        assert_eq!(received, "write1write2");
    }

    #[tokio::test]
    async fn test_write_after_close() {
        let sink = BytesSink::new("closed");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // Write some data and close
        stream.write_all(b"before_close").await.unwrap();
        stream.close().await.unwrap();

        // Attempt to write after close should fail
        let result = stream.write_all(b"after_close").await;
        assert!(result.is_err());

        if let Err(e) = result {
            assert_eq!(e.kind(), ErrorKind::BrokenPipe);
        }

        let received = sink.get_received_string();
        assert_eq!(received, "before_close");
    }

    #[tokio::test]
    async fn test_backpressure_with_async_write() {
        let sink = BytesSink::new("backpressure").with_write_delay(Duration::from_millis(50));
        let strategy = Box::new(TestQueuingStrategy::new(2)); // Small buffer
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let start = std::time::Instant::now();

        // Write several chunks - should experience backpressure
        for i in 0..5 {
            let data = format!("chunk{}", i);
            stream.write_all(data.as_bytes()).await.unwrap();
        }

        let elapsed = start.elapsed();
        // Should take some time due to write delays and backpressure
        assert!(elapsed >= Duration::from_millis(100));

        stream.close().await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "chunk0chunk1chunk2chunk3chunk4");
    }

    #[tokio::test]
    async fn test_partial_writes() {
        let sink = BytesSink::new("partial");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let data = b"Hello, World!";

        // Write in chunks manually (simulating partial writes)
        let mut written = 0;
        while written < data.len() {
            let chunk_size = std::cmp::min(5, data.len() - written);
            let n = stream
                .write(&data[written..written + chunk_size])
                .await
                .unwrap();
            written += n;
            assert!(n > 0, "Should make progress");
        }

        //stream.flush().await.unwrap();
        AsyncWriteExt::flush(&mut stream).await.unwrap();
        stream.close().await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "Hello, World!");
    }

    #[tokio::test]
    async fn test_flush_waits_for_queued_writes() {
        use futures::io::AsyncWriteExt;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};
        use tokio::sync::Mutex as AsyncMutex;

        // Simple sink with write delay to test flush timing
        #[derive(Debug, Clone)]
        struct FlushTestSink {
            received_data: Arc<Mutex<Vec<u8>>>,
            write_delay: Duration,
        }

        impl FlushTestSink {
            fn new(delay: Duration) -> Self {
                Self {
                    received_data: Arc::new(Mutex::new(Vec::new())),
                    write_delay: delay,
                }
            }

            fn get_received_string(&self) -> String {
                String::from_utf8_lossy(&self.received_data.lock().unwrap()).to_string()
            }
        }

        impl WritableSink<Vec<u8>> for FlushTestSink {
            async fn write(
                &mut self,
                chunk: Vec<u8>,
                _: &mut WritableStreamDefaultController,
            ) -> StreamResult<()> {
                // Simulate slow write
                tokio::time::sleep(self.write_delay).await;
                self.received_data.lock().unwrap().extend_from_slice(&chunk);
                Ok(())
            }
        }

        let sink = FlushTestSink::new(Duration::from_millis(100));
        let strategy = Box::new(TestQueuingStrategy::new(10));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // Wrap in Arc<AsyncMutex<>> so multiple tasks can use it without borrow checker conflicts
        let stream = Arc::new(AsyncMutex::new(stream));

        // Spawn concurrent writes
        let s1 = Arc::clone(&stream);
        let s2 = Arc::clone(&stream);
        let s3 = Arc::clone(&stream);

        tokio::spawn(async move {
            s1.lock().await.write_all(b"first").await.unwrap();
        });
        tokio::spawn(async move {
            s2.lock().await.write_all(b"second").await.unwrap();
        });
        tokio::spawn(async move {
            s3.lock().await.write_all(b"third").await.unwrap();
        });

        // Give writes a moment to start but not complete
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Now flush should wait for all pending writes to complete
        let start = Instant::now();
        AsyncWriteExt::flush(&mut *stream.lock().await)
            .await
            .unwrap();
        let elapsed = start.elapsed();

        // Flush should have taken at least the write delay time
        assert!(
            elapsed >= Duration::from_millis(80),
            "Flush should wait for writes to complete, took {:?}",
            elapsed
        );

        // Close the stream
        stream.lock().await.close().await.unwrap();

        // All data should be received
        assert_eq!(sink.get_received_string(), "firstsecondthird");
    }

    #[tokio::test]
    async fn test_concurrent_async_writes() {
        use tokio::sync::Mutex;

        let sink = BytesSink::new("concurrent");
        let strategy = Box::new(TestQueuingStrategy::new(10));
        let stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let stream = Arc::new(Mutex::new(stream));
        let mut handles = Vec::new();

        // Spawn multiple concurrent write tasks
        for i in 0..5 {
            let stream_clone = Arc::clone(&stream);
            let data = format!("task{}_data ", i);

            handles.push(tokio::spawn(async move {
                let mut stream = stream_clone.lock().await;
                stream.write_all(data.as_bytes()).await.unwrap();
            }));
        }

        // Wait for all writes to complete
        for handle in handles {
            handle.await.unwrap();
        }

        //stream.lock().await.flush().await.unwrap();
        AsyncWriteExt::flush(&mut *stream.lock().await)
            .await
            .unwrap();
        stream.lock().await.close().await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received.len(), 5 * "task0_data ".len());

        // Verify all task data is present
        for i in 0..5 {
            let expected = format!("task{}_data ", i);
            assert!(received.contains(&expected), "Missing data for task {}", i);
        }
    }

    #[tokio::test]
    async fn test_timeout_behavior() {
        let sink = BytesSink::new("timeout").with_write_delay(Duration::from_secs(2));
        let strategy = Box::new(TestQueuingStrategy::new(1));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // This should timeout because write takes 2 seconds
        let result = timeout(Duration::from_millis(500), stream.write_all(b"slow_data")).await;

        assert!(result.is_err()); // Should timeout

        // Give it time to complete in background
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Data should eventually be received
        let received = sink.get_received_string();
        assert_eq!(received, "slow_data");
    }

    #[tokio::test]
    async fn test_io_error_kinds() {
        // Test different error conditions produce appropriate IoError kinds

        // Test closed stream error
        {
            let sink = BytesSink::new("closed_error");
            let strategy = Box::new(TestQueuingStrategy::new(5));
            let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
                tokio::spawn(fut);
            });

            stream.close().await.unwrap();

            let result = stream.write_all(b"data").await;
            assert!(result.is_err());
            if let Err(e) = result {
                assert_eq!(e.kind(), ErrorKind::BrokenPipe);
            }
        }

        // Test error state
        {
            let sink = BytesSink::new("error_state").with_write_failure(0);
            let strategy = Box::new(TestQueuingStrategy::new(5));
            let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
                tokio::spawn(fut);
            });

            // First write should fail and put stream in error state
            let _ = stream.write_all(b"data").await;

            let result = stream.write_all(b"more_data").await;
            assert!(result.is_err());
            if let Err(e) = result {
                assert_eq!(e.kind(), ErrorKind::Other);
            }
        }
    }

    #[tokio::test]
    async fn test_binary_data() {
        let sink = BytesSink::new("binary");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        // Write binary data with null bytes and high values
        let binary_data = vec![0u8, 1, 255, 128, 0, 42, 255];
        stream.write_all(&binary_data).await.unwrap();
        stream.close().await.unwrap();

        let received = sink.get_received_data();
        assert_eq!(received, binary_data);
    }

    #[tokio::test]
    async fn test_write_all_vs_write() {
        let sink = BytesSink::new("write_comparison");
        let strategy = Box::new(TestQueuingStrategy::new(5));
        let mut stream = WritableStream::new_with_spawn(sink.clone(), strategy, |fut| {
            tokio::spawn(fut);
        });

        let data = b"Hello, World!";

        // Test individual write calls
        let mut pos = 0;
        while pos < data.len() {
            let n = stream.write(&data[pos..]).await.unwrap();
            assert!(n > 0);
            pos += n;
        }

        // Add some more data with write_all
        stream.write_all(b" Extra data").await.unwrap();
        stream.close().await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "Hello, World! Extra data");
    }

    // Helper struct for testing (reusing from sink tests)
    struct TestQueuingStrategy {
        high_water_mark: usize,
    }

    impl TestQueuingStrategy {
        fn new(high_water_mark: usize) -> Self {
            Self { high_water_mark }
        }
    }

    impl<T> QueuingStrategy<T> for TestQueuingStrategy {
        fn size(&self, _chunk: &T) -> usize {
            1
        }

        fn high_water_mark(&self) -> usize {
            self.high_water_mark
        }
    }
}
