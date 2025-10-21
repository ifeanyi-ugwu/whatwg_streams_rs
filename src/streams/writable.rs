use super::super::{CountQueuingStrategy, Locked, QueuingStrategy, Unlocked};
use super::error::StreamError;
use crate::platform::{MaybeSend, SharedPtr};
use futures::FutureExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::channel::oneshot;
use futures::future::poll_fn;
use futures::{AsyncWrite, SinkExt, StreamExt, future};
use futures::{channel::mpsc::UnboundedReceiver, task::AtomicWaker};
use pin_project::pin_project;
use std::collections::VecDeque;
use std::future::Future;
use std::io::{Error as IoError, ErrorKind};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::task::{Context, Poll, Waker};

type StreamResult<T> = Result<T, StreamError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
    Writable,
    Closed,
    Errored,
}

struct PendingWrite<T> {
    chunk: T,
    completion_tx: oneshot::Sender<StreamResult<()>>,
}

/// Commands sent to stream task for state mutation
enum StreamCommand<T> {
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

#[pin_project]
pub struct WritableStream<T: MaybeSend + 'static, Sink, S = Unlocked> {
    command_tx: UnboundedSender<StreamCommand<T>>,
    backpressure: SharedPtr<AtomicBool>,
    closed: SharedPtr<AtomicBool>,
    errored: SharedPtr<AtomicBool>,
    locked: SharedPtr<AtomicBool>,
    queue_total_size: SharedPtr<AtomicUsize>,
    high_water_mark: SharedPtr<AtomicUsize>,
    stored_error: SharedPtr<RwLock<Option<StreamError>>>,
    _sink: PhantomData<Sink>,
    _state: PhantomData<S>,
    #[pin]
    flush_receiver: Option<oneshot::Receiver<StreamResult<()>>>,
    #[pin]
    close_receiver: Option<oneshot::Receiver<Result<(), StreamError>>>,
    #[pin]
    write_receiver: Option<oneshot::Receiver<Result<(), StreamError>>>,
    pending_write_len: Option<usize>,
    pub(crate) controller: SharedPtr<WritableStreamDefaultController>,
}

impl<T: MaybeSend, Sink, S> WritableStream<T, Sink, S> {
    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }

    fn get_stored_error(&self) -> StreamError {
        self.stored_error
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| "Stream is errored".into())
    }
}

impl<T, Sink> WritableStream<T, Sink, Unlocked>
where
    T: MaybeSend + 'static,
    Sink: WritableSink<T> + 'static,
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
            .map_err(|_| StreamError::TaskDropped)?;

        // Await the completion of the abort operation
        rx.await.unwrap_or_else(|_| Err(StreamError::TaskDropped))
    }

    pub async fn close(&self) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.command_tx
            .clone()
            .send(StreamCommand::Close { completion: tx })
            .await
            .map_err(|_| StreamError::TaskDropped)?;

        rx.await.unwrap_or_else(|_| Err(StreamError::TaskDropped))
    }
}

impl<T, Sink> WritableStream<T, Sink, Unlocked>
where
    T: MaybeSend + 'static,
    Sink: WritableSink<T> + 'static,
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
            return Err("Stream already locked".into());
        }

        let locked = WritableStream {
            command_tx: self.command_tx.clone(),
            backpressure: SharedPtr::clone(&self.backpressure),
            closed: SharedPtr::clone(&self.closed),
            errored: SharedPtr::clone(&self.errored),
            locked: SharedPtr::clone(&self.locked),
            queue_total_size: SharedPtr::clone(&self.queue_total_size),
            high_water_mark: SharedPtr::clone(&self.high_water_mark),
            stored_error: SharedPtr::clone(&self.stored_error),
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

impl<T: MaybeSend + 'static, Sink> WritableStream<T, Sink>
where
    T: 'static,
    Sink: WritableSink<T> + 'static,
{
    /// Common constructor logic shared between spawn variants
    pub(crate) fn new_inner(
        sink: Sink,
        strategy: crate::platform::BoxedStrategy<T>,
    ) -> (Self, impl Future<Output = ()>) {
        let (command_tx, command_rx) = futures::channel::mpsc::unbounded();
        let high_water_mark = SharedPtr::new(AtomicUsize::new(strategy.high_water_mark()));
        let stored_error = SharedPtr::new(RwLock::new(None));

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
            stored_error: SharedPtr::clone(&stored_error),
            ready_wakers: WakerSet::new(),
            closed_wakers: WakerSet::new(),
            flush_completions: Vec::new(),
            pending_flush_commands: Vec::new(),
        };

        let backpressure = SharedPtr::new(AtomicBool::new(false));
        let closed = SharedPtr::new(AtomicBool::new(false));
        let errored = SharedPtr::new(AtomicBool::new(false));
        let locked = SharedPtr::new(AtomicBool::new(false));
        let queue_total_size = SharedPtr::new(AtomicUsize::new(0));

        let (ctrl_tx, ctrl_rx): (
            UnboundedSender<ControllerMsg>,
            UnboundedReceiver<ControllerMsg>,
        ) = unbounded();
        let controller = WritableStreamDefaultController::new(ctrl_tx.clone());

        let fut = stream_task(
            command_rx,
            inner,
            SharedPtr::clone(&backpressure),
            SharedPtr::clone(&closed),
            SharedPtr::clone(&errored),
            SharedPtr::clone(&queue_total_size),
            controller.clone(),
            ctrl_rx,
        );

        let stream = Self {
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
        };

        (stream, fut)
    }
}

impl<T, Sink, S> WritableStream<T, Sink, S>
where
    T: MaybeSend + 'static,
    Sink: WritableSink<T> + 'static,
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

impl<T: MaybeSend + 'static, Sink> Clone for WritableStream<T, Sink, Locked> {
    fn clone(&self) -> Self {
        Self {
            command_tx: self.command_tx.clone(),
            backpressure: SharedPtr::clone(&self.backpressure),
            closed: SharedPtr::clone(&self.closed),
            errored: SharedPtr::clone(&self.errored),
            locked: SharedPtr::clone(&self.locked),
            queue_total_size: SharedPtr::clone(&self.queue_total_size),
            high_water_mark: SharedPtr::clone(&self.high_water_mark),
            stored_error: SharedPtr::clone(&self.stored_error),
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
    T: MaybeSend + 'static,
    SinkType: WritableSink<T> + 'static,
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
            return Err(
                "start_send called while backpressure is active - call poll_ready first".into(),
            );
        }

        let (tx, _rx) = oneshot::channel();
        self.command_tx
            .unbounded_send(StreamCommand::Write {
                chunk: item,
                completion: tx,
            })
            .map_err(|_| StreamError::TaskDropped)?;

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
                opt_err.unwrap_or_else(|| "Stream is errored".into())
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
                return Poll::Ready(Err("Stream task dropped".into()));
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
                    Poll::Ready(Err("Flush operation canceled".into()))
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(Err("Flush receiver missing".into()))
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
                opt_err.unwrap_or_else(|| "Stream is errored".into())
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
                return Poll::Ready(Err("Stream task dropped".into()));
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
                    Poll::Ready(Err("Close operation canceled".into()))
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(Err("Close receiver missing".into()))
        }
    }
}

impl<T, Sink> AsyncWrite for WritableStream<T, Sink, Unlocked>
where
    T: for<'a> From<&'a [u8]> + MaybeSend + 'static,
    Sink: WritableSink<T> + 'static,
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
                        StreamError::TaskDropped => {
                            IoError::new(ErrorKind::BrokenPipe, "Stream task dropped")
                        }
                        StreamError::Other(_) => {
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

pub trait WritableSink<T: MaybeSend + 'static>: MaybeSend + Sized + 'static {
    /// Start the sink
    fn start(
        &mut self,
        controller: &mut WritableStreamDefaultController,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend {
        let _ = controller;
        future::ready(Ok(())) // default no-op
    }

    /// Write a chunk to the sink
    fn write(
        &mut self,
        chunk: T,
        controller: &mut WritableStreamDefaultController,
    ) -> impl std::future::Future<Output = StreamResult<()>> + MaybeSend;

    /// Close the sink
    fn close(self) -> impl Future<Output = StreamResult<()>> + MaybeSend {
        future::ready(Ok(())) // default no-op
    }

    /// Abort the sink
    fn abort(
        &mut self,
        reason: Option<String>,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend {
        let _ = reason;
        future::ready(Ok(())) // default no-op
    }
}

// Helper to process each command. Break out to keep task flat.
fn process_command<T, Sink>(
    cmd: StreamCommand<T>,
    inner: &mut WritableStreamInner<T, Sink>,
    backpressure: &SharedPtr<AtomicBool>,
    closed: &SharedPtr<AtomicBool>,
    errored: &SharedPtr<AtomicBool>,
    queue_total_size: &SharedPtr<AtomicUsize>,
    _cx: &mut Context<'_>,
) where
    T: MaybeSend + 'static,
    Sink: WritableSink<T> + 'static,
{
    match cmd {
        StreamCommand::Write { chunk, completion } => {
            if inner.state == StreamState::Errored {
                let _ = completion.send(Err(inner.get_stored_error()));
                return;
            }
            if inner.state == StreamState::Closed {
                //let _ = completion.send(Err(StreamError::Other("Stream is closed".into())));
                let _ = completion.send(Err(StreamError::Closed));
                return;
            }
            // NEW: Also reject writes if close has been requested (stream is closing)
            // This matches the spec: "During this time any further attempts to write will fail"
            if inner.close_requested {
                /*let _ = completion.send(Err(StreamError::Other(
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
        fut: crate::platform::PlatformBoxFutureStatic<(Sink, StreamResult<()>)>,
        completion: Option<oneshot::Sender<StreamResult<()>>>,
    },
    Close {
        fut: crate::platform::PlatformBoxFutureStatic<StreamResult<()>>,
        completions: Vec<oneshot::Sender<StreamResult<()>>>,
    },
    Abort {
        fut: crate::platform::PlatformBoxFutureStatic<StreamResult<()>>,
        completions: Vec<oneshot::Sender<StreamResult<()>>>,
    },
}

async fn stream_task<T, Sink>(
    mut command_rx: UnboundedReceiver<StreamCommand<T>>,
    mut inner: WritableStreamInner<T, Sink>,
    backpressure: SharedPtr<AtomicBool>,
    closed: SharedPtr<AtomicBool>,
    errored: SharedPtr<AtomicBool>,
    queue_total_size: SharedPtr<AtomicUsize>,
    mut controller: WritableStreamDefaultController,
    mut ctrl_rx: UnboundedReceiver<ControllerMsg>,
) where
    T: MaybeSend + 'static,
    Sink: WritableSink<T> + 'static,
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
                            //let _ = sender.send(Err(StreamError::Other("Stream aborted".into())));
                            let _ = sender.send(Err(StreamError::Aborted(None)));
                        }
                    }
                    InFlight::Close { completions, .. } => {
                        for sender in completions.drain(..) {
                            //let _ = sender.send(Err(StreamError::Other("Stream aborted".into())));
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
                        let fut = Box::pin(async move { sink.close().await });
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
                    let fut = Box::pin(async move { sink.abort(reason).await });
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
                    });
                } else {
                    let _ = pw.completion_tx.send(Err("Sink missing".into()));
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
                                let abort_fut = Box::pin(async move { sink.abort(reason).await });
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

pub struct WritableStreamDefaultWriter<T: MaybeSend + 'static, Sink> {
    stream: WritableStream<T, Sink, Locked>,
}

impl<T: MaybeSend + 'static, Sink> WritableStreamDefaultWriter<T, Sink>
where
    T: 'static,
    Sink: WritableSink<T> + 'static,
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
    pub fn write(&self, chunk: T) -> impl std::future::Future<Output = StreamResult<()>> {
        let (tx, rx) = oneshot::channel();

        let enqueue_result = self
            .stream
            .command_tx
            .unbounded_send(StreamCommand::Write {
                chunk,
                completion: tx,
            })
            .map_err(|_| StreamError::TaskDropped);

        // Return a future that handles the completion waiting
        async move {
            // First check if enqueueing failed
            enqueue_result?;

            // Then wait for the write to complete
            rx.await.unwrap_or_else(|_| Err(StreamError::TaskDropped))
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

    /// Immediately enqueue a chunk for writing without waiting for completion.
    ///
    /// This method adds the chunk to the stream's internal write queue and returns
    /// immediately. Unlike [`write()`], it does not wait for the write operation
    /// to complete or return a future for tracking completion.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the chunk was successfully enqueued, or `Err` if
    /// enqueueing fails due to the stream being closed, errored, or the
    /// stream task being dropped.
    ///
    /// # Behavior
    ///
    /// - **Does not wait** for the chunk to be written to the underlying sink
    /// - **Does not provide** completion notification or error handling for the write itself
    /// - **Does not respect backpressure** - chunks are queued regardless of current queue size
    /// - **Fire-and-forget** - suitable for high-throughput scenarios where individual
    ///   write completion tracking isn't needed
    ///
    /// # Memory Considerations
    ///
    /// Since this method doesn't respect backpressure signals, repeated calls without
    /// awaiting [`ready()`] can lead to unbounded queue growth and increased memory usage.
    /// Consider using [`write()`] or [`enqueue_when_ready()`] for backpressure-aware writing.
    ///
    /// # Example
    ///
    /// ```no_run
    /// // Fire-and-forget writing
    /// writer.enqueue(chunk1)?;
    /// writer.enqueue(chunk2)?;
    ///
    /// // Later, ensure all writes complete
    /// writer.close().await?;
    /// ```
    ///
    /// [`write()`]: Self::write
    /// [`ready()`]: Self::ready
    /// [`enqueue_when_ready()`]: Self::enqueue_when_ready
    pub fn enqueue(&self, chunk: T) -> StreamResult<()> {
        /*if self.stream.errored.load(Ordering::SeqCst) {
            return Err(self.stream.get_stored_error());
        }
        if self.stream.closed.load(Ordering::SeqCst) {
            return Err(StreamError::Closed);
        }*/
        if self.stream.errored.load(Ordering::Acquire) {
            return Err(self.stream.get_stored_error());
        }
        if self.stream.closed.load(Ordering::Acquire) {
            return Err(StreamError::Closed);
        }

        let (tx, _rx) = oneshot::channel(); // Drop the receiver since we don't wait
        self.stream
            .command_tx
            .unbounded_send(StreamCommand::Write {
                chunk,
                completion: tx,
            })
            .map_err(|_| StreamError::TaskDropped)
    }

    /// Close the stream asynchronously
    pub async fn close(&self) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.stream
            .command_tx
            .clone()
            .send(StreamCommand::Close { completion: tx })
            .await
            .map_err(|_| StreamError::TaskDropped)?;

        rx.await.unwrap_or_else(|_| Err(StreamError::TaskDropped))
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
            .map_err(|_| StreamError::TaskDropped)?;

        rx.await.unwrap_or_else(|_| Err(StreamError::TaskDropped))
    }

    /// Get the desired size synchronously (how much data the stream can accept)
    /// Returns None if the stream is closed or errored
    pub fn desired_size(&self) -> Option<usize> {
        self.stream.desired_size()
    }

    pub fn ready(&self) -> impl Future<Output = StreamResult<()>> {
        let writer = self.clone();
        poll_fn(move |cx| {
            if writer.stream.errored.load(Ordering::SeqCst) {
                return Poll::Ready(Err(writer.stream.get_stored_error()));
            }
            if writer.stream.closed.load(Ordering::SeqCst) {
                return Poll::Ready(Ok(()));
            }
            if !writer.stream.backpressure.load(Ordering::SeqCst) {
                return Poll::Ready(Ok(()));
            }
            // Not ready, register waker:
            let waker = cx.waker().clone();
            let _ = writer
                .stream
                .command_tx
                .unbounded_send(StreamCommand::RegisterReadyWaker { waker });
            // If the channel is full or busy, that's okay—the next poll will try again.
            // Re-check backpressure after registration
            if !writer.stream.backpressure.load(Ordering::SeqCst) {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        })
    }

    pub fn closed(&self) -> impl Future<Output = StreamResult<()>> {
        let writer = self.clone();
        poll_fn(move |cx| {
            if writer.stream.errored.load(Ordering::SeqCst) {
                return Poll::Ready(Err(writer.stream.get_stored_error()));
            }
            if writer.stream.closed.load(Ordering::SeqCst) {
                return Poll::Ready(Ok(()));
            }
            let waker = cx.waker().clone();
            let _ = writer
                .stream
                .command_tx
                .unbounded_send(StreamCommand::RegisterClosedWaker { waker });
            // Re-check closed after registration
            if writer.stream.closed.load(Ordering::SeqCst) {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        })
    }
}

// Update the stream task to maintain the atomic counters
// this is meant to be called when queue or inflight size is modified before waking wakers i.e before calling `update_flags`
fn update_atomic_counters<T, Sink>(
    inner: &WritableStreamInner<T, Sink>,
    queue_total_size: &SharedPtr<AtomicUsize>,
) {
    queue_total_size.store(inner.queue_total_size, Ordering::SeqCst);
}

impl<T: MaybeSend + 'static, Sink> WritableStreamDefaultWriter<T, Sink>
where
    T: 'static,
    Sink: WritableSink<T> + 'static,
{
    pub fn release_lock(self) -> StreamResult<()> {
        self.stream.locked.store(false, Ordering::SeqCst);

        Ok(())
    }
}

impl<T: MaybeSend + 'static, Sink> Drop for WritableStreamDefaultWriter<T, Sink> {
    fn drop(&mut self) {
        self.stream.locked.store(false, Ordering::SeqCst);
    }
}

impl<T: MaybeSend + 'static, Sink> Clone for WritableStreamDefaultWriter<T, Sink> {
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
        }
    }
}

struct WritableStreamInner<T, Sink> {
    state: StreamState,
    queue: VecDeque<PendingWrite<T>>,
    queue_total_size: usize,
    strategy: crate::platform::BoxedStrategyStatic<T>,
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
    stored_error: SharedPtr<RwLock<Option<StreamError>>>,

    ready_wakers: WakerSet,
    closed_wakers: WakerSet,

    /// Track flush operations waiting for specific write counts
    flush_completions: Vec<(oneshot::Sender<StreamResult<()>>, usize)>,
    pending_flush_commands: Vec<oneshot::Sender<StreamResult<()>>>,
}

impl<T: MaybeSend + 'static, Sink> WritableStreamInner<T, Sink> {
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
            .unwrap_or_else(|| "Stream is errored".into())
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
fn process_flush_command<T: MaybeSend + 'static, Sink>(
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

enum ControllerMsg {
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
    //abort_reg: SharedPtr<AbortRegistration>,
    abort_requested: SharedPtr<AtomicBool>,
    abort_waker: SharedPtr<AtomicWaker>,
}

impl WritableStreamDefaultController {
    /*pub fn new(sender: UnboundedSender<ControllerMsg>, abort_reg: SharedPtr<AbortRegistration>) -> Self {
        Self { tx: sender,abort_reg }
    }*/
    pub fn new(sender: UnboundedSender<ControllerMsg>) -> Self {
        Self {
            tx: sender,
            abort_requested: SharedPtr::new(AtomicBool::new(false)),
            abort_waker: SharedPtr::new(AtomicWaker::new()),
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
    pub fn with_abort<F, T>(&self, fut: F) -> impl Future<Output = Result<T, StreamError>>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
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

/// A lightweight, thread-safe set storing multiple wakers.
/// It ensures wakers are stored without duplicates (based on `will_wake`).
#[derive(Clone, Default)]
struct WakerSet(SharedPtr<Mutex<Vec<Waker>>>);

impl WakerSet {
    /// Creates a new, empty `WakerSet`.
    pub fn new() -> Self {
        WakerSet(SharedPtr::new(Mutex::new(Vec::new())))
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

pub struct WritableStreamBuilder<T, Sink>
where
    T: MaybeSend + 'static,
    Sink: WritableSink<T> + 'static,
{
    sink: Sink,
    strategy: crate::platform::BoxedStrategyStatic<T>,
    _phantom: PhantomData<fn() -> T>,
}

impl<T: MaybeSend + 'static, Sink> WritableStreamBuilder<T, Sink>
where
    T: 'static,
    Sink: WritableSink<T> + 'static,
{
    fn new(sink: Sink) -> Self {
        Self {
            sink,
            strategy: Box::new(CountQueuingStrategy::new(1)),
            _phantom: PhantomData,
        }
    }

    pub fn strategy<S: QueuingStrategy<T> + MaybeSend + 'static>(mut self, s: S) -> Self {
        self.strategy = Box::new(s);
        self
    }

    /// Return stream + future without spawning
    pub fn prepare(self) -> (WritableStream<T, Sink, Unlocked>, impl Future<Output = ()>) {
        WritableStream::new_inner(self.sink, self.strategy)
    }

    /// Spawn with an owned spawner function
    pub fn spawn<F, R>(self, spawn_fn: F) -> WritableStream<T, Sink, Unlocked>
    where
        F: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream, fut) = self.prepare();
        spawn_fn(Box::pin(fut));
        stream
    }

    /// Spawn using a static spawner function reference
    pub fn spawn_ref<F, R>(self, spawn_fn: &'static F) -> WritableStream<T, Sink, Unlocked>
    where
        F: Fn(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream, fut) = self.prepare();
        spawn_fn(Box::pin(fut));
        stream
    }
}

impl<T, Sink> WritableStream<T, Sink, Unlocked>
where
    T: MaybeSend + 'static,
    Sink: WritableSink<T> + 'static,
{
    /// Returns a builder for this writable stream
    pub fn builder(sink: Sink) -> WritableStreamBuilder<T, Sink> {
        WritableStreamBuilder::new(sink)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::CountQueuingStrategy;
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct CountingSink {
        write_count: SharedPtr<Mutex<usize>>,
    }

    impl CountingSink {
        fn new() -> Self {
            CountingSink {
                write_count: SharedPtr::new(Mutex::new(0)),
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
        ) -> impl std::future::Future<Output = StreamResult<()>> {
            let count = SharedPtr::clone(&self.write_count);
            async move {
                let mut guard = count.lock().unwrap();
                *guard += 1;
                Ok(())
            }
        }
    }

    #[tokio_localset_test::localset_test]
    async fn writes_chunks_to_underlying_sink() {
        let sink = CountingSink::new();
        let expected_writes = 2;
        let strategy = CountQueuingStrategy::new(2);

        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);
        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");

        writer
            .write(vec![1, 2, 3])
            .await
            .expect("first write failed");
        writer.write(vec![4, 5]).await.expect("second write failed");
        writer.close().await.expect("close failed");

        assert_eq!(sink.get_count(), expected_writes);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_basic_write_close_lifecycle() {
        #[derive(Clone)]
        struct TestSink {
            write_count: SharedPtr<Mutex<usize>>,
        }

        impl TestSink {
            fn new() -> Self {
                Self {
                    write_count: SharedPtr::new(Mutex::new(0)),
                }
            }

            fn get_count(&self) -> usize {
                *self.write_count.lock().unwrap()
            }
        }

        impl WritableSink<Vec<u8>> for TestSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let count = self.write_count.clone();
                async move {
                    let mut guard = count.lock().unwrap();
                    *guard += 1;
                    Ok(())
                }
            }
        }

        let sink = TestSink::new();
        let strategy = CountQueuingStrategy::new(10);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);
        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");

        writer.write(vec![1]).await.expect("write failed");
        writer.write(vec![2]).await.expect("write failed");
        writer.close().await.expect("close failed");

        assert_eq!(sink.get_count(), 2);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_close_and_abort_operations() {
        #[derive(Clone)]
        struct TestSink {
            closed: SharedPtr<Mutex<bool>>,
            aborted: SharedPtr<Mutex<Option<String>>>,
        }

        impl TestSink {
            fn new() -> Self {
                Self {
                    closed: SharedPtr::new(Mutex::new(false)),
                    aborted: SharedPtr::new(Mutex::new(None)),
                }
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
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                async { Ok(()) }
            }

            fn close(self) -> impl std::future::Future<Output = StreamResult<()>> {
                let closed = self.closed.clone();
                async move {
                    *closed.lock().unwrap() = true;
                    Ok(())
                }
            }

            fn abort(
                &mut self,
                reason: Option<String>,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let aborted = self.aborted.clone();
                async move {
                    *aborted.lock().unwrap() = reason;
                    Ok(())
                }
            }
        }

        let sink = TestSink::new();
        let strategy = CountQueuingStrategy::new(1);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");

        writer
            .abort(Some("test failure".to_string()))
            .await
            .expect("abort failed");

        assert_eq!(sink.abort_reason(), Some("test failure".to_string()));

        // Operations after abort should fail
        let write_result = writer.write(vec![1]).await;
        assert!(write_result.is_err(), "write after abort should fail");

        let close_result = writer.close().await;
        assert!(close_result.is_err(), "close after abort should fail");
    }

    #[tokio_localset_test::localset_test]
    async fn enforces_writer_lock_exclusivity() {
        let sink = CountingSink::new();
        let strategy = CountQueuingStrategy::new(10);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let (_locked_stream, writer1) = stream.get_writer().expect("first get_writer failed");

        // Second writer acquisition should fail
        let second_writer_result = stream.get_writer();
        assert!(
            second_writer_result.is_err(),
            "second get_writer should fail when locked"
        );

        writer1.release_lock().expect("release_lock failed");

        // Now acquisition should succeed
        let (_locked_stream2, _writer2) = stream
            .get_writer()
            .expect("get_writer after release failed");
    }

    #[tokio_localset_test::localset_test]
    async fn applies_backpressure_correctly() {
        struct SlowSink {
            calls: SharedPtr<Mutex<usize>>,
            unblock_notify: SharedPtr<tokio::sync::Notify>,
        }

        impl SlowSink {
            fn new() -> (Self, SharedPtr<tokio::sync::Notify>) {
                let notify = SharedPtr::new(tokio::sync::Notify::new());
                (
                    Self {
                        calls: SharedPtr::new(Mutex::new(0)),
                        unblock_notify: notify.clone(),
                    },
                    notify,
                )
            }
        }

        impl WritableSink<Vec<u8>> for SlowSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let calls_clone = self.calls.clone();
                let notify = self.unblock_notify.clone();

                async move {
                    {
                        let mut guard = calls_clone.lock().unwrap();
                        *guard += 1;
                    }
                    notify.notified().await;
                    Ok(())
                }
            }
        }

        let (sink, unblock_notify) = SlowSink::new();
        let strategy = CountQueuingStrategy::new(1);
        let stream = WritableStream::builder(sink)
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);
        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");
        let writer = SharedPtr::new(writer);

        // First write starts but blocks in sink
        let writer_clone = writer.clone();
        let write1 = tokio::task::spawn_local(async move { writer_clone.write(vec![1]).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert!(
            !stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst)
        );

        // Second write should queue and trigger backpressure
        let writer_clone_2 = writer.clone();
        let write2 = tokio::task::spawn_local(async move { writer_clone_2.write(vec![2]).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert!(
            stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst)
        );

        // Ready should be pending during backpressure
        let ready_fut = writer.ready();
        tokio::pin!(ready_fut);

        let waker = futures::task::noop_waker_ref();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(matches!(
            ready_fut.as_mut().poll(&mut cx),
            std::task::Poll::Pending
        ));

        // Unblock writes
        unblock_notify.notify_one();
        write1
            .await
            .expect("write1 task failed")
            .expect("write1 failed");

        unblock_notify.notify_one();
        write2
            .await
            .expect("write2 task failed")
            .expect("write2 failed");

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert!(
            !stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst)
        );

        let ready_result = ready_fut.await;
        assert!(
            ready_result.is_ok(),
            "ready future should resolve after backpressure clears"
        );
    }

    #[derive(Clone, Default)]
    struct DummySink;

    impl WritableSink<Vec<u8>> for DummySink {
        fn write(
            &mut self,
            _chunk: Vec<u8>,
            _controller: &mut WritableStreamDefaultController,
        ) -> impl std::future::Future<Output = StreamResult<()>> {
            futures::future::ready(Ok(()))
        }
    }

    #[tokio_localset_test::localset_test]
    async fn desired_size_returns_none_when_closed_or_errored() {
        let strategy = CountQueuingStrategy::new(10);
        let sink = DummySink::default();
        let stream = WritableStream::builder(sink)
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");
        writer.close().await.expect("close failed");

        let after_close = writer.desired_size();
        assert_eq!(after_close, None, "desired_size after close must be None");

        let _ = writer.release_lock();
        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");
        writer.abort(None).await.expect("abort failed");

        let after_error = writer.desired_size();
        assert_eq!(after_error, None, "desired_size after error must be None");
    }

    #[tokio_localset_test::localset_test]
    async fn propagates_sink_errors_to_stream() {
        #[derive(Clone)]
        struct FailingSink {
            error_flag: SharedPtr<Mutex<bool>>,
        }

        impl FailingSink {
            fn new() -> Self {
                Self {
                    error_flag: SharedPtr::new(Mutex::new(false)),
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
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let flag = self.error_flag.clone();
                async move {
                    *flag.lock().unwrap() = true;
                    Err("intentional write failure".into())
                }
            }
        }

        let sink = FailingSink::new();
        let strategy = CountQueuingStrategy::new(10);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");

        let write_result = writer.write(vec![1]).await;
        assert!(write_result.is_err(), "write should error on sink failure");
        assert!(sink.did_error(), "sink error flag should be set");

        assert!(stream.errored.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            !stream
                .backpressure
                .load(std::sync::atomic::Ordering::SeqCst)
        );

        let close_result = writer.close().await;
        assert!(
            close_result.is_err(),
            "close should error when stream errored"
        );

        let closed_future_result = writer.closed().await;
        assert!(
            closed_future_result.is_err(),
            "closed future should reject on error"
        );
    }

    #[tokio_localset_test::localset_test]
    async fn writer_enqueue_when_ready_respects_backpressure() {
        let sink = CountingSink::new();
        let strategy = CountQueuingStrategy::new(2);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);
        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");

        writer
            .enqueue_when_ready(vec![1, 2, 3])
            .await
            .expect("enqueue_when_ready failed");
        writer
            .enqueue_when_ready(vec![4, 5, 6])
            .await
            .expect("enqueue_when_ready failed");

        writer.close().await.expect("close failed");

        assert_eq!(sink.get_count(), 2);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_multiple_close_calls_idempotently() {
        let stream = WritableStream::builder(DummySink)
            .strategy(CountQueuingStrategy::new(10))
            .spawn(tokio::task::spawn_local);
        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");

        writer.close().await.expect("first close failed");
        writer.close().await.expect("second close failed");
        writer.close().await.expect("third close failed");

        let closed_res = writer.closed().await;
        assert!(
            closed_res.is_ok(),
            "closed future should resolve after close"
        );
    }

    #[tokio_localset_test::localset_test]
    async fn rejects_operations_after_close() {
        let stream = WritableStream::builder(DummySink)
            .strategy(CountQueuingStrategy::new(10))
            .spawn(tokio::task::spawn_local);
        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");

        writer.close().await.expect("close failed");

        let write_err = writer.write(vec![1]).await;
        assert!(write_err.is_err(), "write after close must fail");
    }

    #[tokio_localset_test::localset_test]
    async fn ready_future_resolves_when_no_backpressure() {
        #[derive(Clone)]
        struct BlockSink {
            notify: SharedPtr<tokio::sync::Notify>,
            write_calls: SharedPtr<Mutex<usize>>,
        }

        impl BlockSink {
            fn new() -> (Self, SharedPtr<tokio::sync::Notify>) {
                let notify = SharedPtr::new(tokio::sync::Notify::new());
                (
                    Self {
                        notify: notify.clone(),
                        write_calls: SharedPtr::new(Mutex::new(0)),
                    },
                    notify,
                )
            }
        }

        impl WritableSink<Vec<u8>> for BlockSink {
            fn write(
                &mut self,
                _chunk: Vec<u8>,
                _ctrl: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let notify = self.notify.clone();
                let count_clone = self.write_calls.clone();
                async move {
                    *count_clone.lock().unwrap() += 1;
                    notify.notified().await;
                    Ok(())
                }
            }
        }

        let (sink, notify) = BlockSink::new();
        let strategy = CountQueuingStrategy::new(1usize);
        let stream = WritableStream::builder(sink)
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);
        let (_locked_stream, writer) = stream.get_writer().expect("failed to get writer");
        let writer = SharedPtr::new(writer);

        // Start first write
        let writer_clone = writer.clone();
        let write1 = tokio::task::spawn_local(async move { writer_clone.write(vec![1]).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Queue more writes to trigger backpressure
        let writer_clone = writer.clone();
        let write2 = tokio::task::spawn_local(async move { writer_clone.write(vec![2]).await });
        let writer_clone = writer.clone();
        let write3 = tokio::task::spawn_local(async move { writer_clone.write(vec![3]).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Ready should be pending during backpressure
        let mut ready = writer.ready();
        use futures::task::{Context, Poll};
        use std::pin::Pin;
        let waker = futures::task::noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        let mut pinned = Pin::new(&mut ready);
        assert!(matches!(pinned.as_mut().poll(&mut cx), Poll::Pending));

        // Unblock writes
        notify.notify_waiters();
        write1
            .await
            .expect("write1 task failed")
            .expect("write1 failed");

        notify.notify_waiters();
        write2
            .await
            .expect("write2 task failed")
            .expect("write2 failed");

        notify.notify_waiters();
        write3
            .await
            .expect("write3 task failed")
            .expect("write3 failed");

        let ready_res = ready.await;
        assert!(
            ready_res.is_ok(),
            "ready future must resolve after backpressure clears"
        );
    }
}

#[cfg(test)]
mod sink_integration_tests {
    use super::*;
    use futures::{SinkExt, StreamExt, stream};
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Debug, Clone)]
    struct TestSink {
        id: String,
        received_items: SharedPtr<Mutex<Vec<String>>>,
        write_delay: Option<Duration>,
        fail_on_write: Option<usize>,
        fail_on_close: bool,
        operation_log: SharedPtr<Mutex<Vec<String>>>,
    }

    impl TestSink {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                received_items: SharedPtr::new(Mutex::new(Vec::new())),
                write_delay: None,
                fail_on_write: None,
                fail_on_close: false,
                operation_log: SharedPtr::new(Mutex::new(Vec::new())),
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

            if let Some(delay) = self.write_delay {
                tokio::time::sleep(delay).await;
            }

            let current_count = self.received_items.lock().unwrap().len();
            if let Some(fail_on) = self.fail_on_write {
                if current_count >= fail_on {
                    self.log_operation(&format!("write failed on item {}", current_count + 1));
                    return Err(
                        format!("Intentional write failure on item {}", current_count + 1).into(),
                    );
                }
            }

            self.received_items.lock().unwrap().push(chunk.clone());
            self.log_operation(&format!("write completed: {}", chunk));
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            self.log_operation("close called");

            if self.fail_on_close {
                self.log_operation("close failed");
                return Err("Intentional close failure".into());
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
            1
        }

        fn high_water_mark(&self) -> usize {
            self.high_water_mark
        }
    }

    #[tokio_localset_test::localset_test]
    async fn processes_sink_operations_in_order() {
        let sink = TestSink::new("basic");
        let strategy = TestQueuingStrategy::new(5);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let mut sink_handle = stream;

        assert!(
            sink_handle
                .poll_ready_unpin(&mut std::task::Context::from_waker(
                    &futures::task::noop_waker()
                ))
                .is_ready()
        );

        sink_handle.start_send_unpin("item1".to_string()).unwrap();
        sink_handle.start_send_unpin("item2".to_string()).unwrap();
        sink_handle.start_send_unpin("item3".to_string()).unwrap();

        sink_handle.flush().await.unwrap();

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2", "item3"]);

        sink_handle.close().await.unwrap();

        let log = sink.get_operation_log();
        assert!(log.contains(&"basic: start called".to_string()));
        assert!(log.contains(&"basic: write completed: item1".to_string()));
        assert!(log.contains(&"basic: close completed".to_string()));
    }

    #[tokio_localset_test::localset_test]
    async fn handles_backpressure_with_small_buffer() {
        let sink = TestSink::new("backpressure").with_write_delay(Duration::from_millis(10));
        let strategy = TestQueuingStrategy::new(2);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let mut sink_handle = stream;

        sink_handle.start_send_unpin("item1".to_string()).unwrap();
        sink_handle.start_send_unpin("item2".to_string()).unwrap();

        let start = std::time::Instant::now();
        sink_handle.send("item3".to_string()).await.unwrap();

        assert!(start.elapsed() >= Duration::from_millis(10));
        sink_handle.close().await.unwrap();

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2", "item3"]);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_write_failures_appropriately() {
        let sink = TestSink::new("write_fail").with_write_failure(2);
        let strategy = TestQueuingStrategy::new(5);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let mut sink_handle = stream;

        sink_handle.send("item1".to_string()).await.unwrap();
        sink_handle.send("item2".to_string()).await.unwrap();

        let result = sink_handle.send("item3".to_string()).await;
        assert!(result.is_err(), "third write should fail");

        let ready_result = sink_handle.poll_ready_unpin(&mut std::task::Context::from_waker(
            &futures::task::noop_waker(),
        ));
        match ready_result {
            std::task::Poll::Ready(Err(_)) => {}
            other => panic!("Expected error state after write failure, got: {:?}", other),
        }

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2"]);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_close_failures() {
        let sink = TestSink::new("close_fail").with_close_failure();
        let strategy = TestQueuingStrategy::new(5);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let mut sink_handle = stream;

        sink_handle.send("item1".to_string()).await.unwrap();
        sink_handle.send("item2".to_string()).await.unwrap();

        let close_result = sink_handle.close().await;
        assert!(close_result.is_err(), "close should fail");

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2"]);
    }

    #[tokio_localset_test::localset_test]
    async fn rejects_operations_after_close() {
        let sink = TestSink::new("after_close");
        let strategy = TestQueuingStrategy::new(5);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let mut sink_handle = stream;

        sink_handle.send("item1".to_string()).await.unwrap();
        sink_handle.close().await.unwrap();

        let send_result = sink_handle.send("item2".to_string()).await;
        assert!(send_result.is_err(), "send after close should fail");

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1"]);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_abort_with_reason() {
        let sink = TestSink::new("abort_test");
        let strategy = TestQueuingStrategy::new(5);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let (_locked_stream, writer) = stream.get_writer().unwrap();

        writer.write("item1".to_string()).await.unwrap();
        writer.write("item2".to_string()).await.unwrap();

        writer
            .abort(Some("Test abort reason".to_string()))
            .await
            .unwrap();

        let log = sink.get_operation_log();
        assert!(
            log.iter()
                .any(|entry| entry.contains("abort called with reason: Test abort reason"))
        );
        assert!(log.iter().any(|entry| entry.contains("abort completed")));

        let received = sink.get_received_items();
        assert_eq!(received, vec!["item1", "item2"]);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_multiple_close_calls_idempotently() {
        let sink = TestSink::new("multi_close");
        let strategy = TestQueuingStrategy::new(5);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let mut sink_handle = stream;

        sink_handle.send("item1".to_string()).await.unwrap();

        sink_handle.close().await.unwrap();
        sink_handle.close().await.unwrap();
        sink_handle.close().await.unwrap();

        let log = sink.get_operation_log();
        let close_count = log
            .iter()
            .filter(|entry| entry.contains("close called"))
            .count();
        assert_eq!(close_count, 1, "should only see one close operation");
    }

    #[tokio_localset_test::localset_test]
    async fn flush_waits_for_all_pending_writes() {
        let sink = TestSink::new("flush_test").with_write_delay(Duration::from_millis(50));
        let strategy = TestQueuingStrategy::new(5);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let mut sink_handle = stream;

        sink_handle.start_send_unpin("item1".to_string()).unwrap();
        sink_handle.start_send_unpin("item2".to_string()).unwrap();
        sink_handle.start_send_unpin("item3".to_string()).unwrap();

        let start = std::time::Instant::now();
        sink_handle.flush().await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(30),
            "flush should wait for writes to complete"
        );

        let received_after_flush = sink.get_received_items();
        assert_eq!(received_after_flush, vec!["item1", "item2", "item3"]);

        sink_handle.close().await.unwrap();
    }

    #[tokio_localset_test::localset_test]
    async fn integrates_with_futures_stream() {
        let sink = TestSink::new("stream_integration");
        let strategy = TestQueuingStrategy::new(3);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let mut sink_handle = stream;

        let mut items = stream::iter(vec!["a", "b", "c", "d", "e"])
            .map(|s| Ok::<String, StreamError>(s.to_string()));

        sink_handle.send_all(&mut items).await.unwrap();

        let received = sink.get_received_items();
        assert_eq!(received, vec!["a", "b", "c", "d", "e"]);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_timeout_scenarios() {
        use tokio::time::timeout;

        let sink = TestSink::new("timeout_test").with_write_delay(Duration::from_millis(50));
        let strategy = TestQueuingStrategy::new(1);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let mut sink_handle = stream;

        let result = timeout(
            Duration::from_millis(10),
            sink_handle.send("slow_item".to_string()),
        )
        .await;
        assert!(result.is_err(), "operation should timeout");

        let _ = sink_handle.close().await;

        let received = sink.get_received_items();
        assert_eq!(
            received,
            vec!["slow_item"],
            "item should eventually be received"
        );
    }

    #[tokio_localset_test::localset_test]
    async fn handles_concurrent_operations() {
        use tokio::sync::Mutex;

        let sink = TestSink::new("concurrent");
        let strategy = TestQueuingStrategy::new(10);
        let stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let sink_handle = SharedPtr::new(Mutex::new(stream));
        let item_count = 100;
        let mut send_futures = Vec::new();

        for i in 0..item_count {
            let sink_clone = SharedPtr::clone(&sink_handle);
            let item = format!("item_{}", i);
            send_futures.push(tokio::task::spawn_local(async move {
                let mut sink = sink_clone.lock().await;
                sink.send(item).await
            }));
        }

        let results = futures::future::join_all(send_futures).await;
        for r in results {
            r.unwrap().unwrap();
        }

        sink_handle.lock().await.close().await.unwrap();

        let received = sink.get_received_items();
        assert_eq!(received.len(), item_count);

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
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Debug, Clone)]
    struct BytesSink {
        id: String,
        received_data: SharedPtr<Mutex<Vec<u8>>>,
        write_delay: Option<Duration>,
        fail_on_write: Option<usize>,
        fail_on_close: bool,
        operation_log: SharedPtr<Mutex<Vec<String>>>,
    }

    impl BytesSink {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                received_data: SharedPtr::new(Mutex::new(Vec::new())),
                write_delay: None,
                fail_on_write: None,
                fail_on_close: false,
                operation_log: SharedPtr::new(Mutex::new(Vec::new())),
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

        fn get_received_data(&self) -> Vec<u8> {
            self.received_data.lock().unwrap().clone()
        }

        fn get_received_string(&self) -> String {
            String::from_utf8_lossy(&self.get_received_data()).to_string()
        }

        fn log_operation(&self, op: &str) {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("{}: {}", self.id, op));
        }
    }

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

            if let Some(delay) = self.write_delay {
                tokio::time::sleep(delay).await;
            }

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
                    return Err(
                        format!("Intentional write failure on write {}", current_writes).into(),
                    );
                }
            }

            self.received_data.lock().unwrap().extend_from_slice(&chunk);
            self.log_operation(&format!("write completed: {} bytes", chunk.len()));
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            self.log_operation("close called");
            if self.fail_on_close {
                self.log_operation("close failed");
                return Err("Intentional close failure".into());
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

    #[tokio_localset_test::localset_test]
    async fn writes_bytes_through_async_write_interface() {
        let sink = BytesSink::new("basic");
        let strategy = TestQueuingStrategy::new(5);
        let mut stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let data1 = b"Hello, ";
        let data2 = b"World!";

        stream.write_all(data1).await.unwrap();
        stream.write_all(data2).await.unwrap();
        AsyncWriteExt::flush(&mut stream).await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "Hello, World!");

        stream.close().await.unwrap();
    }

    #[tokio_localset_test::localset_test]
    async fn handles_empty_writes_correctly() {
        let sink = BytesSink::new("empty");
        let strategy = TestQueuingStrategy::new(5);
        let mut stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let result = stream.write(&[]).await.unwrap();
        assert_eq!(result, 0, "empty write should return 0");

        stream.write_all(b"test").await.unwrap();
        stream.close().await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "test");
    }

    #[tokio_localset_test::localset_test]
    async fn handles_large_data_writes() {
        let sink = BytesSink::new("large");
        let strategy = TestQueuingStrategy::new(10);
        let mut stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let large_data = vec![b'X'; 10000];
        stream.write_all(&large_data).await.unwrap();
        AsyncWriteExt::flush(&mut stream).await.unwrap();

        let received = sink.get_received_data();
        assert_eq!(received.len(), 10000);
        assert!(received.iter().all(|&b| b == b'X'));

        stream.close().await.unwrap();
    }

    #[tokio_localset_test::localset_test]
    async fn processes_multiple_small_writes() {
        let sink = BytesSink::new("multi");
        let strategy = TestQueuingStrategy::new(5);
        let mut stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        for i in 0..10 {
            let data = format!("{}", i);
            stream.write_all(data.as_bytes()).await.unwrap();
        }
        AsyncWriteExt::flush(&mut stream).await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "0123456789");

        stream.close().await.unwrap();
    }

    #[tokio_localset_test::localset_test]
    async fn propagates_write_errors_correctly() {
        let sink = BytesSink::new("error").with_write_failure(2);
        let strategy = TestQueuingStrategy::new(5);
        let mut stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        stream.write_all(b"write1").await.unwrap();
        stream.write_all(b"write2").await.unwrap();

        let result = stream.write_all(b"write3").await;
        assert!(result.is_err(), "third write should fail");

        let result = stream.write_all(b"write4").await;
        assert!(result.is_err(), "subsequent writes should also fail");

        let received = sink.get_received_string();
        assert_eq!(received, "write1write2");
    }

    #[tokio_localset_test::localset_test]
    async fn rejects_writes_after_close() {
        let sink = BytesSink::new("closed");
        let strategy = TestQueuingStrategy::new(5);
        let mut stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        stream.write_all(b"before_close").await.unwrap();
        stream.close().await.unwrap();

        let result = stream.write_all(b"after_close").await;
        assert!(result.is_err(), "write after close should fail");

        if let Err(e) = result {
            assert_eq!(e.kind(), ErrorKind::BrokenPipe);
        }

        let received = sink.get_received_string();
        assert_eq!(received, "before_close");
    }

    #[tokio_localset_test::localset_test]
    async fn applies_backpressure_correctly() {
        let sink = BytesSink::new("backpressure").with_write_delay(Duration::from_millis(50));
        let strategy = TestQueuingStrategy::new(2);
        let mut stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let start = std::time::Instant::now();

        for i in 0..5 {
            let data = format!("chunk{}", i);
            stream.write_all(data.as_bytes()).await.unwrap();
        }

        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "should take time due to backpressure"
        );

        stream.close().await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "chunk0chunk1chunk2chunk3chunk4");
    }

    #[tokio_localset_test::localset_test]
    async fn handles_partial_writes() {
        let sink = BytesSink::new("partial");
        let strategy = TestQueuingStrategy::new(5);
        let mut stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let data = b"Hello, World!";

        let mut written = 0;
        while written < data.len() {
            let chunk_size = std::cmp::min(5, data.len() - written);
            let n = stream
                .write(&data[written..written + chunk_size])
                .await
                .unwrap();
            written += n;
            assert!(n > 0, "should make progress on each write");
        }

        AsyncWriteExt::flush(&mut stream).await.unwrap();
        stream.close().await.unwrap();

        let received = sink.get_received_string();
        assert_eq!(received, "Hello, World!");
    }

    #[tokio_localset_test::localset_test]
    async fn produces_appropriate_io_error_kinds() {
        // Test closed stream error
        {
            let sink = BytesSink::new("closed_error");
            let strategy = TestQueuingStrategy::new(5);
            let mut stream = WritableStream::builder(sink.clone())
                .strategy(strategy)
                .spawn(tokio::task::spawn_local);
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
            let strategy = TestQueuingStrategy::new(5);
            let mut stream = WritableStream::builder(sink.clone())
                .strategy(strategy)
                .spawn(tokio::task::spawn_local);

            let _ = stream.write_all(b"data").await;

            let result = stream.write_all(b"more_data").await;
            assert!(result.is_err());
            if let Err(e) = result {
                assert_eq!(e.kind(), ErrorKind::Other);
            }
        }
    }

    #[tokio_localset_test::localset_test]
    async fn handles_binary_data_correctly() {
        let sink = BytesSink::new("binary");
        let strategy = TestQueuingStrategy::new(5);
        let mut stream = WritableStream::builder(sink.clone())
            .strategy(strategy)
            .spawn(tokio::task::spawn_local);

        let binary_data = vec![0u8, 1, 255, 128, 0, 42, 255];
        stream.write_all(&binary_data).await.unwrap();
        stream.close().await.unwrap();

        let received = sink.get_received_data();
        assert_eq!(received, binary_data);
    }
}

#[cfg(test)]
mod writable_builder_tests {
    use super::*;

    #[cfg(feature = "send")]
    pub struct TestSink {
        pub written_data: crate::platform::SharedPtr<std::sync::Mutex<Vec<String>>>,
    }

    #[cfg(feature = "local")]
    pub struct TestSink {
        pub written_data: crate::platform::SharedPtr<std::cell::RefCell<Vec<String>>>,
    }

    impl TestSink {
        #[cfg(feature = "send")]
        pub fn new() -> Self {
            Self {
                written_data: crate::platform::SharedPtr::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        #[cfg(feature = "local")]
        pub fn new() -> Self {
            Self {
                written_data: crate::platform::SharedPtr::new(std::cell::RefCell::new(Vec::new())),
            }
        }
    }

    // Helper to access written data regardless of feature
    #[cfg(feature = "send")]
    macro_rules! get_written_data {
        ($data:expr) => {
            $data.lock().unwrap()
        };
    }

    #[cfg(feature = "local")]
    macro_rules! get_written_data {
        ($data:expr) => {
            $data.borrow()
        };
    }

    impl WritableSink<String> for TestSink {
        #[cfg(feature = "send")]
        async fn write(
            &mut self,
            chunk: String,
            _controller: &mut super::WritableStreamDefaultController,
        ) -> Result<(), StreamError> {
            self.written_data.lock().unwrap().push(chunk);
            Ok(())
        }

        #[cfg(feature = "local")]
        async fn write(
            &mut self,
            chunk: String,
            _controller: &mut super::WritableStreamDefaultController,
        ) -> Result<(), StreamError> {
            self.written_data.borrow_mut().push(chunk);
            Ok(())
        }

        async fn close(self) -> Result<(), StreamError> {
            Ok(())
        }

        async fn abort(&mut self, _reason: Option<String>) -> Result<(), StreamError> {
            Ok(())
        }
    }

    #[tokio_localset_test::localset_test]
    async fn builder_spawn_creates_working_stream() {
        let sink = TestSink::new();
        let written_data = crate::platform::SharedPtr::clone(&sink.written_data);

        let stream = WritableStream::builder(sink).spawn(tokio::task::spawn_local);
        let (_, writer) = stream.get_writer().unwrap();

        writer.write("hello".to_string()).await.unwrap();
        writer.write("world".to_string()).await.unwrap();
        writer.close().await.unwrap();

        let data = get_written_data!(written_data);
        assert_eq!(*data, vec!["hello".to_string(), "world".to_string()]);
    }

    #[tokio_localset_test::localset_test]
    async fn builder_prepare_allows_manual_spawning() {
        let sink = TestSink::new();
        let written_data = crate::platform::SharedPtr::clone(&sink.written_data);

        let (stream, fut) = WritableStream::builder(sink).prepare();

        tokio::task::spawn_local(fut);

        let (_, writer) = stream.get_writer().unwrap();

        writer.write("test".to_string()).await.unwrap();
        writer.close().await.unwrap();

        let data = get_written_data!(written_data);
        assert_eq!(*data, vec!["test".to_string()]);
    }

    fn spawn_local_fn(fut: crate::platform::PlatformFuture<'static, ()>) {
        tokio::task::spawn_local(fut);
    }

    #[tokio_localset_test::localset_test]
    async fn builder_spawn_ref_works_with_function_pointer() {
        let sink = TestSink::new();
        let written_data = crate::platform::SharedPtr::clone(&sink.written_data);

        let stream = WritableStream::builder(sink).spawn_ref(&spawn_local_fn);
        let (_, writer) = stream.get_writer().unwrap();

        writer.write("reference".to_string()).await.unwrap();
        writer.close().await.unwrap();

        let data = get_written_data!(written_data);
        assert_eq!(*data, vec!["reference".to_string()]);
    }

    #[tokio_localset_test::localset_test]
    async fn builder_accepts_custom_strategy() {
        let sink = TestSink::new();
        let written_data = crate::platform::SharedPtr::clone(&sink.written_data);

        let custom_strategy = CountQueuingStrategy::new(5);
        let stream = WritableStream::builder(sink)
            .strategy(custom_strategy)
            .spawn(tokio::task::spawn_local);

        let (_, writer) = stream.get_writer().unwrap();

        writer.write("custom".to_string()).await.unwrap();
        writer.close().await.unwrap();

        let data = get_written_data!(written_data);
        assert_eq!(*data, vec!["custom".to_string()]);
    }
}
