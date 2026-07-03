use super::super::{CountQueuingStrategy, Locked, QueuingStrategy, Unlocked};
use super::byte_state::{PendingPullInto, PullIntoOutcome};
use super::shared::AbortSignal;
use super::shared::StreamResult;
use super::shared::WakerSet;
pub use super::{
    byte_source_trait::ReadableByteSource,
    byte_state::{ByteStreamState, ByteStreamStateInterface},
    error::StreamError,
    transform::{TransformReadableSource, TransformStream},
    writable::{WritableSink, WritableStream},
};
use crate::platform::{MaybeSend, MaybeSync, SharedPtr};
use bytes::{Bytes, BytesMut};
use futures::{
    FutureExt,
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    future::{Either, poll_fn, select},
    io::AsyncRead,
    stream::{Stream, StreamExt},
};
use parking_lot::{Mutex, RwLock};
use std::{
    collections::VecDeque,
    future::Future,
    io::Result as IoResult,
    marker::PhantomData,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
    Readable,
    Closed,
    Errored,
}

// ----------- Stream Type Markers -----------
pub struct DefaultStream;
pub struct ByteStream;

// ----------- Stream Type Marker Trait -----------
pub trait StreamTypeMarker: MaybeSend + 'static {
    type Controller<T: MaybeSend + 'static>: MaybeSend + MaybeSync + Clone + 'static;
}

impl StreamTypeMarker for DefaultStream {
    type Controller<T: MaybeSend + 'static> = ReadableStreamDefaultController<T>;
}

impl StreamTypeMarker for ByteStream {
    type Controller<T: MaybeSend + 'static> = ReadableByteStreamController;
}

// ----------- Source Traits -----------
pub trait ReadableSource<T: MaybeSend + 'static>: MaybeSend + 'static {
    fn start(
        &mut self,
        #[allow(unused)] controller: &mut ReadableStreamDefaultController<T>,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend {
        async { Ok(()) }
    }

    fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend;

    fn cancel(
        &mut self,
        #[allow(unused)] reason: Option<String>,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend {
        async { Ok(()) }
    }
}

// ----------- Stream Commands -----------
enum StreamCommand<T> {
    Read {
        completion: oneshot::Sender<StreamResult<Option<T>>>,
    },
    Cancel {
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

// ----------- Controller Messages -----------
enum ControllerMsg<T> {
    Enqueue { chunk: T },
    Close,
    Error(StreamError),
}

// ----------- Controllers -----------
pub struct ReadableStreamDefaultController<T: MaybeSend + 'static> {
    tx: UnboundedSender<ControllerMsg<T>>,
    queue_total_size: SharedPtr<AtomicUsize>,
    high_water_mark: SharedPtr<AtomicUsize>,
    desired_size: SharedPtr<AtomicIsize>,
    closed: SharedPtr<AtomicBool>,
    errored: SharedPtr<AtomicBool>,
    // Both flags are set synchronously by close()/error() so that enqueue(),
    // close(), and error() can gate on them immediately — before the async
    // task processes the corresponding ControllerMsg. This mirrors the spec's
    // [[closeRequested]] flag and the synchronous state transitions in JS.
    close_requested: SharedPtr<AtomicBool>,
    error_requested: SharedPtr<AtomicBool>,
}

impl<T: MaybeSend + 'static> Clone for ReadableStreamDefaultController<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            queue_total_size: self.queue_total_size.clone(),
            high_water_mark: self.high_water_mark.clone(),
            desired_size: self.desired_size.clone(),
            closed: self.closed.clone(),
            errored: self.errored.clone(),
            close_requested: self.close_requested.clone(),
            error_requested: self.error_requested.clone(),
        }
    }
}

impl<T: MaybeSend + 'static> ReadableStreamDefaultController<T> {
    fn new(
        tx: UnboundedSender<ControllerMsg<T>>,
        queue_total_size: SharedPtr<AtomicUsize>,
        high_water_mark: SharedPtr<AtomicUsize>,
        desired_size: SharedPtr<AtomicIsize>,
        closed: SharedPtr<AtomicBool>,
        errored: SharedPtr<AtomicBool>,
        close_requested: SharedPtr<AtomicBool>,
        error_requested: SharedPtr<AtomicBool>,
    ) -> Self {
        Self {
            tx,
            queue_total_size,
            high_water_mark,
            desired_size,
            closed,
            errored,
            close_requested,
            error_requested,
        }
    }

    /// Returns true when the stream is in any terminal or terminal-bound state.
    /// Useful for breaking out of waits (e.g. backpressure loops) early.
    pub fn is_closed_or_errored(&self) -> bool {
        self.is_stream_unusable()
    }

    pub fn desired_size(&self) -> Option<isize> {
        if self.closed.load(Ordering::Acquire) || self.errored.load(Ordering::Acquire) {
            return None;
        }

        Some(self.desired_size.load(Ordering::Acquire))
    }

    fn is_stream_unusable(&self) -> bool {
        self.close_requested.load(Ordering::Acquire)
            || self.closed.load(Ordering::Acquire)
            || self.error_requested.load(Ordering::Acquire)
            || self.errored.load(Ordering::Acquire)
    }

    pub fn close(&self) -> StreamResult<()> {
        if self.is_stream_unusable() {
            return Err(StreamError::from(
                "Cannot close a ReadableStream that is already closed or errored",
            ));
        }
        self.close_requested.store(true, Ordering::Release);
        self.tx
            .unbounded_send(ControllerMsg::Close)
            .map_err(|_| StreamError::from("Failed to close stream"))?;
        Ok(())
    }

    pub fn enqueue(&self, chunk: T) -> StreamResult<()> {
        // Per spec §3.6.4: ReadableStreamDefaultControllerCanCloseOrEnqueue
        // returns false when closeRequested or state is not "readable".
        if self.close_requested.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire) {
            return Err(StreamError::from(
                "Cannot enqueue a chunk into a closed ReadableStream",
            ));
        }
        if self.error_requested.load(Ordering::Acquire) || self.errored.load(Ordering::Acquire) {
            return Err(StreamError::from(
                "Cannot enqueue a chunk into an errored ReadableStream",
            ));
        }

        self.tx
            .unbounded_send(ControllerMsg::Enqueue { chunk })
            .map_err(|_| StreamError::from("Failed to enqueue chunk"))?;
        Ok(())
    }

    pub fn error(&self, error: StreamError) -> StreamResult<()> {
        // Per spec §3.6.5: if state is not "readable", return (no-op, no throw).
        if self.is_stream_unusable() {
            return Ok(());
        }
        self.error_requested.store(true, Ordering::Release);
        self.tx
            .unbounded_send(ControllerMsg::Error(error))
            .map_err(|_| StreamError::from("Failed to error stream"))?;
        Ok(())
    }
}

pub struct ReadableByteStreamController {
    byte_state: SharedPtr<dyn ByteStreamStateInterface>,
}

impl ReadableByteStreamController {
    pub fn new<Source>(byte_state: SharedPtr<ByteStreamState<Source>>) -> Self
    where
        Source: ReadableByteSource,
    {
        Self {
            byte_state: byte_state as SharedPtr<dyn ByteStreamStateInterface>,
        }
    }

    pub fn desired_size(&self) -> Option<isize> {
        self.byte_state.desired_size()
    }

    pub fn close(&mut self) -> StreamResult<()> {
        if self.byte_state.is_closed() {
            return Err("Stream is already closed".into());
        }
        if self.byte_state.is_errored() {
            return Err("Stream is errored".into());
        }
        self.byte_state.close();
        Ok(())
    }

    pub fn enqueue(&mut self, chunk: impl Into<Bytes>) -> StreamResult<()> {
        if self.byte_state.is_closed() {
            return Err("Stream is closed".into());
        }
        if self.byte_state.is_errored() {
            return Err("Stream is errored".into());
        }

        // A caller already holding Bytes transfers it in with no byte copy; owned
        // buffers (Vec<u8>, &'static [u8]) convert at the boundary.
        self.byte_state.enqueue_bytes(chunk.into());
        Ok(())
    }

    pub fn error(&mut self, error: StreamError) -> StreamResult<()> {
        self.byte_state.error(error);
        Ok(())
    }

    /// If a BYOB owned-buffer read is waiting, returns its buffer for the source
    /// to fill directly. Filling it and calling [`ByobRequest::respond`] delivers
    /// the bytes to the reader with no queue copy. Returns `None` when no such
    /// read is pending — the source should then [`enqueue`](Self::enqueue) as
    /// usual.
    pub fn byob_request(&mut self) -> Option<ByobRequest> {
        self.byte_state.take_pull_into().map(|pending| ByobRequest {
            pending: Some(pending),
            byte_state: self.byte_state.clone(),
        })
    }
}

impl Clone for ReadableByteStreamController {
    fn clone(&self) -> Self {
        Self {
            byte_state: self.byte_state.clone(),
        }
    }
}

/// A buffer a BYOB reader transferred into the stream, handed to the source to
/// fill directly. Deref/DerefMut expose the writable byte slice; [`respond`] hands
/// it back to the reader filled. Dropping without responding returns the buffer so
/// the next pull retries.
///
/// [`respond`]: ByobRequest::respond
pub struct ByobRequest {
    pending: Option<PendingPullInto>,
    byte_state: SharedPtr<dyn ByteStreamStateInterface>,
}

impl ByobRequest {
    /// Hand the filled buffer back to the waiting reader; `n` is the number of
    /// bytes written into it. Errors (and rejects the reader) if `n` exceeds the
    /// buffer length.
    pub fn respond(mut self, n: usize) -> StreamResult<()> {
        let pending = self.pending.take().expect("respond on an active request");
        if n > pending.buf_len() {
            let err = StreamError::from("respond() byte count exceeds the request buffer");
            pending.complete_err(err.clone());
            return Err(err);
        }
        pending.complete(n);
        Ok(())
    }
}

impl std::ops::Deref for ByobRequest {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.pending.as_ref().expect("active request").buf()
    }
}

impl std::ops::DerefMut for ByobRequest {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.pending.as_mut().expect("active request").buf_mut()
    }
}

impl Drop for ByobRequest {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.byte_state.return_pull_into(pending);
        }
    }
}

// ----------- Inner State -----------
struct ReadableStreamInner<T, Source> {
    state: StreamState,
    queue: VecDeque<T>,
    queue_total_size: usize,
    strategy: crate::platform::BoxedStrategyStatic<T>,
    source: Option<Source>,
    cancel_requested: bool,
    cancel_reason: Option<String>,
    cancel_completions: Vec<oneshot::Sender<StreamResult<()>>>,
    pending_reads: VecDeque<oneshot::Sender<StreamResult<Option<T>>>>,
    ready_wakers: WakerSet,
    closed_wakers: WakerSet,
    stored_error: Option<StreamError>,
    pulling: bool,
}

impl<T: MaybeSend + 'static, Source> ReadableStreamInner<T, Source> {
    fn new(source: Source, strategy: crate::platform::BoxedStrategyStatic<T>) -> Self {
        Self {
            state: StreamState::Readable,
            queue: VecDeque::new(),
            queue_total_size: 0,
            strategy,
            source: Some(source),
            cancel_requested: false,
            cancel_reason: None,
            cancel_completions: Vec::new(),
            pending_reads: VecDeque::new(),
            ready_wakers: WakerSet::new(),
            closed_wakers: WakerSet::new(),
            stored_error: None,
            pulling: false,
        }
    }

    fn get_stored_error(&self) -> StreamError {
        self.stored_error
            .clone()
            .unwrap_or_else(|| StreamError::from("Stream is errored"))
    }

}

// ----------- Main ReadableStream with Typestate -----------
pub struct ReadableStream<T: MaybeSend + 'static, Source, StreamType, LockState = Unlocked>
where
    StreamType: StreamTypeMarker,
{
    command_tx: UnboundedSender<StreamCommand<T>>,
    queue_total_size: SharedPtr<AtomicUsize>,
    high_water_mark: SharedPtr<AtomicUsize>,
    closed: SharedPtr<AtomicBool>,
    errored: SharedPtr<AtomicBool>,
    locked: SharedPtr<AtomicBool>,
    stored_error: SharedPtr<RwLock<Option<StreamError>>>,
    desired_size: SharedPtr<AtomicIsize>,
    pub(crate) controller: SharedPtr<StreamType::Controller<T>>,
    pub(crate) byte_state: Option<SharedPtr<dyn ByteStreamStateInterface>>,
    /// Pending receiver for in-flight poll_next read (used by futures::Stream impl)
    poll_read_rx: Option<oneshot::Receiver<StreamResult<Option<T>>>>,
    _phantom: PhantomData<fn() -> (T, Source, StreamType, LockState)>,
}

impl<T: MaybeSend + 'static, Source> ReadableStream<T, Source, DefaultStream, Unlocked> {
    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }
}

impl<T: MaybeSend + 'static, Source, S> ReadableStream<T, Source, S, Unlocked>
where
    S: StreamTypeMarker,
{
    pub async fn cancel(&self, reason: Option<String>) -> StreamResult<()> {
        // Per WHATWG Streams spec §3.3.3 step 1.
        if self.locked.load(Ordering::Acquire) {
            return Err(StreamError::from("Cannot cancel a locked ReadableStream"));
        }
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .unbounded_send(StreamCommand::Cancel {
                reason,
                completion: tx,
            })
            .map_err(|_| StreamError::TaskDropped)?;
        rx.await.unwrap_or_else(|_| Err(StreamError::TaskDropped))
    }

    async fn pipe_to_inner<Sink>(
        self,
        destination: &WritableStream<T, Sink>,
        options: Option<StreamPipeOptions>,
    ) -> StreamResult<()>
    where
        Sink: WritableSink<T> + 'static,
    {
        let options = options.unwrap_or_default();
        let (_, writer) = destination.get_writer()?;
        let (_stream, reader) = self.get_reader()?;

        let pipe_loop = async {
            use futures::FutureExt;
            loop {
                // Check writer state before pulling (detects backpressure and errors
                // from the *previous* iteration's fire-and-forget write, if they have
                // already propagated by the time we loop back).
                if let Err(write_err) = writer.ready().await {
                    // Spec shutdown-with-action: cancel the source; a rejected cancel
                    // takes precedence over the write error as the pipe's rejection
                    // (finalize(r)), otherwise reject with the original write error.
                    if !options.prevent_cancel {
                        if let Err(cancel_err) = reader.cancel(Some(write_err.to_string())).await {
                            return Err(cancel_err);
                        }
                    }
                    return Err(write_err);
                }

                // Race: wait for the next chunk vs. the writer reaching a final state.
                //
                // A fire-and-forget write that errors the sink propagates asynchronously.
                // If the source then blocks in pull(), writer.ready() at the top of the
                // next iteration will never be reached — the pipe deadlocks. Selecting
                // writer.closed() alongside reader.read() breaks that deadlock: when the
                // writer enters an errored state the closed arm fires immediately, we
                // cancel the source, and the pipe terminates cleanly.
                let read_result = {
                    let read_fut = reader.read().fuse();
                    let mut closed_fut = writer.closed().fuse();
                    futures::pin_mut!(read_fut);

                    futures::select! {
                        r = read_fut => r,
                        c = closed_fut => {
                            match c {
                                Err(write_err) => {
                                    // A prior write errored the stream while source blocked.
                                    // Shutdown-with-action: a rejected source.cancel() takes
                                    // precedence over the write error (spec finalize(r)).
                                    if !options.prevent_cancel {
                                        if let Err(cancel_err) =
                                            reader.cancel(Some(write_err.to_string())).await
                                        {
                                            return Err(cancel_err);
                                        }
                                    }
                                    return Err(write_err);
                                }
                                Ok(()) => {
                                    // The destination closed while the pipe was still
                                    // live (it started closed, or was closed by someone
                                    // else). Closing propagates backward: cancel the
                                    // source so it can release resources, and reject —
                                    // a chunk cannot be piped into a closed stream. If
                                    // the cancel itself fails, that failure is what
                                    // pipeTo rejects with.
                                    let reason =
                                        "cannot pipe to a closed writable stream".to_string();
                                    let mut result_err = StreamError::from(reason.clone());
                                    if !options.prevent_cancel {
                                        if let Err(cancel_err) =
                                            reader.cancel(Some(reason)).await
                                        {
                                            result_err = cancel_err;
                                        }
                                    }
                                    return Err(result_err);
                                }
                            }
                        }
                    }
                };

                match read_result {
                    Ok(Some(chunk)) => {
                        // Fire-and-forget: spec-faithful pipelined write.
                        let _ = writer.write(chunk);
                    }
                    Ok(None) => {
                        if !options.prevent_close {
                            writer.close().await?;
                        }
                        return Ok(());
                    }
                    Err(read_err) => {
                        if !options.prevent_abort {
                            if let Err(abort_err) = writer.abort(Some(read_err.to_string())).await {
                                return Err(abort_err);
                            }
                        }
                        return Err(read_err);
                    }
                }
            }
        };

        // Run with abort support if provided
        if let Some(signal) = options.signal {
            let abort_fut = signal.aborted_future();
            futures::pin_mut!(pipe_loop, abort_fut);
            // `select` polls the abort future first, so an already-fired signal
            // wins before the pipe does any work.
            match select(abort_fut, pipe_loop).await {
                Either::Right((result, _)) => result,
                Either::Left(((), _pipe_loop)) => {
                    // Spec shutdown-with-action: cancel the source and abort the
                    // destination with the signal's reason, then reject with the
                    // action's failure if one occurred — a sink.abort() rejection
                    // preferred over a source.cancel() one — otherwise with the
                    // abort reason itself.
                    let reason = signal.reason();
                    let mut action_error = None;
                    if !options.prevent_cancel {
                        if let Err(e) = reader.cancel(reason.clone()).await {
                            action_error = Some(e);
                        }
                    }
                    if !options.prevent_abort {
                        if let Err(e) = writer.abort(reason.clone()).await {
                            action_error = Some(e);
                        }
                    }
                    Err(action_error.unwrap_or(StreamError::Aborted(reason)))
                }
            }
        } else {
            pipe_loop.await
        }
    }

    pub async fn pipe_to<Sink>(
        self,
        destination: &WritableStream<T, Sink>,
        options: Option<StreamPipeOptions>,
    ) -> StreamResult<()>
    where
        Sink: WritableSink<T> + 'static,
    {
        self.pipe_to_inner(destination, options).await
    }
}

// ===== Tee implementation =====

#[derive(Debug, Clone)]
enum TeeChunk<T> {
    Data(T),
    End,
    Error(StreamError),
}

#[derive(Debug, Clone)]
pub struct TeeConfig {
    pub backpressure_mode: BackpressureMode,
    pub branch1_hwm: Option<usize>,
    pub branch2_hwm: Option<usize>,
    pub max_buffer_per_branch: Option<usize>,
}

impl Default for TeeConfig {
    fn default() -> Self {
        Self {
            backpressure_mode: BackpressureMode::Unbounded,
            branch1_hwm: None,
            branch2_hwm: None,
            max_buffer_per_branch: Some(1000),
        }
    }
}

#[derive(Debug, Clone)]
pub enum BackpressureMode {
    /// Matches the behavior of `ReadableStream.prototype.tee()` in the WHATWG Streams spec.  
    ///
    /// - Pulls if *either branch* has demand (the faster consumer drives progress).  
    /// - Every pulled chunk is cloned and enqueued into *both* branches.  
    /// - The slower consumer buffers unboundedly (no backpressure limit applied).  
    /// - ⚠️ This can lead to unbounded memory use if one branch lags indefinitely.
    SpecCompliant,

    /// Always pulls if at least one branch is active, ignoring per-branch limits.  
    /// - Ignores backpressure entirely.  
    /// - Every chunk is cloned and sent to all active branches.  
    /// - Can quickly lead to runaway memory usage if consumers stall.  
    /// - ⚠️ Not part of the WHATWG spec — this is a "push at all costs" mode.
    Unbounded,

    /// Both branches must have buffer space before a pull is made.  
    /// - The slowest consumer throttles the entire stream.  
    /// - Prevents unbounded buffering.  
    /// - Sacrifices throughput if one branch is consistently slower.  
    /// - Useful when strict fairness between consumers is required.
    SlowestConsumer,

    /// Pulls based on the *combined demand* of both branches.
    /// - A pull is allowed if the sum of buffered items across both branches
    ///   is less than the sum of their individual high-water marks.
    /// - This adapts to per-branch configuration instead of using a fixed
    ///   multiple of a single buffer size.
    /// - Balances throughput and safety by allowing temporary imbalance,
    ///   while preventing total buffer growth from exceeding the combined limit.
    Aggregate,
}

#[derive(Clone)]
struct AsyncSignal {
    waker: SharedPtr<Mutex<Option<Waker>>>,
    signaled: SharedPtr<AtomicBool>,
}

impl AsyncSignal {
    pub fn new() -> Self {
        Self {
            waker: SharedPtr::new(Mutex::new(None)),
            signaled: SharedPtr::new(AtomicBool::new(false)),
        }
    }

    pub async fn wait(&self) {
        poll_fn(|cx| {
            if self.signaled.swap(false, Ordering::AcqRel) {
                return Poll::Ready(());
            }
            *self.waker.lock() = Some(cx.waker().clone());
            // Recheck after registering to avoid race
            if self.signaled.swap(false, Ordering::AcqRel) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }

    pub fn signal(&self) {
        self.signaled.store(true, Ordering::Release);
        if let Some(w) = self.waker.lock().take() {
            w.wake();
        }
    }
}

/// Shared cancel coordination for a tee pair.
///
/// When both tee branches cancel, the coordinator calls source.cancel() and
/// broadcasts the result here.  The "second" branch cancel() waits on this
/// so the caller receives the source cancel error rather than Ok(()).
/// `complete()` is idempotent — the first call wins; subsequent calls are no-ops.
struct TeeCancelResult {
    result: Mutex<Option<StreamResult<()>>>,
    waker: futures::task::AtomicWaker,
}

impl TeeCancelResult {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            waker: futures::task::AtomicWaker::new(),
        }
    }

    fn complete(&self, result: StreamResult<()>) {
        let mut lock = self.result.lock();
        if lock.is_some() {
            return; // already completed — first call wins
        }
        *lock = Some(result);
        drop(lock);
        self.waker.wake();
    }

    fn is_done(&self) -> bool {
        self.result.lock().is_some()
    }
}

pub struct TeeSource<T: MaybeSend + 'static> {
    chunk_rx: UnboundedReceiver<TeeChunk<T>>,
    branch_canceled: SharedPtr<AtomicBool>,

    // Cancel coordination — both TeeSource instances share these.
    first_cancel_done: SharedPtr<AtomicBool>,
    cancel_result: SharedPtr<TeeCancelResult>,

    // Optional fields used only for backpressure-aware modes.
    // None for the Unbounded fast-path.
    pending_count: Option<SharedPtr<AtomicUsize>>,
    backpressure_signal: Option<AsyncSignal>,
}

impl<T: MaybeSend + 'static> TeeSource<T> {
    /// Backpressure bookkeeping shared by the default and byte pull paths: when a
    /// chunk leaves this branch's queue, decrement the pending count and wake the
    /// coordinator so it can read the next chunk from the source.
    fn note_chunk_dequeued(&self) {
        if let Some(pending) = &self.pending_count {
            let old = pending.fetch_sub(1, Ordering::AcqRel);
            if old > 0 {
                if let Some(sig) = &self.backpressure_signal {
                    sig.signal();
                }
            }
        }
    }

    /// Cancel coordination shared by the default and byte sources. The first branch
    /// to cancel resolves immediately; the second waits for the coordinator to run
    /// the source's cancel and propagates its result.
    async fn cancel_coordinated(&mut self) -> StreamResult<()> {
        self.branch_canceled.store(true, Ordering::Release);

        // Wake coordinator if we have a signal (non-fast path).
        if let Some(sig) = &self.backpressure_signal {
            sig.signal();
        }

        // first_cancel_done.swap(true) returns the OLD value.
        // If false → we are the FIRST branch to cancel; return Ok(()) immediately.
        // If true  → we are the SECOND branch; wait for the coordinator to call
        //            source.cancel() and propagate its result.
        let is_second = self.first_cancel_done.swap(true, Ordering::AcqRel);
        if !is_second {
            return Ok(());
        }

        let cancel_result = SharedPtr::clone(&self.cancel_result);
        poll_fn(move |cx| {
            if cancel_result.is_done() {
                return std::task::Poll::Ready(());
            }
            cancel_result.waker.register(cx.waker());
            if cancel_result.is_done() {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await;

        self.cancel_result.result.lock().clone().unwrap_or(Ok(()))
    }
}

impl<T: MaybeSend + 'static> ReadableSource<T> for TeeSource<T> {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> StreamResult<()> {
        if self.branch_canceled.load(Ordering::Acquire) {
            controller.close()?;
            return Ok(());
        }

        match self.chunk_rx.next().await {
            Some(TeeChunk::Data(chunk)) => {
                self.note_chunk_dequeued();
                controller.enqueue(chunk)?;
            }
            Some(TeeChunk::End) | None => {
                controller.close()?;
            }
            Some(TeeChunk::Error(err)) => {
                return Err(err);
            }
        }

        Ok(())
    }

    async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
        self.cancel_coordinated().await
    }
}

// A byte stream's tee yields byte (BYOB-capable) branches: this source feeds a byte
// branch by enqueueing each chunk. The branches refcount-share one Bytes rather than
// copying (the spec clones via CloneAsUint8Array because JS ArrayBuffers are mutable;
// Bytes is immutable, so sharing is observably identical and safe — BYOB reads copy
// into the caller's own buffer, never mutating the shared chunk).
impl ReadableByteSource for TeeSource<Bytes> {
    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
    ) -> StreamResult<()> {
        if self.branch_canceled.load(Ordering::Acquire) {
            controller.close()?;
            return Ok(());
        }

        match self.chunk_rx.next().await {
            Some(TeeChunk::Data(chunk)) => {
                self.note_chunk_dequeued();
                controller.enqueue(chunk)?;
            }
            Some(TeeChunk::End) | None => {
                controller.close()?;
            }
            Some(TeeChunk::Error(err)) => {
                return Err(err);
            }
        }

        Ok(())
    }

    async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
        self.cancel_coordinated().await
    }
}

struct TeeCoordinator<T, Source, StreamType, LockState>
where
    T: MaybeSend + Clone + 'static,
    StreamType: StreamTypeMarker,
{
    reader: ReadableStreamDefaultReader<T, Source, StreamType, LockState>,

    branch1_tx: UnboundedSender<TeeChunk<T>>,
    branch2_tx: UnboundedSender<TeeChunk<T>>,

    branch1_canceled: SharedPtr<AtomicBool>,
    branch2_canceled: SharedPtr<AtomicBool>,

    cancel_result: SharedPtr<TeeCancelResult>,

    // Backpressure configuration
    backpressure_mode: BackpressureMode,
    branch1_pending_count: Option<SharedPtr<AtomicUsize>>,
    branch2_pending_count: Option<SharedPtr<AtomicUsize>>,

    backpressure_signal: Option<AsyncSignal>,
    branch1_high_water_mark: usize,
    branch2_high_water_mark: usize,
}

impl<T: MaybeSend + 'static, Source, StreamType, LockState>
    TeeCoordinator<T, Source, StreamType, LockState>
where
    T: Clone,
    StreamType: StreamTypeMarker,
{
    // Decide whether to pull; respects fast-path (None pending counts => Unbounded fast-path)
    fn should_pull(&self) -> bool {
        let branch1_active =
            !self.branch1_canceled.load(Ordering::Acquire) && !self.branch1_tx.is_closed();
        let branch2_active =
            !self.branch2_canceled.load(Ordering::Acquire) && !self.branch2_tx.is_closed();

        if !branch1_active && !branch2_active {
            return false;
        }

        // Fast-path: no pending counts allocated => treat as Unbounded behavior.
        if self.branch1_pending_count.is_none() || self.branch2_pending_count.is_none() {
            // fast path: pull if either active
            return branch1_active || branch2_active;
        }

        // Safe to unwrap because we checked above
        let branch1_pending = self
            .branch1_pending_count
            .as_ref()
            .unwrap()
            .load(Ordering::Acquire);
        let branch2_pending = self
            .branch2_pending_count
            .as_ref()
            .unwrap()
            .load(Ordering::Acquire);

        let branch1_hwm = self.branch1_high_water_mark;
        let branch2_hwm = self.branch2_high_water_mark;

        match self.backpressure_mode {
            BackpressureMode::Unbounded => branch1_active || branch2_active,
            BackpressureMode::SlowestConsumer => {
                let b1_ok = !branch1_active || branch1_pending < branch1_hwm;
                let b2_ok = !branch2_active || branch2_pending < branch2_hwm;
                b1_ok && b2_ok
            }
            BackpressureMode::Aggregate => {
                let total = branch1_pending + branch2_pending;
                total < (branch1_hwm + branch2_hwm)
            }
            BackpressureMode::SpecCompliant => {
                let b1_can = !branch1_active || branch1_pending < branch1_hwm;
                let b2_can = !branch2_active || branch2_pending < branch2_hwm;
                b1_can || b2_can
            }
        }
    }

    async fn distribute_chunk(&self, chunk: T) {
        let branch1_active =
            !self.branch1_canceled.load(Ordering::Acquire) && !self.branch1_tx.is_closed();
        let branch2_active =
            !self.branch2_canceled.load(Ordering::Acquire) && !self.branch2_tx.is_closed();

        // Try send to branch1
        if branch1_active {
            if self
                .branch1_tx
                .unbounded_send(TeeChunk::Data(chunk.clone()))
                .is_ok()
            {
                if let Some(p) = &self.branch1_pending_count {
                    p.fetch_add(1, Ordering::AcqRel);
                }
            } else {
                self.branch1_canceled.store(true, Ordering::Release);
            }
        }

        // Branch2
        if branch2_active {
            if self
                .branch2_tx
                .unbounded_send(TeeChunk::Data(chunk))
                .is_ok()
            {
                if let Some(p) = &self.branch2_pending_count {
                    p.fetch_add(1, Ordering::AcqRel);
                }
            } else {
                self.branch2_canceled.store(true, Ordering::Release);
            }
        }
    }

    async fn run(self) {
        self.run_inner().await;
        // Ensure the cancel_result is always completed so any branch cancel()
        // waiting on it doesn't hang (e.g. when coordinator exits via EOF or error).
        self.cancel_result.complete(Ok(()));
    }

    async fn run_inner(&self) {
        loop {
            // termination check
            let branch1_dead =
                self.branch1_canceled.load(Ordering::Acquire) || self.branch1_tx.is_closed();
            let branch2_dead =
                self.branch2_canceled.load(Ordering::Acquire) || self.branch2_tx.is_closed();

            if branch1_dead && branch2_dead {
                let result = self
                    .reader
                    .cancel(Some("Both tee branches terminated".to_string()))
                    .await;
                // Broadcast the source cancel result to whichever branch is waiting.
                self.cancel_result.complete(result);
                break;
            }

            // Wait for backpressure to clear if needed
            // If non-fast-path, wait while should_pull() is false using a real signal.
            while !self.should_pull() {
                if let Some(sig) = &self.backpressure_signal {
                    sig.wait().await;
                } else {
                    // No signal available (fast-path) — shouldn't be here, but break to avoid deadlock.
                    break;
                }

                // Recheck termination after waiting
                let branch1_dead =
                    self.branch1_canceled.load(Ordering::Acquire) || self.branch1_tx.is_closed();
                let branch2_dead =
                    self.branch2_canceled.load(Ordering::Acquire) || self.branch2_tx.is_closed();
                if branch1_dead && branch2_dead {
                    let result = self
                        .reader
                        .cancel(Some("Both tee branches terminated".to_string()))
                        .await;
                    self.cancel_result.complete(result);
                    return;
                }
            }

            // Pull from original stream
            match self.reader.read().await {
                Ok(Some(chunk)) => {
                    self.distribute_chunk(chunk).await;
                }
                Ok(None) => {
                    // EOF - notify both branches
                    let _ = self.branch1_tx.unbounded_send(TeeChunk::End);
                    let _ = self.branch2_tx.unbounded_send(TeeChunk::End);
                    break;
                }
                Err(err) => {
                    // Error - notify both branches
                    let _ = self.branch1_tx.unbounded_send(TeeChunk::Error(err.clone()));
                    let _ = self.branch2_tx.unbounded_send(TeeChunk::Error(err));
                    break;
                }
            }
        }
    }
}

pub struct TeeBuilder<T, Source, S>
where
    T: MaybeSend + Clone + 'static,
    Source: MaybeSend + 'static,
    S: StreamTypeMarker,
{
    mode: BackpressureMode,
    stream: ReadableStream<T, Source, S, Unlocked>,
    branch1_strategy: crate::platform::BoxedStrategyStatic<T>,
    branch2_strategy: crate::platform::BoxedStrategyStatic<T>,
}

impl<T: MaybeSend + 'static, Source, S> TeeBuilder<T, Source, S>
where
    T: Clone,
    Source: MaybeSend + 'static,
    S: StreamTypeMarker,
{
    fn new(stream: ReadableStream<T, Source, S, Unlocked>) -> Self {
        Self {
            stream,
            mode: BackpressureMode::SpecCompliant,
            branch1_strategy: Box::new(CountQueuingStrategy::new(1)),
            branch2_strategy: Box::new(CountQueuingStrategy::new(1)),
        }
    }

    pub fn backpressure_mode(mut self, mode: BackpressureMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set queuing strategy for the first branch
    pub fn branch1_strategy<Strategy: QueuingStrategy<T> + MaybeSend + 'static>(
        mut self,
        strategy: Strategy,
    ) -> Self {
        self.branch1_strategy = Box::new(strategy);
        self
    }

    /// Set queuing strategy for the second branch
    pub fn branch2_strategy<Strategy: QueuingStrategy<T> + MaybeSend + 'static>(
        mut self,
        strategy: Strategy,
    ) -> Self {
        self.branch2_strategy = Box::new(strategy);
        self
    }

    /// Set the same queuing strategy for both branches
    pub fn strategy<Strategy: QueuingStrategy<T> + MaybeSend + 'static + Clone>(
        mut self,
        strategy: Strategy,
    ) -> Self {
        self.branch1_strategy = Box::new(strategy.clone());
        self.branch2_strategy = Box::new(strategy);
        self
    }
}

// Terminal methods for a default-stream tee: the branches are default streams.
impl<T: MaybeSend + 'static, Source> TeeBuilder<T, Source, DefaultStream>
where
    T: Clone,
    Source: MaybeSend + 'static,
{
    /// Prepare without spawning: returns streams + futures for coordinator and branches
    pub fn prepare(
        self,
    ) -> Result<
        (
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
            impl Future<Output = ()>, // coordinator future
            impl Future<Output = ()>, // branch1 future
            impl Future<Output = ()>, // branch2 future
        ),
        StreamError,
    > {
        self.stream
            .tee_inner(self.mode, self.branch1_strategy, self.branch2_strategy)
    }

    /// Spawn the coordinator and both branches in a single task using owned closures
    pub fn spawn<F, R>(
        self,
        spawn_fn: F,
    ) -> Result<
        (
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ),
        StreamError,
    >
    where
        F: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream1, stream2, coord_fut, rfut1, rfut2) = self.prepare()?;
        let fut = async move {
            futures::join!(coord_fut, rfut1, rfut2);
        };
        spawn_fn(Box::pin(fut));
        Ok((stream1, stream2))
    }

    /// Spawn the coordinator and both branches in a single task using a 'static function reference.
    pub fn spawn_ref<F, R>(
        self,
        spawn_fn: &'static F,
    ) -> Result<
        (
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ),
        StreamError,
    >
    where
        F: Fn(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream1, stream2, coord_fut, rfut1, rfut2) = self.prepare()?;
        let fut = async move {
            futures::join!(coord_fut, rfut1, rfut2);
        };
        spawn_fn(Box::pin(fut));
        Ok((stream1, stream2))
    }

    /// Spawn each part separately using owned closures
    pub fn spawn_parts<R1, R2, R3, F1, F2, F3>(
        self,
        coordinator_spawn: F1,
        branch1_spawn: F2,
        branch2_spawn: F3,
    ) -> Result<
        (
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ),
        StreamError,
    >
    where
        F1: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R1,
        F2: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R2,
        F3: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R3,
    {
        let (stream1, stream2, coord_fut, rfut1, rfut2) = self.prepare()?;
        coordinator_spawn(Box::pin(coord_fut));
        branch1_spawn(Box::pin(rfut1));
        branch2_spawn(Box::pin(rfut2));
        Ok((stream1, stream2))
    }

    /// Spawn each part separately using static function references
    pub fn spawn_parts_ref<R1, R2, R3, F1, F2, F3>(
        self,
        coordinator_spawn: &'static F1,
        branch1_spawn: &'static F2,
        branch2_spawn: &'static F3,
    ) -> Result<
        (
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ),
        StreamError,
    >
    where
        F1: Fn(crate::platform::PlatformFuture<'static, ()>) -> R1,
        F2: Fn(crate::platform::PlatformFuture<'static, ()>) -> R2,
        F3: Fn(crate::platform::PlatformFuture<'static, ()>) -> R3,
    {
        let (stream1, stream2, coord_fut, rfut1, rfut2) = self.prepare()?;
        coordinator_spawn(Box::pin(coord_fut));
        branch1_spawn(Box::pin(rfut1));
        branch2_spawn(Box::pin(rfut2));
        Ok((stream1, stream2))
    }
}

// Terminal methods for a byte-stream tee: the branches are byte streams, so a consumer
// can `get_byob_reader()` on either branch. Config (backpressure_mode/strategies) is
// shared with the default tee via the universal TeeBuilder impl above.
impl<Source> TeeBuilder<Bytes, Source, ByteStream>
where
    Source: ReadableByteSource,
{
    /// Prepare without spawning: returns byte branches + futures for coordinator and branches
    pub fn prepare(
        self,
    ) -> Result<
        (
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
            impl Future<Output = ()>, // coordinator future
            impl Future<Output = ()>, // branch1 future
            impl Future<Output = ()>, // branch2 future
        ),
        StreamError,
    > {
        self.stream
            .tee_inner_bytes(self.mode, self.branch1_strategy, self.branch2_strategy)
    }

    /// Spawn the coordinator and both branches in a single task using owned closures
    pub fn spawn<F, R>(
        self,
        spawn_fn: F,
    ) -> Result<
        (
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
        ),
        StreamError,
    >
    where
        F: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream1, stream2, coord_fut, rfut1, rfut2) = self.prepare()?;
        let fut = async move {
            futures::join!(coord_fut, rfut1, rfut2);
        };
        spawn_fn(Box::pin(fut));
        Ok((stream1, stream2))
    }

    /// Spawn the coordinator and both branches in a single task using a 'static function reference.
    pub fn spawn_ref<F, R>(
        self,
        spawn_fn: &'static F,
    ) -> Result<
        (
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
        ),
        StreamError,
    >
    where
        F: Fn(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream1, stream2, coord_fut, rfut1, rfut2) = self.prepare()?;
        let fut = async move {
            futures::join!(coord_fut, rfut1, rfut2);
        };
        spawn_fn(Box::pin(fut));
        Ok((stream1, stream2))
    }

    /// Spawn each part separately using owned closures
    pub fn spawn_parts<R1, R2, R3, F1, F2, F3>(
        self,
        coordinator_spawn: F1,
        branch1_spawn: F2,
        branch2_spawn: F3,
    ) -> Result<
        (
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
        ),
        StreamError,
    >
    where
        F1: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R1,
        F2: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R2,
        F3: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R3,
    {
        let (stream1, stream2, coord_fut, rfut1, rfut2) = self.prepare()?;
        coordinator_spawn(Box::pin(coord_fut));
        branch1_spawn(Box::pin(rfut1));
        branch2_spawn(Box::pin(rfut2));
        Ok((stream1, stream2))
    }

    /// Spawn each part separately using static function references
    pub fn spawn_parts_ref<R1, R2, R3, F1, F2, F3>(
        self,
        coordinator_spawn: &'static F1,
        branch1_spawn: &'static F2,
        branch2_spawn: &'static F3,
    ) -> Result<
        (
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
        ),
        StreamError,
    >
    where
        F1: Fn(crate::platform::PlatformFuture<'static, ()>) -> R1,
        F2: Fn(crate::platform::PlatformFuture<'static, ()>) -> R2,
        F3: Fn(crate::platform::PlatformFuture<'static, ()>) -> R3,
    {
        let (stream1, stream2, coord_fut, rfut1, rfut2) = self.prepare()?;
        coordinator_spawn(Box::pin(coord_fut));
        branch1_spawn(Box::pin(rfut1));
        branch2_spawn(Box::pin(rfut2));
        Ok((stream1, stream2))
    }
}

impl<T: MaybeSend + 'static, Source, S> ReadableStream<T, Source, S, Unlocked>
where
    T: Clone,
    Source: MaybeSend + 'static,
    S: StreamTypeMarker,
{
    fn tee_inner(
        self,
        mode: BackpressureMode,
        branch1_strategy: crate::platform::BoxedStrategyStatic<T>,
        branch2_strategy: crate::platform::BoxedStrategyStatic<T>,
    ) -> Result<
        (
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
            ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
            impl Future<Output = ()>, // coordinator future
            impl Future<Output = ()>, // branch1 future
            impl Future<Output = ()>, // branch2 future
        ),
        StreamError,
    > {
        let (_, reader) = self.get_reader()?;

        let (branch1_tx, branch1_rx) = unbounded::<TeeChunk<T>>();
        let (branch2_tx, branch2_rx) = unbounded::<TeeChunk<T>>();

        let branch1_canceled = SharedPtr::new(AtomicBool::new(false));
        let branch2_canceled = SharedPtr::new(AtomicBool::new(false));

        let (branch1_pending, branch2_pending, backpressure_signal) =
            if matches!(mode, BackpressureMode::Unbounded) {
                (None, None, None)
            } else {
                (
                    Some(SharedPtr::new(AtomicUsize::new(0))),
                    Some(SharedPtr::new(AtomicUsize::new(0))),
                    Some(AsyncSignal::new()),
                )
            };

        let branch1_hwm = branch1_strategy.high_water_mark();
        let branch2_hwm = branch2_strategy.high_water_mark();

        let first_cancel_done = SharedPtr::new(AtomicBool::new(false));
        let cancel_result = SharedPtr::new(TeeCancelResult::new());

        let coordinator = TeeCoordinator {
            reader,
            branch1_tx: branch1_tx.clone(),
            branch2_tx: branch2_tx.clone(),
            branch1_canceled: branch1_canceled.clone(),
            branch2_canceled: branch2_canceled.clone(),
            cancel_result: cancel_result.clone(),
            backpressure_mode: mode,
            branch1_pending_count: branch1_pending.clone(),
            branch2_pending_count: branch2_pending.clone(),
            backpressure_signal: backpressure_signal.clone(),
            branch1_high_water_mark: branch1_hwm,
            branch2_high_water_mark: branch2_hwm,
        };

        let source1 = TeeSource {
            chunk_rx: branch1_rx,
            branch_canceled: branch1_canceled,
            first_cancel_done: first_cancel_done.clone(),
            cancel_result: cancel_result.clone(),
            pending_count: branch1_pending,
            backpressure_signal: backpressure_signal.clone(),
        };

        let source2 = TeeSource {
            chunk_rx: branch2_rx,
            branch_canceled: branch2_canceled,
            first_cancel_done,
            cancel_result,
            pending_count: branch2_pending,
            backpressure_signal,
        };

        let (stream1, rfut1) = ReadableStream::new_inner(source1, branch1_strategy);
        let (stream2, rfut2) = ReadableStream::new_inner(source2, branch2_strategy);

        let coordinator_fut = coordinator.run();

        Ok((stream1, stream2, coordinator_fut, rfut1, rfut2))
    }

    pub fn tee(self) -> TeeBuilder<T, Source, S> {
        TeeBuilder::new(self)
    }
}

// Byte tee path: the coordinator and channels are identical to the default tee — it
// reads Bytes chunks from the source and fans them out by refcount — but the branches
// are built as byte streams (BYOB-capable), so a consumer can `get_byob_reader()` on a
// branch.
impl<Source> ReadableStream<Bytes, Source, ByteStream, Unlocked>
where
    Source: ReadableByteSource,
{
    fn tee_inner_bytes(
        self,
        mode: BackpressureMode,
        branch1_strategy: crate::platform::BoxedStrategyStatic<Bytes>,
        branch2_strategy: crate::platform::BoxedStrategyStatic<Bytes>,
    ) -> Result<
        (
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
            ReadableStream<Bytes, TeeSource<Bytes>, ByteStream, Unlocked>,
            impl Future<Output = ()>, // coordinator future
            impl Future<Output = ()>, // branch1 future
            impl Future<Output = ()>, // branch2 future
        ),
        StreamError,
    > {
        let (_, reader) = self.get_reader()?;

        let (branch1_tx, branch1_rx) = unbounded::<TeeChunk<Bytes>>();
        let (branch2_tx, branch2_rx) = unbounded::<TeeChunk<Bytes>>();

        let branch1_canceled = SharedPtr::new(AtomicBool::new(false));
        let branch2_canceled = SharedPtr::new(AtomicBool::new(false));

        let (branch1_pending, branch2_pending, backpressure_signal) =
            if matches!(mode, BackpressureMode::Unbounded) {
                (None, None, None)
            } else {
                (
                    Some(SharedPtr::new(AtomicUsize::new(0))),
                    Some(SharedPtr::new(AtomicUsize::new(0))),
                    Some(AsyncSignal::new()),
                )
            };

        let branch1_hwm = branch1_strategy.high_water_mark();
        let branch2_hwm = branch2_strategy.high_water_mark();

        let first_cancel_done = SharedPtr::new(AtomicBool::new(false));
        let cancel_result = SharedPtr::new(TeeCancelResult::new());

        let coordinator = TeeCoordinator {
            reader,
            branch1_tx: branch1_tx.clone(),
            branch2_tx: branch2_tx.clone(),
            branch1_canceled: branch1_canceled.clone(),
            branch2_canceled: branch2_canceled.clone(),
            cancel_result: cancel_result.clone(),
            backpressure_mode: mode,
            branch1_pending_count: branch1_pending.clone(),
            branch2_pending_count: branch2_pending.clone(),
            backpressure_signal: backpressure_signal.clone(),
            branch1_high_water_mark: branch1_hwm,
            branch2_high_water_mark: branch2_hwm,
        };

        let source1 = TeeSource {
            chunk_rx: branch1_rx,
            branch_canceled: branch1_canceled,
            first_cancel_done: first_cancel_done.clone(),
            cancel_result: cancel_result.clone(),
            pending_count: branch1_pending,
            backpressure_signal: backpressure_signal.clone(),
        };

        let source2 = TeeSource {
            chunk_rx: branch2_rx,
            branch_canceled: branch2_canceled,
            first_cancel_done,
            cancel_result,
            pending_count: branch2_pending,
            backpressure_signal,
        };

        let (stream1, rfut1) = ReadableStream::new_bytes_inner(source1, branch1_strategy);
        let (stream2, rfut2) = ReadableStream::new_bytes_inner(source2, branch2_strategy);

        let coordinator_fut = coordinator.run();

        Ok((stream1, stream2, coordinator_fut, rfut1, rfut2))
    }
}

pub struct PipeBuilder<T, O, Source, S>
where
    T: MaybeSend + 'static,
    O: MaybeSend + 'static,
    S: StreamTypeMarker,
{
    source_stream: ReadableStream<T, Source, S, Unlocked>,
    transform: TransformStream<T, O>,
    options: Option<StreamPipeOptions>,
}

impl<T: MaybeSend + 'static, O: MaybeSend + 'static, Source, S> PipeBuilder<T, O, Source, S>
where
    Source: MaybeSend + 'static,
    S: StreamTypeMarker,
{
    pub fn new(
        source_stream: ReadableStream<T, Source, S, Unlocked>,
        transform: TransformStream<T, O>,
        options: Option<StreamPipeOptions>,
    ) -> Self {
        Self {
            source_stream,
            transform,
            options,
        }
    }

    /// Prepare without spawning: returns the readable and the unspawned pipe future
    pub fn prepare(
        self,
    ) -> (
        ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>,
        impl Future<Output = StreamResult<()>>,
    ) {
        let (readable, writable) = self.transform.split();

        let pipe_future = async move { self.source_stream.pipe_to(&writable, self.options).await };

        (readable, pipe_future)
    }

    /// Spawn the pipeline with an owned spawner closure
    pub fn spawn<SpawnFn, R>(
        self,
        spawn_fn: SpawnFn,
    ) -> ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>
    where
        SpawnFn: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (readable, pipe_future) = self.prepare();
        let fut = Box::pin(async move {
            let _ = pipe_future.await;
        });
        spawn_fn(fut);
        readable
    }

    /// Spawn the pipeline with a static function reference
    pub fn spawn_ref<SpawnFn, R>(
        self,
        spawn_fn: &'static SpawnFn,
    ) -> ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>
    where
        SpawnFn: Fn(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (readable, pipe_future) = self.prepare();
        let fut = Box::pin(async move {
            let _ = pipe_future.await;
        });
        spawn_fn(fut);
        readable
    }
}

impl<T: MaybeSend + 'static, Source, S> ReadableStream<T, Source, S, Unlocked>
where
    Source: MaybeSend + 'static,
    S: StreamTypeMarker,
{
    pub fn pipe_through<O>(
        self,
        transform: TransformStream<T, O>,
        options: Option<StreamPipeOptions>,
    ) -> PipeBuilder<T, O, Source, S>
    where
        O: MaybeSend + 'static,
    {
        PipeBuilder::new(self, transform, options)
    }
}

#[derive(Default)]
pub struct StreamPipeOptions {
    pub prevent_close: bool,
    pub prevent_abort: bool,
    pub prevent_cancel: bool,
    pub signal: Option<AbortSignal>,
}

// ----------- Constructor Implementation  -----------

impl<Source> ReadableStream<Bytes, Source, ByteStream, Unlocked>
where
    Source: ReadableByteSource,
{
    pub(crate) fn new_bytes_inner(
        source: Source,
        strategy: Box<dyn QueuingStrategy<Bytes> + 'static>,
    ) -> (Self, impl Future<Output = ()>) {
        let (command_tx, command_rx) = unbounded();
        let queue_total_size = SharedPtr::new(AtomicUsize::new(0));
        let closed = SharedPtr::new(AtomicBool::new(false));
        let errored = SharedPtr::new(AtomicBool::new(false));
        let locked = SharedPtr::new(AtomicBool::new(false));
        let stored_error = SharedPtr::new(RwLock::new(None));

        let high_water_mark = SharedPtr::new(AtomicUsize::new(strategy.high_water_mark()));
        let desired_size = SharedPtr::new(AtomicIsize::new(strategy.high_water_mark() as isize));

        let byte_state = ByteStreamState::new(source, strategy.high_water_mark());
        let controller = ReadableByteStreamController::new(byte_state.clone());

        let task_state = byte_state.clone();
        let task_fut = readable_byte_stream_task(task_state, command_rx, controller.clone());

        let stream = Self {
            command_tx,
            queue_total_size,
            high_water_mark,
            desired_size,
            closed,
            errored,
            locked,
            stored_error,
            controller: SharedPtr::new(controller),
            byte_state: Some(byte_state),
            poll_read_rx: None,
            _phantom: PhantomData,
        };

        (stream, task_fut)
    }
}

// ----------- Generic Constructor -----------
impl<T: MaybeSend + 'static, Source: ReadableSource<T>>
    ReadableStream<T, Source, DefaultStream, Unlocked>
{
    pub(crate) fn new_inner(
        source: Source,
        strategy: crate::platform::BoxedStrategy<T>,
    ) -> (Self, impl Future<Output = ()>) {
        let (command_tx, command_rx) = unbounded();
        let (ctrl_tx, ctrl_rx) = unbounded();
        let queue_total_size = SharedPtr::new(AtomicUsize::new(0));
        let closed = SharedPtr::new(AtomicBool::new(false));
        let errored = SharedPtr::new(AtomicBool::new(false));
        let locked = SharedPtr::new(AtomicBool::new(false));
        let stored_error = SharedPtr::new(RwLock::new(None));

        let high_water_mark = SharedPtr::new(AtomicUsize::new(strategy.high_water_mark()));
        let desired_size = SharedPtr::new(AtomicIsize::new(strategy.high_water_mark() as isize));

        let inner = ReadableStreamInner::new(source, strategy);

        let close_requested = SharedPtr::new(AtomicBool::new(false));
        let error_requested = SharedPtr::new(AtomicBool::new(false));

        let controller = ReadableStreamDefaultController::new(
            ctrl_tx.clone(),
            SharedPtr::clone(&queue_total_size),
            SharedPtr::clone(&high_water_mark),
            SharedPtr::clone(&desired_size),
            SharedPtr::clone(&closed),
            SharedPtr::clone(&errored),
            SharedPtr::clone(&close_requested),
            SharedPtr::clone(&error_requested),
        );

        let task_fut = readable_stream_task(
            command_rx,
            ctrl_rx,
            inner,
            SharedPtr::clone(&queue_total_size),
            SharedPtr::clone(&high_water_mark),
            SharedPtr::clone(&desired_size),
            SharedPtr::clone(&closed),
            SharedPtr::clone(&errored),
            SharedPtr::clone(&stored_error),
            ctrl_tx,
            controller.clone(),
        );

        let stream = Self {
            command_tx,
            queue_total_size,
            high_water_mark,
            desired_size,
            closed,
            errored,
            locked,
            stored_error,
            controller: controller.into(),
            byte_state: None,
            poll_read_rx: None,
            _phantom: PhantomData,
        };

        (stream, Box::pin(task_fut))
    }
}

// ----------- Additional reader methods for generic streams -----------
impl<T: MaybeSend + 'static, Source, StreamType> ReadableStream<T, Source, StreamType, Unlocked>
where
    StreamType: StreamTypeMarker,
{
    pub fn get_reader(
        &self,
    ) -> Result<
        (
            ReadableStream<T, Source, StreamType, Locked>,
            ReadableStreamDefaultReader<T, Source, StreamType, Locked>,
        ),
        StreamError,
    > {
        if self
            .locked
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("Stream already locked".into());
        }

        let locked_stream = ReadableStream {
            command_tx: self.command_tx.clone(),
            queue_total_size: SharedPtr::clone(&self.queue_total_size),
            high_water_mark: SharedPtr::clone(&self.high_water_mark),
            desired_size: SharedPtr::clone(&self.desired_size),
            closed: SharedPtr::clone(&self.closed),
            errored: SharedPtr::clone(&self.errored),
            locked: SharedPtr::clone(&self.locked),
            stored_error: SharedPtr::clone(&self.stored_error),
            controller: self.controller.clone(),
            byte_state: self.byte_state.clone(),
            poll_read_rx: None,
            _phantom: PhantomData,
        };

        let reader = ReadableStreamDefaultReader::new(ReadableStream {
            command_tx: self.command_tx.clone(),
            queue_total_size: self.queue_total_size.clone(),
            high_water_mark: self.high_water_mark.clone(),
            desired_size: self.desired_size.clone(),
            closed: self.closed.clone(),
            errored: self.errored.clone(),
            locked: self.locked.clone(),
            stored_error: self.stored_error.clone(),
            controller: self.controller.clone(),
            byte_state: self.byte_state.clone(),
            poll_read_rx: None,
            _phantom: PhantomData,
        });

        Ok((locked_stream, reader))
    }
}

// ----------- Reader Methods for Byte Streams -----------
impl<Source> ReadableStream<Bytes, Source, ByteStream, Unlocked>
where
    Source: ReadableByteSource,
{
    pub fn get_byob_reader(
        &self,
    ) -> Result<
        (
            ReadableStream<Bytes, Source, ByteStream, Locked>,
            ReadableStreamBYOBReader<Source, Locked>,
        ),
        StreamError,
    > {
        if self
            .locked
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(StreamError::from("Stream already locked"));
        }

        let locked_stream = ReadableStream {
            command_tx: self.command_tx.clone(),
            queue_total_size: SharedPtr::clone(&self.queue_total_size),
            high_water_mark: SharedPtr::clone(&self.high_water_mark),
            desired_size: SharedPtr::clone(&self.desired_size),
            closed: SharedPtr::clone(&self.closed),
            errored: SharedPtr::clone(&self.errored),
            locked: SharedPtr::clone(&self.locked),
            stored_error: SharedPtr::clone(&self.stored_error),
            controller: self.controller.clone(),
            byte_state: self.byte_state.clone(),
            poll_read_rx: None,
            _phantom: PhantomData,
        };

        let reader = ReadableStreamBYOBReader::new(ReadableStream {
            command_tx: self.command_tx.clone(),
            queue_total_size: self.queue_total_size.clone(),
            high_water_mark: self.high_water_mark.clone(),
            desired_size: self.desired_size.clone(),
            closed: self.closed.clone(),
            errored: self.errored.clone(),
            locked: self.locked.clone(),
            stored_error: self.stored_error.clone(),
            controller: self.controller.clone(),
            byte_state: self.byte_state.clone(),
            poll_read_rx: None,
            _phantom: PhantomData,
        });

        Ok((locked_stream, reader))
    }
}

// ----------- Stream Trait Implementation  -----------
impl<T: MaybeSend + 'static, Source, StreamType, LockState> Stream
    for ReadableStream<T, Source, StreamType, LockState>
where
    StreamType: StreamTypeMarker,
{
    type Item = StreamResult<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Safe: ReadableStream is Unpin (all fields are Unpin), so we can get &mut Self
        let this = self.get_mut();

        // If we have an in-flight read, poll it
        if let Some(rx) = this.poll_read_rx.as_mut() {
            match Pin::new(rx).poll(cx) {
                Poll::Ready(Ok(Ok(Some(chunk)))) => {
                    this.poll_read_rx = None;
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Poll::Ready(Ok(Ok(None))) => {
                    this.poll_read_rx = None;
                    return Poll::Ready(None); // stream closed
                }
                Poll::Ready(Ok(Err(e))) => {
                    this.poll_read_rx = None;
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(Err(_)) => {
                    this.poll_read_rx = None;
                    return Poll::Ready(Some(Err(StreamError::TaskDropped)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        // No in-flight read — send a Read command
        let (tx, rx) = oneshot::channel();
        if this
            .command_tx
            .unbounded_send(StreamCommand::Read { completion: tx })
            .is_err()
        {
            return Poll::Ready(Some(Err(StreamError::TaskDropped)));
        }

        // Store the receiver and poll it immediately
        this.poll_read_rx = Some(rx);
        match Pin::new(this.poll_read_rx.as_mut().unwrap()).poll(cx) {
            Poll::Ready(Ok(Ok(Some(chunk)))) => {
                this.poll_read_rx = None;
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Ok(Ok(None))) => {
                this.poll_read_rx = None;
                Poll::Ready(None)
            }
            Poll::Ready(Ok(Err(e))) => {
                this.poll_read_rx = None;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(Err(_)) => {
                this.poll_read_rx = None;
                Poll::Ready(Some(Err(StreamError::TaskDropped)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// Gated to byte streams: only they carry a byte_state to read from. The chunk type T
// is irrelevant here — AsyncRead fills the caller's buffer, it never constructs a chunk.
impl<T: MaybeSend + 'static, Source, LockState> AsyncRead
    for ReadableStream<T, Source, ByteStream, LockState>
where
    Source: ReadableByteSource,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<IoResult<usize>> {
        match self.byte_state.as_ref().unwrap().poll_read_into(cx, buf) {
            Poll::Ready(Ok(bytes)) => Poll::Ready(Ok(bytes)),
            Poll::Ready(Err(stream_err)) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                stream_err.to_string(),
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ----------- Example Source Implementations  -----------
pub struct IteratorSource<I: MaybeSend + 'static> {
    iter: I,
}

impl<I: MaybeSend + 'static, T: MaybeSend + 'static> ReadableSource<T> for IteratorSource<I>
where
    I: Iterator<Item = T> + MaybeSend + 'static,
{
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> StreamResult<()> {
        if let Some(item) = self.iter.next() {
            controller.enqueue(item)?;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

pub struct AsyncStreamSource<S: MaybeSend + 'static> {
    stream: S,
}

impl<S: MaybeSend + 'static, T: MaybeSend + 'static> ReadableSource<T> for AsyncStreamSource<S>
where
    S: Stream<Item = T> + Unpin + MaybeSend + 'static,
{
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> StreamResult<()> {
        if let Some(item) = self.stream.next().await {
            controller.enqueue(item)?;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

// ----------- Default Reader -----------
pub struct ReadableStreamDefaultReader<T: MaybeSend + 'static, Source, StreamType, LockState>(
    ReadableStream<T, Source, StreamType, LockState>,
)
where
    StreamType: StreamTypeMarker;

impl<T: MaybeSend + 'static, Source, StreamType, LockState>
    ReadableStreamDefaultReader<T, Source, StreamType, LockState>
where
    StreamType: StreamTypeMarker,
{
    pub fn new(stream: ReadableStream<T, Source, StreamType, LockState>) -> Self {
        ReadableStreamDefaultReader(stream)
    }

    fn is_byte_stream(&self) -> bool {
        self.0.byte_state.is_some()
    }

    pub async fn closed(&self) -> StreamResult<()> {
        if self.is_byte_stream() {
            return self.0.byte_state.as_ref().unwrap().closed().await;
        }

        poll_fn(|cx| {
            if self.0.errored.load(Ordering::Acquire) {
                let error = self
                    .0
                    .stored_error
                    .read()
                    .clone()
                    .unwrap_or_else(|| "Stream is errored".into());
                return Poll::Ready(Err(error));
            }
            if self.0.closed.load(Ordering::Acquire) {
                return Poll::Ready(Ok(()));
            }
            let waker = cx.waker().clone();
            let _ = self
                .0
                .command_tx
                .unbounded_send(StreamCommand::RegisterClosedWaker { waker });
            Poll::Pending
        })
        .await
    }

    pub async fn cancel(&self, reason: Option<String>) -> StreamResult<()> {
        // Byte and default streams both route through their task's Cancel handler,
        // which applies the spec terminal-state rules and closes the stream.
        let (tx, rx) = oneshot::channel();
        self.0
            .command_tx
            .unbounded_send(StreamCommand::Cancel {
                reason,
                completion: tx,
            })
            .map_err(|_| StreamError::TaskDropped)?;
        rx.await.unwrap_or_else(|_| Err(StreamError::TaskDropped))
    }

    /// Resolves when the next [`read()`] would return immediately — i.e. the queue
    /// is non-empty, the stream is closed, or the stream is errored.
    ///
    /// Useful for back-pressure-aware producers that want to avoid building up an
    /// unbounded queue of inflight reads.
    pub fn ready(&self) -> impl Future<Output = StreamResult<()>> + '_ {
        poll_fn(|cx| {
            if self.0.errored.load(Ordering::Acquire) {
                let error = self
                    .0
                    .stored_error
                    .read()
                    .clone()
                    .unwrap_or_else(|| "Stream is errored".into());
                return Poll::Ready(Err(error));
            }
            if self.0.closed.load(Ordering::Acquire)
                || self.0.queue_total_size.load(Ordering::Acquire) > 0
            {
                return Poll::Ready(Ok(()));
            }
            let waker = cx.waker().clone();
            let _ = self
                .0
                .command_tx
                .unbounded_send(StreamCommand::RegisterReadyWaker { waker });
            // Re-check after registration to close the race between checking and registering.
            if self.0.errored.load(Ordering::Acquire) {
                let error = self
                    .0
                    .stored_error
                    .read()
                    .clone()
                    .unwrap_or_else(|| "Stream is errored".into());
                return Poll::Ready(Err(error));
            }
            if self.0.closed.load(Ordering::Acquire)
                || self.0.queue_total_size.load(Ordering::Acquire) > 0
            {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        })
    }

    pub async fn read(&self) -> StreamResult<Option<T>> {
        let (tx, rx) = oneshot::channel();
        self.0
            .command_tx
            .unbounded_send(StreamCommand::Read { completion: tx })
            .map_err(|_| StreamError::TaskDropped)?;
        rx.await.unwrap_or_else(|_| Err(StreamError::TaskDropped))
    }

    pub fn release_lock(self) -> ReadableStream<T, Source, StreamType, Unlocked> {
        self.0.locked.store(false, Ordering::Release);
        ReadableStream {
            command_tx: self.0.command_tx.clone(),
            queue_total_size: self.0.queue_total_size.clone(),
            high_water_mark: self.0.high_water_mark.clone(),
            desired_size: self.0.desired_size.clone(),
            closed: self.0.closed.clone(),
            errored: self.0.errored.clone(),
            locked: self.0.locked.clone(),
            stored_error: self.0.stored_error.clone(),
            controller: self.0.controller.clone(),
            byte_state: self.0.byte_state.clone(),
            poll_read_rx: None,
            _phantom: PhantomData,
        }
    }
}

impl<T: MaybeSend + 'static, Source, StreamType, LockState> Drop
    for ReadableStreamDefaultReader<T, Source, StreamType, LockState>
where
    StreamType: StreamTypeMarker,
{
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::Release);
    }
}

// ----------- BYOB Reader (preserving original structure) -----------
pub struct ReadableStreamBYOBReader<Source, LockState>(
    ReadableStream<Bytes, Source, ByteStream, LockState>,
);

impl<Source, LockState> ReadableStreamBYOBReader<Source, LockState>
where
    Source: ReadableByteSource,
{
    pub fn new(stream: ReadableStream<Bytes, Source, ByteStream, LockState>) -> Self {
        ReadableStreamBYOBReader(stream)
    }

    pub async fn closed(&self) -> Result<(), StreamError> {
        self.0.controller.byte_state.closed().await
    }

    pub async fn cancel(&self, reason: Option<String>) -> Result<(), StreamError> {
        // Route through the byte task's Cancel handler, which runs after start_source
        // has completed — cancelling the source directly here can race task startup and
        // leave a read gated on start-ready forever.
        let (tx, rx) = oneshot::channel();
        self.0
            .command_tx
            .unbounded_send(StreamCommand::Cancel {
                reason,
                completion: tx,
            })
            .map_err(|_| StreamError::TaskDropped)?;
        rx.await.unwrap_or_else(|_| Err(StreamError::TaskDropped))
    }

    pub async fn read(&self, buf: &mut [u8]) -> StreamResult<usize> {
        poll_fn(|cx| self.0.byte_state.as_ref().unwrap().poll_read_into(cx, buf)).await
    }

    /// Read by transferring an owned buffer into the stream and getting it back
    /// filled — the zero-copy-capable BYOB read.
    ///
    /// Returns the same buffer with the bytes in `[..n]` (`n == 0` is EOF). When
    /// the queue already holds data, it is filled from the queue (one copy,
    /// preserving FIFO order). When the queue is empty, the buffer is handed to
    /// the source: a source that fills it via [`ReadableByteStreamController::byob_request`]
    /// and `respond` writes its bytes straight into this buffer with **no copy**;
    /// a source that `enqueue`s instead falls back to one copy out of the queue.
    ///
    /// `BytesMut` is the transfer unit — Rust's analog of a transferred
    /// `ArrayBuffer`: ownership leaves the caller for the duration and returns
    /// here, so the source can fill it safely across tasks. The buffer's length
    /// bounds how many bytes a single read takes; `freeze()` the result for
    /// zero-copy onward use, or reuse the buffer across reads.
    pub async fn read_owned(&self, buf: BytesMut) -> StreamResult<(BytesMut, usize)> {
        match self.0.byte_state.as_ref().unwrap().begin_pull_into(buf) {
            PullIntoOutcome::Ready(buf, n) => Ok((buf, n)),
            PullIntoOutcome::Errored(err) => Err(err),
            PullIntoOutcome::Registered(rx) => rx.await.unwrap_or_else(|_canceled| {
                Err(StreamError::from("stream dropped before the read completed"))
            }),
        }
    }

    pub fn release_lock(self) -> ReadableStream<Bytes, Source, ByteStream, Unlocked> {
        self.0.locked.store(false, Ordering::Release);
        ReadableStream {
            command_tx: self.0.command_tx.clone(),
            queue_total_size: self.0.queue_total_size.clone(),
            high_water_mark: self.0.high_water_mark.clone(),
            desired_size: self.0.desired_size.clone(),
            closed: self.0.closed.clone(),
            errored: self.0.errored.clone(),
            locked: self.0.locked.clone(),
            stored_error: self.0.stored_error.clone(),
            controller: self.0.controller.clone(),
            byte_state: self.0.byte_state.clone(),
            poll_read_rx: None,
            _phantom: PhantomData,
        }
    }
}

impl<Source, LockState> Drop for ReadableStreamBYOBReader<Source, LockState> {
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::Release);
    }
}

fn update_desired_size(
    queue_total_size: &SharedPtr<AtomicUsize>,
    high_water_mark: &SharedPtr<AtomicUsize>,
    desired_size: &SharedPtr<AtomicIsize>,
    closed: &SharedPtr<AtomicBool>,
    errored: &SharedPtr<AtomicBool>,
) {
    if closed.load(Ordering::Acquire) || errored.load(Ordering::Acquire) {
        desired_size.store(0, Ordering::Release);
        return;
    }

    let hwm = high_water_mark.load(Ordering::Acquire) as isize;
    let current = queue_total_size.load(Ordering::Acquire) as isize;
    let new_desired_size = hwm - current;

    desired_size.store(new_desired_size, Ordering::Release);
}

// Drives source.pull() while also listening for an internal cancel signal.
//
// Using select! here means that when the cancel channel fires, the pull future
// is dropped (releasing the &mut borrow on source), and the source is returned
// to the caller intact. This lets the task call source.cancel() afterwards
// without ever losing access to the source — no API change to ReadableSource needed.
async fn pull_with_cancel<T, Source>(
    source: &mut Source,
    controller: &mut ReadableStreamDefaultController<T>,
    cancel: oneshot::Receiver<()>,
) -> StreamResult<()>
where
    T: MaybeSend + 'static,
    Source: ReadableSource<T>,
{
    use futures::FutureExt;

    let pull_fut = source.pull(controller).fuse();
    let mut cancel_fut = cancel.fuse();
    futures::pin_mut!(pull_fut);

    futures::select! {
        r = pull_fut => r,
        // Cancel signal fired — return Ok so the caller treats this as a clean
        // early exit rather than a stream error.
        _ = cancel_fut => Ok(()),
    }
}

// ----------- Stream Task Implementation -----------
async fn readable_stream_task<T: 'static, Source>(
    mut command_rx: UnboundedReceiver<StreamCommand<T>>,
    mut ctrl_rx: UnboundedReceiver<ControllerMsg<T>>,
    mut inner: ReadableStreamInner<T, Source>,
    queue_total_size: SharedPtr<AtomicUsize>,
    high_water_mark: SharedPtr<AtomicUsize>,
    desired_size: SharedPtr<AtomicIsize>,
    closed: SharedPtr<AtomicBool>,
    errored: SharedPtr<AtomicBool>,
    stored_error: SharedPtr<RwLock<Option<StreamError>>>,
    _ctrl_tx: UnboundedSender<ControllerMsg<T>>,
    mut controller: ReadableStreamDefaultController<T>,
) where
    T: MaybeSend,
    Source: ReadableSource<T>,
{
    // Call start() first before processing any commands
    if let Some(mut source) = inner.source.take() {
        match source.start(&mut controller).await {
            Ok(()) => {
                // Start succeeded, put source back and continue
                inner.source = Some(source);
            }
            Err(err) => {
                // Start failed, error the stream immediately
                inner.state = StreamState::Errored;
                errored.store(true, Ordering::Release);
                inner.stored_error = Some(err.clone());
                {
                    let mut guard = stored_error.write();
                    *guard = Some(err.clone());
                }
                desired_size.store(0, Ordering::Release);
                inner.closed_wakers.wake_all();
                inner.ready_wakers.wake_all();
                // Don't return here - we still need to handle any pending commands
            }
        }
    }

    let mut pull_future: Option<
        crate::platform::PlatformBoxFutureStatic<(Source, StreamResult<()>)>,
    > = None;
    // Sender half of the in-flight pull's cancel channel. Dropping it signals
    // pull_with_cancel to abort the pull and return the source cleanly.
    let mut pull_cancel_tx: Option<oneshot::Sender<()>> = None;
    let mut cancel_future: Option<crate::platform::PlatformBoxFutureStatic<StreamResult<()>>> =
        None;

    // Edge-triggered pull request, mirroring the spec's ReadableStreamDefaultControllerCallPullIfNeeded.
    // Armed at each spec call site (after start, after an enqueue, after a read's pull steps) and
    // consumed when the gate services it. Without this, a level-triggered `desired_size > 0` gate
    // re-pulls forever on a source whose pull() enqueues nothing (busy loop). Starts armed for the
    // post-start pull.
    let mut needs_pull = true;

    poll_fn(|cx| {
        // Process controller messages first
        while let Poll::Ready(Some(msg)) = ctrl_rx.poll_next_unpin(cx) {
            match msg {
                ControllerMsg::Enqueue { chunk } => {
                    if let Some(tx) = inner.pending_reads.pop_front() {
                        let _ = tx.send(Ok(Some(chunk)));
                    } else {
                        let chunk_size = inner.strategy.size(&chunk);
                        inner.queue.push_back(chunk);
                        inner.queue_total_size += chunk_size;
                        queue_total_size.store(inner.queue_total_size, Ordering::Release);
                        update_desired_size(
                            &queue_total_size,
                            &high_water_mark,
                            &desired_size,
                            &closed,
                            &errored,
                        );
                        inner.ready_wakers.wake_all();
                    }
                    // Enqueue re-arms the pull (spec: enqueue calls CallPullIfNeeded). This is
                    // what lets an autonomous source pull again after enqueueing while a pull
                    // was in flight — the analogue of [[pullAgain]].
                    needs_pull = true;
                }
                ControllerMsg::Close => {
                    if inner.state == StreamState::Readable {
                        inner.state = StreamState::Closed;
                        closed.store(true, Ordering::Release);
                        desired_size.store(0, Ordering::Release);
                        while let Some(tx) = inner.pending_reads.pop_front() {
                            let _ = tx.send(Ok(None));
                        }
                        inner.closed_wakers.wake_all();
                        inner.ready_wakers.wake_all();
                    }
                }
                ControllerMsg::Error(err) => {
                    if inner.state != StreamState::Closed {
                        inner.state = StreamState::Errored;
                        errored.store(true, Ordering::Release);
                        inner.stored_error = Some(err.clone());
                        desired_size.store(0, Ordering::Release);
                        {
                            let mut guard = stored_error.write();
                            *guard = Some(err.clone());
                        }
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        queue_total_size.store(0, Ordering::Release);
                        while let Some(tx) = inner.pending_reads.pop_front() {
                            let _ = tx.send(Err(err.clone()));
                        }
                        inner.closed_wakers.wake_all();
                        inner.ready_wakers.wake_all();
                    }
                }
            }
        }

        // Process stream commands
        while let Poll::Ready(Some(cmd)) = command_rx.poll_next_unpin(cx) {
            match cmd {
                StreamCommand::Read { completion } => {
                    if inner.state == StreamState::Errored {
                        let _ = completion.send(Err(inner.get_stored_error()));
                        continue;
                    }
                    if inner.state == StreamState::Closed && inner.queue.is_empty() {
                        let _ = completion.send(Ok(None));
                        continue;
                    }
                    if let Some(chunk) = inner.queue.pop_front() {
                        let chunk_size = inner.strategy.size(&chunk);
                        inner.queue_total_size -= chunk_size;
                        queue_total_size.store(inner.queue_total_size, Ordering::Release);
                        update_desired_size(
                            &queue_total_size,
                            &high_water_mark,
                            &desired_size,
                            &closed,
                            &errored,
                        );
                        let _ = completion.send(Ok(Some(chunk)));
                    } else {
                        inner.pending_reads.push_back(completion);
                    }
                    // A read's pull steps re-arm the pull (spec: PullSteps calls
                    // CallPullIfNeeded), whether it dequeued or parked as a pending read.
                    needs_pull = true;
                }
                StreamCommand::Cancel { reason, completion } => {
                    // Spec ReadableStreamCancel: a closed stream resolves with undefined,
                    // an errored stream rejects with the stored error — in both cases
                    // without running the controller's cancel steps.
                    if inner.state == StreamState::Closed {
                        let _ = completion.send(Ok(()));
                        continue;
                    }
                    if inner.state == StreamState::Errored {
                        let _ = completion.send(Err(inner.get_stored_error()));
                        continue;
                    }
                    if inner.cancel_requested {
                        inner.cancel_completions.push(completion);
                    } else {
                        inner.cancel_requested = true;
                        inner.cancel_reason = reason.clone();
                        inner.cancel_completions.push(completion);
                        inner.state = StreamState::Closed;
                        closed.store(true, Ordering::Release);
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        queue_total_size.store(0, Ordering::Release);
                        while let Some(tx) = inner.pending_reads.pop_front() {
                            let _ = tx.send(Err(StreamError::Canceled));
                        }
                        inner.closed_wakers.wake_all();
                        inner.ready_wakers.wake_all();
                        if let Some(mut source) = inner.source.take() {
                            let reason_clone = reason;
                            cancel_future =
                                Some(Box::pin(async move { source.cancel(reason_clone).await }));
                        } else if let Some(tx) = pull_cancel_tx.take() {
                            // Source is inside pull_with_cancel. Signal the cancel channel:
                            // pull_with_cancel will drop the pull future (releasing source)
                            // and return Ok(()), restoring source to inner.source.
                            // The poll_fn re-enters and the Ok(()) branch below starts
                            // cancel_future so source.cancel() is called normally.
                            drop(tx);
                        }
                    }
                }
                StreamCommand::RegisterReadyWaker { waker } => {
                    inner.ready_wakers.register(&waker);
                    if !inner.queue.is_empty()
                        || inner.state == StreamState::Closed
                        || inner.state == StreamState::Errored
                    {
                        inner.ready_wakers.wake_all();
                    }
                }
                StreamCommand::RegisterClosedWaker { waker } => {
                    inner.closed_wakers.register(&waker);
                    if inner.state == StreamState::Closed || inner.state == StreamState::Errored {
                        inner.closed_wakers.wake_all();
                    }
                }
            }
        }

        // Poll cancel future if in progress
        if let Some(ref mut fut) = cancel_future {
            match fut.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    inner.cancel_requested = false;
                    for tx in inner.cancel_completions.drain(..) {
                        let _ = tx.send(result.clone());
                    }
                    cancel_future = None;
                    cx.waker().wake_by_ref();
                }
                Poll::Pending => {}
            }
        }

        // Pull data if needed — spec: ReadableStreamDefaultControllerCallPullIfNeeded.
        // Edge-triggered: only when a request is armed (needs_pull), never merely because
        // desired_size stays positive. Servicing the request consumes it — a fresh enqueue,
        // read, or pull-again re-arms it — so a source whose pull() enqueues nothing is
        // pulled exactly once, not in a busy loop.
        let current_ds = desired_size.load(std::sync::atomic::Ordering::Acquire);
        if needs_pull
            && inner.state == StreamState::Readable
            && !inner.pulling
            && !inner.cancel_requested
        {
            needs_pull = false;
            if (current_ds > 0 || !inner.pending_reads.is_empty()) && inner.source.is_some() {
                let source = inner.source.take().unwrap();
                inner.pulling = true;
                let mut ctrl = controller.clone();
                let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
                pull_cancel_tx = Some(cancel_tx);
                pull_future = Some(Box::pin(async move {
                    let mut source = source;
                    let result = pull_with_cancel(&mut source, &mut ctrl, cancel_rx).await;
                    (source, result)
                }));
            }
        }

        // Poll pull future if in progress
        if let Some(ref mut fut) = pull_future {
            match fut.as_mut().poll(cx) {
                Poll::Ready((source, result)) => {
                    inner.pulling = false;
                    pull_cancel_tx = None; // pull is done; cancel channel no longer needed
                    match result {
                        Ok(()) => {
                            // This branch covers both normal pull completion AND the case
                            // where pull_with_cancel returned Ok(()) because the cancel
                            // signal fired. Either way, source is back and cancel_future
                            // can be started if cancel was requested.
                            inner.source = Some(source);
                            if inner.cancel_requested && cancel_future.is_none() {
                                if let Some(mut src) = inner.source.take() {
                                    let reason = inner.cancel_reason.clone();
                                    cancel_future =
                                        Some(Box::pin(async move { src.cancel(reason).await }));
                                }
                            }
                        }
                        Err(err) => {
                            inner.state = StreamState::Errored;
                            errored.store(true, Ordering::Release);
                            inner.stored_error = Some(err.clone());
                            {
                                let mut guard = stored_error.write();
                                *guard = Some(err.clone());
                            }
                            while let Some(tx) = inner.pending_reads.pop_front() {
                                let _ = tx.send(Err(err.clone()));
                            }
                            // Pull failed while cancel was pending: stream is now errored,
                            // source is dropped, so resolve cancel completions immediately.
                            if inner.cancel_requested {
                                inner.cancel_requested = false;
                                for tx in inner.cancel_completions.drain(..) {
                                    let _ = tx.send(Ok(()));
                                }
                            }
                            inner.closed_wakers.wake_all();
                            inner.ready_wakers.wake_all();
                        }
                    }
                    pull_future = None;
                    cx.waker().wake_by_ref();
                }
                Poll::Pending => {}
            }
        }

        Poll::Pending::<()>
    })
    .await;
}

// ----------- Byte Stream Task Implementation -----------
async fn readable_byte_stream_task<Source>(
    byte_state: SharedPtr<ByteStreamState<Source>>,
    mut command_rx: UnboundedReceiver<StreamCommand<Bytes>>,
    mut controller: ReadableByteStreamController,
) where
    Source: ReadableByteSource,
{
    let _ = byte_state.start_source(&controller).await;

    // Pending read requests queued while no data is available
    let mut pending_reads: VecDeque<oneshot::Sender<StreamResult<Option<Bytes>>>> =
        VecDeque::new();

    loop {
        let pull_fut = poll_fn(|cx| byte_state.poll_pull_needed(cx)).fuse();
        let cmd_fut = command_rx.next().fuse();
        futures::pin_mut!(pull_fut, cmd_fut);

        futures::select! {
            // 1️⃣ Pull data from source if needed
            _ = pull_fut => {
                if byte_state.closed.load(std::sync::atomic::Ordering::Acquire)
                    || byte_state.errored.load(std::sync::atomic::Ordering::Acquire)
                {
                    // Don't break immediately - continue to process pending reads
                    continue;
                }

                let mut source = match byte_state.source.lock().take() {
                    Some(s) => s,
                    None => {
                        // No source available, but continue to handle commands
                        continue;
                    }
                };

                byte_state.mark_pull_started();

                let size_before = byte_state.buffer_size();

                match source.pull(&mut controller).await {
                    Ok(()) => {
                        // The source produces by enqueueing into the controller.
                        if byte_state.buffer_size() > size_before {
                            while let Some(completion) = pending_reads.pop_front() {
                                match poll_fn(|cx| byte_state.poll_read_chunk(cx)).now_or_never() {
                                    Some(Ok(None)) => {
                                        let _ = completion.send(Ok(None));
                                    }
                                    Some(Ok(Some(chunk))) => {
                                        let _ = completion.send(Ok(Some(chunk)));
                                        break; // Only fulfill one read per pull
                                    }
                                    Some(Err(err)) => {
                                        let _ = completion.send(Err(err));
                                    }
                                    None => {
                                        pending_reads.push_front(completion);
                                        break;
                                    }
                                }
                            }
                        }

                        // EOF is explicit: the source signals it with controller.close().
                        if byte_state.is_closed() {
                            while let Some(completion) = pending_reads.pop_front() {
                                let _ = completion.send(Ok(None));
                            }
                        }

                        // The source may error via controller.error() while pull() still returns
                        // Ok. That doesn't hit the Err arm below, so parked reads must be rejected
                        // here or they strand forever.
                        if byte_state.errored.load(std::sync::atomic::Ordering::Acquire) {
                            let error = byte_state
                                .error
                                .lock()
                                .clone()
                                .unwrap_or_else(|| "Stream errored".into());
                            while let Some(completion) = pending_reads.pop_front() {
                                let _ = completion.send(Err(error.clone()));
                            }
                        }
                    }
                    Err(err) => {
                        byte_state.error(err.clone());
                        while let Some(completion) = pending_reads.pop_front() {
                            let _ = completion.send(Err(err.clone()));
                        }
                    }
                }

                byte_state.mark_pull_completed();

                // Return source to state if still open and no error
                if !byte_state.closed.load(std::sync::atomic::Ordering::Acquire)
                    && !byte_state.errored.load(std::sync::atomic::Ordering::Acquire)
                {
                    *byte_state.source.lock() = Some(source);
                }
                // Note: Don't break here even if closed/errored - continue to handle commands
            }

            // 2️⃣ Handle Read commands
            cmd = cmd_fut => {
                match cmd {
                    Some(StreamCommand::Read { completion }) => {
                        // Error state
                        if byte_state.errored.load(Ordering::Acquire) {
                            let error = byte_state.error.lock()
                                .clone()
                                .unwrap_or_else(|| "Stream errored".into());
                            let _ = completion.send(Err(error));
                            continue;
                        }

                        // Closed and no data
                        if byte_state.closed.load(Ordering::Acquire) && byte_state.is_buffer_empty() {
                            let _ = completion.send(Ok(None));
                            continue;
                        }

                        // Attempt immediate read
                        match poll_fn(|cx| byte_state.poll_read_chunk(cx)).now_or_never() {
                            Some(Ok(None)) => {
                                let _ = completion.send(Ok(None));
                            }
                            Some(Ok(Some(chunk))) => {
                                let _ = completion.send(Ok(Some(chunk)));
                            }
                            Some(Err(err)) => {
                                let _ = completion.send(Err(err));
                            }
                            None => {
                                // No data available
                                if byte_state.closed.load(Ordering::Acquire) {
                                    // Stream is closed and no data - return EOF
                                    let _ = completion.send(Ok(None));
                                } else {
                                    // Queue read and trigger pull immediately
                                    pending_reads.push_back(completion);
                                    if !byte_state.errored.load(Ordering::Acquire) {
                                        byte_state.maybe_trigger_pull();
                                    }
                                }
                            }
                        }
                    }
                    Some(StreamCommand::Cancel { reason, completion }) => {
                        let cancel_result = byte_state.cancel_source(reason).await;
                        // cancel_source closed the stream; resolve reads already queued
                        // in this task with EOF, and keep the task alive so later reads
                        // also get EOF rather than a dropped-task error.
                        while let Some(tx) = pending_reads.pop_front() {
                            let _ = tx.send(Ok(None));
                        }
                        let _ = completion.send(cancel_result);
                    }
                    Some(_) => {}
                    None => {
                        // Command channel closed - exit only if no pending reads
                        if pending_reads.is_empty() {
                            break;
                        }
                        // If we have pending reads but stream is closed, fulfill them with EOF
                        if byte_state.closed.load(Ordering::Acquire) {
                            while let Some(completion) = pending_reads.pop_front() {
                                let _ = completion.send(Ok(None));
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
}

// ----------- Builder Pattern Implementation -----------
pub struct ReadableStreamBuilder<T, Source, StreamType = DefaultStream>
where
    T: MaybeSend + 'static,
    StreamType: StreamTypeMarker,
{
    source: Source,
    strategy: crate::platform::BoxedStrategyStatic<T>,
    _phantom: PhantomData<fn() -> (T, StreamType)>,
}

impl<T: MaybeSend + 'static, Source> ReadableStreamBuilder<T, Source, DefaultStream>
where
    Source: ReadableSource<T>,
{
    fn new(source: Source) -> Self {
        Self {
            source,
            strategy: Box::new(CountQueuingStrategy::new(1)),
            _phantom: PhantomData,
        }
    }

    pub fn strategy<S: QueuingStrategy<T> + MaybeSend + 'static>(mut self, s: S) -> Self {
        self.strategy = Box::new(s);
        self
    }

    /// Return stream + future without spawning
    pub fn prepare(
        self,
    ) -> (
        ReadableStream<T, Source, DefaultStream, Unlocked>,
        impl Future<Output = ()>,
    ) {
        ReadableStream::new_inner(self.source, self.strategy)
    }

    /// Spawn bundled into one task
    pub fn spawn<F, R>(self, spawn_fn: F) -> ReadableStream<T, Source, DefaultStream, Unlocked>
    where
        F: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream, fut) = self.prepare();
        spawn_fn(Box::pin(fut));
        stream
    }

    /// Spawn using a static function reference
    pub fn spawn_ref<F, R>(
        self,
        spawn_fn: &'static F,
    ) -> ReadableStream<T, Source, DefaultStream, Unlocked>
    where
        F: Fn(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream, fut) = self.prepare();
        spawn_fn(Box::pin(fut));
        stream
    }
}

// Byte stream builder - specialized for Bytes
impl<Source> ReadableStreamBuilder<Bytes, Source, ByteStream>
where
    Source: ReadableByteSource,
{
    fn new_bytes(source: Source) -> Self {
        Self {
            source,
            strategy: Box::new(CountQueuingStrategy::new(1)),
            _phantom: PhantomData,
        }
    }

    pub fn strategy<S: QueuingStrategy<Bytes> + MaybeSend + 'static>(mut self, s: S) -> Self {
        self.strategy = Box::new(s);
        self
    }

    /// Return stream + future without spawning
    pub fn prepare(
        self,
    ) -> (
        ReadableStream<Bytes, Source, ByteStream, Unlocked>,
        impl Future<Output = ()>,
    ) {
        ReadableStream::new_bytes_inner(self.source, self.strategy)
    }

    /// Spawn with an owned spawner function
    pub fn spawn<F, R>(self, spawn_fn: F) -> ReadableStream<Bytes, Source, ByteStream, Unlocked>
    where
        F: FnOnce(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream, fut) = self.prepare();
        spawn_fn(Box::pin(fut));
        stream
    }

    /// Spawn using a static spawner function reference
    pub fn spawn_ref<F, R>(
        self,
        spawn_fn: &'static F,
    ) -> ReadableStream<Bytes, Source, ByteStream, Unlocked>
    where
        F: Fn(crate::platform::PlatformFuture<'static, ()>) -> R,
    {
        let (stream, fut) = self.prepare();
        spawn_fn(Box::pin(fut));
        stream
    }
}

// Main ReadableStream impl - default streams
impl<T: MaybeSend + 'static, Source> ReadableStream<T, Source, DefaultStream, Unlocked>
where
    Source: ReadableSource<T>,
{
    /// Returns a builder for this readable stream
    pub fn builder(source: Source) -> ReadableStreamBuilder<T, Source, DefaultStream> {
        ReadableStreamBuilder::new(source)
    }
}

// Shortcut methods on ReadableStream for common cases
impl<T: MaybeSend + 'static>
    ReadableStream<T, IteratorSource<std::vec::IntoIter<T>>, DefaultStream, Unlocked>
{
    /// Create from Vec - shortcut for ReadableStreamBuilder::from_vec()
    pub fn from_vec(
        vec: Vec<T>,
    ) -> ReadableStreamBuilder<T, IteratorSource<std::vec::IntoIter<T>>, DefaultStream> {
        ReadableStreamBuilder::from_vec(vec)
    }
}

impl<T: MaybeSend + 'static, I> ReadableStream<T, IteratorSource<I>, DefaultStream, Unlocked>
where
    I: Iterator<Item = T> + MaybeSend + 'static,
{
    /// Create from Iterator - shortcut for ReadableStreamBuilder::from_iterator()
    pub fn from_iterator(iter: I) -> ReadableStreamBuilder<T, IteratorSource<I>, DefaultStream> {
        ReadableStreamBuilder::from_iterator(iter)
    }
}

impl<T: MaybeSend + 'static, S> ReadableStream<T, AsyncStreamSource<S>, DefaultStream, Unlocked>
where
    S: Stream<Item = T> + Unpin + MaybeSend + 'static,
{
    /// Create from Stream - shortcut for ReadableStreamBuilder::from_stream()
    pub fn from_stream(stream: S) -> ReadableStreamBuilder<T, AsyncStreamSource<S>, DefaultStream> {
        ReadableStreamBuilder::from_stream(stream)
    }
}

// Byte streams
impl<Source> ReadableStream<Bytes, Source, ByteStream, Unlocked>
where
    Source: ReadableByteSource,
{
    /// Returns a builder for byte streams
    pub fn builder_bytes(source: Source) -> ReadableStreamBuilder<Bytes, Source, ByteStream> {
        ReadableStreamBuilder::new_bytes(source)
    }
}

// Convenience constructors as static methods on the builder
impl<T: MaybeSend + 'static>
    ReadableStreamBuilder<T, IteratorSource<std::vec::IntoIter<T>>, DefaultStream>
{
    /// Create a builder from a Vec
    pub fn from_vec(vec: Vec<T>) -> Self {
        Self::new(IteratorSource {
            iter: vec.into_iter(),
        })
    }
}

impl<T: MaybeSend + 'static, I> ReadableStreamBuilder<T, IteratorSource<I>, DefaultStream>
where
    I: Iterator<Item = T> + MaybeSend + 'static,
{
    /// Create a builder from an Iterator
    pub fn from_iterator(iter: I) -> Self {
        Self::new(IteratorSource { iter })
    }
}

impl<T: MaybeSend + 'static, S> ReadableStreamBuilder<T, AsyncStreamSource<S>, DefaultStream>
where
    S: Stream<Item = T> + Unpin + MaybeSend + 'static,
{
    /// Create a builder from a Stream
    pub fn from_stream(stream: S) -> Self {
        Self::new(AsyncStreamSource { stream })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio_localset_test::localset_test]
    async fn reads_items_sequentially_from_iterator() {
        let data = vec![1, 2, 3, 4, 5];
        let stream = ReadableStream::from_iterator(data.clone().into_iter()).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = stream.get_reader().unwrap();

        for expected in data {
            assert_eq!(reader.read().await.unwrap(), Some(expected));
        }

        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn transitions_to_closed_state_after_exhaustion() {
        let data = vec![1, 2, 3];
        let stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);
        let (_locked, reader) = stream.get_reader().unwrap();

        assert!(!reader.0.closed.load(std::sync::atomic::Ordering::Acquire));
        assert!(!reader.0.errored.load(std::sync::atomic::Ordering::Acquire));

        while reader.read().await.unwrap().is_some() {}

        reader.closed().await.unwrap();
        assert!(reader.0.closed.load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio_localset_test::localset_test]
    async fn handles_empty_stream_immediately() {
        let empty: Vec<i32> = vec![];
        let stream = ReadableStream::from_iterator(empty.into_iter()).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), None);
        reader.closed().await.unwrap();
    }

    #[tokio_localset_test::localset_test]
    async fn enforces_stream_locking_correctly() {
        let data = vec![1, 2, 3];
        let stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        assert!(!stream.locked());

        let (_locked_stream, reader) = stream.get_reader().unwrap();
        let unlocked_stream = reader.release_lock();
        assert!(!unlocked_stream.locked());
    }

    #[tokio_localset_test::localset_test]
    async fn auto_unlocks_on_reader_drop() {
        let data = vec![1, 2, 3];
        let stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);
        let locked_ref = SharedPtr::clone(&stream.locked);

        {
            let (_locked_stream, _reader) = stream.get_reader().unwrap();
        } // Reader drops here

        assert!(!locked_ref.load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio_localset_test::localset_test]
    async fn cancels_stream_and_stops_reading() {
        let data = vec![1, 2, 3, 4, 5];
        let stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);
        let (_locked, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some(1));

        reader
            .cancel(Some("test cancellation".to_string()))
            .await
            .unwrap();

        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn cancels_without_reason() {
        let data = vec![1, 2, 3];
        let stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);
        let (_locked, reader) = stream.get_reader().unwrap();

        reader.cancel(None).await.unwrap();
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn propagates_source_errors() {
        struct ErroringSource {
            call_count: std::cell::RefCell<usize>,
        }

        impl ReadableSource<i32> for ErroringSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<i32>,
            ) -> StreamResult<()> {
                let count = *self.call_count.borrow();
                *self.call_count.borrow_mut() += 1;

                if count == 0 {
                    controller.enqueue(42)?;
                    Ok(())
                } else {
                    controller.error("Source error".into())?;
                    Ok(())
                }
            }
        }

        let source = ErroringSource {
            call_count: std::cell::RefCell::new(0),
        };
        let stream = ReadableStream::builder(source).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some(42));

        let read_result = reader.read().await;
        assert!(read_result.is_err(), "Second read should propagate error");
        assert!(reader.0.errored.load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio_localset_test::localset_test]
    async fn integrates_with_async_streams() {
        let items = vec![10, 20, 30];
        let async_stream = stream::iter(items.clone());

        let readable_stream =
            ReadableStream::from_stream(async_stream).spawn(tokio::task::spawn_local);
        let (_locked, reader) = readable_stream.get_reader().unwrap();

        for expected in items {
            assert_eq!(reader.read().await.unwrap(), Some(expected));
        }
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn futures_stream_trait_yields_all_chunks() {
        use futures::stream::StreamExt;

        let items = vec![1, 2, 3, 4, 5];
        let mut stream = ReadableStream::from_vec(items.clone()).spawn(tokio::task::spawn_local);

        let mut collected = Vec::new();
        while let Some(result) = stream.next().await {
            collected.push(result.unwrap());
        }
        assert_eq!(collected, items);
    }

    #[tokio_localset_test::localset_test]
    async fn futures_stream_trait_propagates_errors() {
        use futures::stream::StreamExt;

        struct FailSource {
            count: usize,
        }

        impl ReadableSource<String> for FailSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<String>,
            ) -> StreamResult<()> {
                self.count += 1;
                if self.count <= 2 {
                    controller.enqueue(format!("item-{}", self.count))?;
                    Ok(())
                } else {
                    Err(StreamError::from("source failed"))
                }
            }
        }

        let mut stream =
            ReadableStream::builder(FailSource { count: 0 }).spawn(tokio::task::spawn_local);

        // First two items succeed
        assert_eq!(stream.next().await.unwrap().unwrap(), "item-1");
        assert_eq!(stream.next().await.unwrap().unwrap(), "item-2");

        // Third should be an error
        let err = stream.next().await.unwrap().unwrap_err();
        assert!(err.to_string().contains("source failed"));
    }

    #[tokio_localset_test::localset_test]
    async fn handles_byte_stream_operations() {
        struct ChunkedByteSource {
            chunks: Vec<Vec<u8>>,
            index: std::cell::RefCell<usize>,
        }

        impl ReadableByteSource for ChunkedByteSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
            ) -> StreamResult<()> {
                let idx = *self.index.borrow();

                if idx >= self.chunks.len() {
                    controller.close()?;
                    return Ok(());
                }

                let chunk = self.chunks[idx].clone();
                *self.index.borrow_mut() = idx + 1;

                controller.enqueue(chunk)?;
                Ok(())
            }
        }

        let source = ChunkedByteSource {
            chunks: vec![b"hello".to_vec(), b" world".to_vec()],
            index: std::cell::RefCell::new(0),
        };

        let stream = ReadableStream::builder_bytes(source).spawn(tokio::task::spawn_local);
        let (_locked, reader) = stream.get_reader().unwrap();

        let mut all_data = Vec::new();
        while let Some(chunk) = reader.read().await.unwrap() {
            all_data.extend(chunk);
        }

        assert_eq!(all_data, b"hello world");
    }

    #[tokio_localset_test::localset_test]
    async fn supports_byob_reader_operations() {
        struct SingleChunkByteSource {
            data: Vec<u8>,
            consumed: std::cell::RefCell<bool>,
        }

        impl ReadableByteSource for SingleChunkByteSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
            ) -> StreamResult<()> {
                if *self.consumed.borrow() {
                    controller.close()?;
                    return Ok(());
                }

                let chunk = self.data.clone();
                *self.consumed.borrow_mut() = true;

                controller.enqueue(chunk)?;
                Ok(())
            }
        }

        let source = SingleChunkByteSource {
            data: b"byob test data".to_vec(),
            consumed: std::cell::RefCell::new(false),
        };

        let stream = ReadableStream::builder_bytes(source).spawn(tokio::task::spawn_local);
        let (_locked, byob_reader) = stream.get_byob_reader().unwrap();

        let mut buffer = [0u8; 20];
        let bytes_read = byob_reader.read(&mut buffer).await.unwrap();

        assert!(bytes_read > 0, "BYOB reader should return bytes read");
        assert_eq!(byob_reader.read(&mut buffer).await.unwrap(), 0);
    }

    #[tokio_localset_test::localset_test]
    async fn read_owned_direct_fill_single_threaded() {
        // The owned-buffer handoff (move out, fill, move back) works under the
        // single-threaded Rc runtime as well as the multi-threaded one.
        struct DirectSource {
            data: Vec<u8>,
            sent: std::cell::RefCell<bool>,
        }

        impl ReadableByteSource for DirectSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
            ) -> StreamResult<()> {
                if *self.sent.borrow() {
                    controller.close()?;
                    return Ok(());
                }
                if let Some(mut req) = controller.byob_request() {
                    let n = self.data.len().min(req.len());
                    req[..n].copy_from_slice(&self.data[..n]);
                    *self.sent.borrow_mut() = true;
                    req.respond(n)?;
                } else {
                    controller.enqueue(self.data.clone())?;
                    *self.sent.borrow_mut() = true;
                }
                Ok(())
            }
        }

        let source = DirectSource {
            data: b"owned bytes".to_vec(),
            sent: std::cell::RefCell::new(false),
        };
        let stream = ReadableStream::builder_bytes(source).spawn(tokio::task::spawn_local);
        let (_locked, byob) = stream.get_byob_reader().unwrap();

        let (buf, n) = byob.read_owned(BytesMut::zeroed(32)).await.unwrap();
        assert_eq!(&buf[..n], b"owned bytes");

        let (_buf, n) = byob.read_owned(BytesMut::zeroed(32)).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio_localset_test::localset_test]
    async fn controller_closes_stream_properly() {
        struct ControlledSource {
            items: Vec<i32>,
            index: std::cell::RefCell<usize>,
        }

        impl ReadableSource<i32> for ControlledSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<i32>,
            ) -> StreamResult<()> {
                let idx = *self.index.borrow();

                if idx >= self.items.len() {
                    controller.close()?;
                    return Ok(());
                }

                controller.enqueue(self.items[idx])?;
                *self.index.borrow_mut() = idx + 1;
                Ok(())
            }
        }

        let source = ControlledSource {
            items: vec![1, 2],
            index: std::cell::RefCell::new(0),
        };

        let stream = ReadableStream::builder(source).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert_eq!(reader.read().await.unwrap(), None);
        reader.closed().await.unwrap();
    }

    #[tokio_localset_test::localset_test]
    async fn controller_errors_stream_correctly() {
        struct ErrorAfterItemsSource {
            sent_items: std::cell::RefCell<bool>,
        }

        impl ReadableSource<String> for ErrorAfterItemsSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<String>,
            ) -> StreamResult<()> {
                if !*self.sent_items.borrow() {
                    controller.enqueue("valid item".to_string())?;
                    *self.sent_items.borrow_mut() = true;
                    Ok(())
                } else {
                    controller.error("Controller error".into())?;
                    Ok(())
                }
            }
        }

        let source = ErrorAfterItemsSource {
            sent_items: std::cell::RefCell::new(false),
        };

        let stream = ReadableStream::builder(source).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some("valid item".to_string()));

        let read_result = reader.read().await;
        assert!(read_result.is_err(), "Controller error should propagate");
    }

    #[tokio_localset_test::localset_test]
    async fn handles_concurrent_read_attempts() {
        let data: Vec<i32> = (0..10).collect();
        let stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);
        let (_locked, reader) = stream.get_reader().unwrap();

        let read1 = reader.read();
        let read2 = reader.read();

        let result1 = timeout(Duration::from_millis(100), read1).await.unwrap();
        let result2 = timeout(Duration::from_millis(100), read2).await.unwrap();

        assert!(result1.is_ok());
        assert!(result2.is_ok());
    }

    #[tokio_localset_test::localset_test]
    async fn notifies_when_closed() {
        let data = vec![1];
        let stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);
        let (_locked, reader) = stream.get_reader().unwrap();

        let close_future = reader.closed();

        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), None);

        timeout(Duration::from_millis(100), close_future)
            .await
            .expect("Stream should close within timeout")
            .expect("Close should succeed");
    }

    #[tokio_localset_test::localset_test]
    async fn processes_large_streams_efficiently() {
        let large_data: Vec<i32> = (0..1000).collect();
        let expected_sum: i32 = large_data.iter().sum();

        let stream = ReadableStream::from_iterator(large_data.into_iter()).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = stream.get_reader().unwrap();

        let mut actual_sum = 0;
        while let Some(item) = reader.read().await.unwrap() {
            actual_sum += item;
        }

        assert_eq!(actual_sum, expected_sum);
    }

    #[tokio_localset_test::localset_test]
    async fn supports_push_based_sources() {
        use parking_lot::Mutex;

        struct PushStartSource {
            data: Vec<i32>,
            enqueued: SharedPtr<Mutex<bool>>,
        }

        impl ReadableSource<i32> for PushStartSource {
            fn start(
                &mut self,
                controller: &mut ReadableStreamDefaultController<i32>,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let enqueued = self.enqueued.clone();
                let data = self.data.clone();

                async move {
                    let mut enq_lock = enqueued.lock();
                    if !*enq_lock {
                        for item in data {
                            controller.enqueue(item)?;
                        }
                        controller.close()?;
                        *enq_lock = true;
                    }
                    Ok(())
                }
            }

            fn pull(
                &mut self,
                _controller: &mut ReadableStreamDefaultController<i32>,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                async { Ok(()) }
            }
        }

        let source = PushStartSource {
            data: vec![10, 20, 30],
            enqueued: SharedPtr::new(Mutex::new(false)),
        };

        let stream = ReadableStream::builder(source).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked_stream, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some(10));
        assert_eq!(reader.read().await.unwrap(), Some(20));
        assert_eq!(reader.read().await.unwrap(), Some(30));
        assert_eq!(reader.read().await.unwrap(), None);

        reader.closed().await.unwrap();
    }

    #[tokio_localset_test::localset_test]
    async fn errors_when_start_method_fails() {
        struct FailingStartSource;

        impl ReadableSource<i32> for FailingStartSource {
            fn start(
                &mut self,
                _controller: &mut ReadableStreamDefaultController<i32>,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                async { Err("Start initialization failed".into()) }
            }

            fn pull(
                &mut self,
                _controller: &mut ReadableStreamDefaultController<i32>,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                async { Ok(()) }
            }
        }

        let source = FailingStartSource;
        let stream = ReadableStream::builder(source).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked_stream, reader) = stream.get_reader().unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let read_result = reader.read().await;
        assert!(read_result.is_err());

        if let Err(err) = read_result {
            assert_eq!(err.to_string(), "Start initialization failed");
        }

        let read_result2 = reader.read().await;
        assert!(read_result2.is_err());

        let closed_result = reader.closed().await;
        assert!(closed_result.is_err());

        if let Err(err) = closed_result {
            assert_eq!(err.to_string(), "Start initialization failed");
        }
    }

    #[tokio_localset_test::localset_test]
    async fn start_blocks_read_operations() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::Barrier;

        struct SlowStartByteSource {
            data: Vec<u8>,
            start_barrier: SharedPtr<Barrier>,
            start_completed: SharedPtr<AtomicBool>,
        }

        impl ReadableByteSource for SlowStartByteSource {
            async fn start(
                &mut self,
                _controller: &mut ReadableByteStreamController,
            ) -> StreamResult<()> {
                self.start_barrier.wait().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                self.start_completed.store(true, Ordering::Release);
                Ok(())
            }

            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
            ) -> StreamResult<()> {
                assert!(
                    self.start_completed.load(Ordering::Acquire),
                    "pull called before start completed"
                );

                if !self.data.is_empty() {
                    let chunk = self.data.clone();
                    self.data.clear();
                    controller.enqueue(chunk)?;
                } else {
                    controller.close()?;
                }
                Ok(())
            }
        }

        let barrier = SharedPtr::new(Barrier::new(2));
        let start_completed = SharedPtr::new(AtomicBool::new(false));

        let source = SlowStartByteSource {
            data: b"delayed data".to_vec(),
            start_barrier: SharedPtr::clone(&barrier),
            start_completed: SharedPtr::clone(&start_completed),
        };

        let stream = ReadableStream::builder_bytes(source).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = stream.get_reader().unwrap();

        let read_future = reader.read();
        assert!(!start_completed.load(Ordering::Acquire));

        barrier.wait().await;

        let result = timeout(Duration::from_millis(500), read_future)
            .await
            .expect("Read should complete after start finishes")
            .unwrap();

        assert!(
            result.is_some(),
            "Should receive data after start completes"
        );
        assert!(start_completed.load(Ordering::Acquire));
    }
}

#[cfg(test)]
mod pipe_to_tests {
    use super::super::writable::WritableStreamDefaultController;
    use super::*;
    use parking_lot::Mutex;

    #[derive(Clone)]
    struct CountingSink {
        written: SharedPtr<Mutex<Vec<Vec<u8>>>>,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                written: SharedPtr::new(Mutex::new(Vec::new())),
            }
        }

        fn get_written(&self) -> Vec<Vec<u8>> {
            self.written.lock().clone()
        }
    }

    impl WritableSink<Vec<u8>> for CountingSink {
        fn write(
            &mut self,
            chunk: Vec<u8>,
            _controller: &mut WritableStreamDefaultController,
        ) -> impl std::future::Future<Output = StreamResult<()>> {
            let written = self.written.clone();
            async move {
                written.lock().push(chunk);
                Ok(())
            }
        }
    }

    #[tokio_localset_test::localset_test]
    async fn pipes_data_from_readable_to_writable() {
        let data = vec![vec![1u8, 2, 3], vec![4u8, 5], vec![6u8]];
        let readable =
            ReadableStream::from_iterator(data.clone().into_iter()).spawn(tokio::task::spawn_local);

        let sink = CountingSink::new();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(|fut| {
                tokio::task::spawn_local(fut);
            });

        readable
            .pipe_to(&writable, None)
            .await
            .expect("pipe_to failed");

        let written = sink.get_written();
        assert_eq!(written, data);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_destination_write_errors() {
        #[derive(Clone)]
        struct FailingWriteSink {
            written: SharedPtr<Mutex<Vec<Vec<u8>>>>,
            fail_after: usize,
            write_count: SharedPtr<Mutex<usize>>,
            abort_called: SharedPtr<Mutex<bool>>,
        }

        impl FailingWriteSink {
            fn new(fail_after: usize) -> Self {
                Self {
                    written: SharedPtr::new(Mutex::new(Vec::new())),
                    fail_after,
                    write_count: SharedPtr::new(Mutex::new(0)),
                    abort_called: SharedPtr::new(Mutex::new(false)),
                }
            }

            fn get_written(&self) -> Vec<Vec<u8>> {
                self.written.lock().clone()
            }
        }

        impl WritableSink<Vec<u8>> for FailingWriteSink {
            fn write(
                &mut self,
                chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let written = SharedPtr::clone(&self.written);
                let write_count = SharedPtr::clone(&self.write_count);
                let fail_after = self.fail_after;

                async move {
                    let mut count = write_count.lock();
                    *count += 1;

                    if *count > fail_after {
                        return Err("Write failed".into());
                    }

                    written.lock().push(chunk);
                    Ok(())
                }
            }

            fn abort(
                &mut self,
                _reason: Option<String>,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let abort_called = self.abort_called.clone();
                async move {
                    *abort_called.lock() = true;
                    Ok(())
                }
            }
        }

        struct TrackingSource {
            data: Vec<Vec<u8>>,
            index: usize,
            cancelled: SharedPtr<Mutex<bool>>,
        }

        impl TrackingSource {
            fn new(data: Vec<Vec<u8>>) -> Self {
                Self {
                    data,
                    index: 0,
                    cancelled: SharedPtr::new(Mutex::new(false)),
                }
            }
        }

        impl ReadableSource<Vec<u8>> for TrackingSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<Vec<u8>>,
            ) -> StreamResult<()> {
                if self.index >= self.data.len() {
                    controller.close()?;
                    return Ok(());
                }

                controller.enqueue(self.data[self.index].clone())?;
                self.index += 1;
                Ok(())
            }

            async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
                *self.cancelled.lock() = true;
                Ok(())
            }
        }

        let data = vec![
            vec![1u8, 2, 3],
            vec![4u8, 5, 6],
            vec![7u8, 8, 9],
            vec![10u8, 11],
        ];

        let source = TrackingSource::new(data.clone());
        let cancelled_flag = SharedPtr::clone(&source.cancelled);
        let readable = ReadableStream::builder(source).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });

        let sink = FailingWriteSink::new(2);
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(tokio::task::spawn_local);

        let pipe_result = readable.pipe_to(&writable, None).await;
        assert!(
            pipe_result.is_err(),
            "pipe_to should fail when write errors"
        );

        // Note: the source may or may not be cancelled depending on timing.
        // If the source finishes delivering all chunks before the write error
        // is detected, the pipe follows the "source completed" path (Ok(None))
        // and attempts writer.close() which fails. In that case, the source
        // is not explicitly cancelled since it already completed naturally.

        let written = sink.get_written();
        assert!(
            written.len() <= 2,
            "At most 2 chunks should have been written successfully, got {}",
            written.len()
        );
    }

    #[tokio_localset_test::localset_test]
    async fn handles_source_read_errors() {
        struct ErroringSource {
            data: Vec<Vec<u8>>,
            index: usize,
            error_after: usize,
        }

        impl ErroringSource {
            fn new(data: Vec<Vec<u8>>, error_after: usize) -> Self {
                Self {
                    data,
                    index: 0,
                    error_after,
                }
            }
        }

        impl ReadableSource<Vec<u8>> for ErroringSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<Vec<u8>>,
            ) -> StreamResult<()> {
                if self.index >= self.error_after {
                    return Err("Source error".into());
                }

                if self.index >= self.data.len() {
                    controller.close()?;
                    return Ok(());
                }

                controller.enqueue(self.data[self.index].clone())?;
                self.index += 1;
                Ok(())
            }
        }

        #[derive(Clone)]
        struct TrackingSink {
            written: SharedPtr<Mutex<Vec<Vec<u8>>>>,
            abort_called: SharedPtr<Mutex<bool>>,
            abort_reason: SharedPtr<Mutex<Option<String>>>,
        }

        impl TrackingSink {
            fn new() -> Self {
                Self {
                    written: SharedPtr::new(Mutex::new(Vec::new())),
                    abort_called: SharedPtr::new(Mutex::new(false)),
                    abort_reason: SharedPtr::new(Mutex::new(None)),
                }
            }

            fn get_written(&self) -> Vec<Vec<u8>> {
                self.written.lock().clone()
            }

            fn was_abort_called(&self) -> bool {
                *self.abort_called.lock()
            }

            fn get_abort_reason(&self) -> Option<String> {
                self.abort_reason.lock().clone()
            }
        }

        impl WritableSink<Vec<u8>> for TrackingSink {
            fn write(
                &mut self,
                chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let written = SharedPtr::clone(&self.written);
                async move {
                    written.lock().push(chunk);
                    Ok(())
                }
            }

            fn abort(
                &mut self,
                reason: Option<String>,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let abort_called = self.abort_called.clone();
                let abort_reason = self.abort_reason.clone();
                async move {
                    *abort_called.lock() = true;
                    *abort_reason.lock() = reason;
                    Ok(())
                }
            }
        }

        let data = vec![vec![1u8, 2, 3], vec![4u8, 5, 6]];

        let source = ErroringSource::new(data.clone(), 2);
        let readable = ReadableStream::builder(source).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });

        let sink = TrackingSink::new();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(tokio::task::spawn_local);

        let pipe_result = readable.pipe_to(&writable, None).await;
        assert!(
            pipe_result.is_err(),
            "pipe_to should fail when source errors"
        );

        assert!(
            sink.was_abort_called(),
            "Destination should be aborted when source errors"
        );

        let written = sink.get_written();
        assert_eq!(written, data, "All chunks before error should be written");

        let abort_reason = sink.get_abort_reason();
        assert!(
            abort_reason.is_some() && abort_reason.unwrap().contains("Source error"),
            "Abort reason should contain source error information"
        );
    }

    #[tokio_localset_test::localset_test]
    async fn respects_prevent_options() {
        #[derive(Clone)]
        struct TestSource {
            data: Vec<Vec<u8>>,
            index: SharedPtr<Mutex<usize>>,
            cancelled: SharedPtr<Mutex<bool>>,
            should_error: bool,
            error_after: usize,
        }

        impl TestSource {
            fn new(data: Vec<Vec<u8>>) -> Self {
                Self {
                    data,
                    index: SharedPtr::new(Mutex::new(0)),
                    cancelled: SharedPtr::new(Mutex::new(false)),
                    should_error: false,
                    error_after: 0,
                }
            }

            fn with_error_after(mut self, count: usize) -> Self {
                self.should_error = true;
                self.error_after = count;
                self
            }

            fn was_cancelled(&self) -> bool {
                *self.cancelled.lock()
            }
        }

        impl ReadableSource<Vec<u8>> for TestSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<Vec<u8>>,
            ) -> StreamResult<()> {
                let mut idx = self.index.lock();

                if self.should_error && *idx >= self.error_after {
                    return Err("Source error".into());
                }

                if *idx >= self.data.len() {
                    controller.close()?;
                    return Ok(());
                }

                controller.enqueue(self.data[*idx].clone())?;
                *idx += 1;
                Ok(())
            }

            async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
                *self.cancelled.lock() = true;
                Ok(())
            }
        }

        #[derive(Clone)]
        struct TestSink {
            written: SharedPtr<Mutex<Vec<Vec<u8>>>>,
            aborted: SharedPtr<Mutex<bool>>,
            closed: SharedPtr<Mutex<bool>>,
            should_error: bool,
        }

        impl TestSink {
            fn new() -> Self {
                Self {
                    written: SharedPtr::new(Mutex::new(Vec::new())),
                    aborted: SharedPtr::new(Mutex::new(false)),
                    closed: SharedPtr::new(Mutex::new(false)),
                    should_error: false,
                }
            }

            fn with_error(mut self) -> Self {
                self.should_error = true;
                self
            }

            fn was_aborted(&self) -> bool {
                *self.aborted.lock()
            }

            fn was_closed(&self) -> bool {
                *self.closed.lock()
            }

            fn written_data(&self) -> Vec<Vec<u8>> {
                self.written.lock().clone()
            }
        }

        impl WritableSink<Vec<u8>> for TestSink {
            fn write(
                &mut self,
                chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let written = self.written.clone();
                let should_error = self.should_error;

                async move {
                    if should_error {
                        return Err("Sink error".into());
                    }
                    written.lock().push(chunk);
                    Ok(())
                }
            }

            fn abort(
                &mut self,
                _reason: Option<String>,
            ) -> impl std::future::Future<Output = StreamResult<()>> {
                let aborted = self.aborted.clone();
                async move {
                    *aborted.lock() = true;
                    Ok(())
                }
            }

            fn close(self) -> impl std::future::Future<Output = StreamResult<()>> {
                let closed = self.closed;
                async move {
                    *closed.lock() = true;
                    Ok(())
                }
            }
        }

        // Test successful pipe with normal cleanup
        let data = vec![vec![1u8, 2, 3], vec![4u8, 5, 6]];
        let source = TestSource::new(data.clone());
        let readable = ReadableStream::builder(source.clone()).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let sink = TestSink::new();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(tokio::task::spawn_local);

        let result = readable.pipe_to(&writable, None).await;
        assert!(result.is_ok(), "Normal pipe should succeed");
        assert!(
            !source.was_cancelled(),
            "Source should not be cancelled on success"
        );
        assert!(sink.was_closed(), "Sink should be closed on success");
        assert!(!sink.was_aborted(), "Sink should not be aborted on success");
        assert_eq!(sink.written_data(), data, "All data should be written");

        // Test prevent_close = true
        let data = vec![vec![7u8, 8, 9]];
        let source = TestSource::new(data.clone());
        let readable = ReadableStream::builder(source).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let sink = TestSink::new();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(tokio::task::spawn_local);

        let options = StreamPipeOptions {
            prevent_close: true,
            ..Default::default()
        };

        let result = readable.pipe_to(&writable, Some(options)).await;
        assert!(result.is_ok(), "Pipe with prevent_close should succeed");
        assert!(
            !sink.was_closed(),
            "Sink should NOT be closed when prevent_close=true"
        );
    }
}

#[cfg(test)]
mod tee_tests {
    use super::*;
    use std::time::Duration;

    #[tokio_localset_test::localset_test]
    async fn splits_stream_into_two_identical_branches() {
        let data = vec![1, 2, 3, 4];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let (stream1, stream2) = source_stream.tee().spawn(tokio::task::spawn_local).unwrap();

        let (_, reader1) = stream1.get_reader().unwrap();
        let (_, reader2) = stream2.get_reader().unwrap();

        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        assert_eq!(reader1.read().await.unwrap(), Some(2));
        assert_eq!(reader2.read().await.unwrap(), Some(2));

        assert_eq!(reader1.read().await.unwrap(), Some(3));
        assert_eq!(reader2.read().await.unwrap(), Some(3));

        assert_eq!(reader1.read().await.unwrap(), Some(4));
        assert_eq!(reader2.read().await.unwrap(), Some(4));

        assert_eq!(reader1.read().await.unwrap(), None);
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_different_consumption_speeds() {
        let data = vec![1, 2, 3];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let (stream1, stream2) = source_stream.tee().spawn(tokio::task::spawn_local).unwrap();
        let (_, reader1) = stream1.get_reader().unwrap();
        let (_, reader2) = stream2.get_reader().unwrap();

        // Reader1 reads everything quickly
        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader1.read().await.unwrap(), Some(2));
        assert_eq!(reader1.read().await.unwrap(), Some(3));
        assert_eq!(reader1.read().await.unwrap(), None);

        // Reader2 can still read all data (buffered by coordinator)
        assert_eq!(reader2.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(2));
        assert_eq!(reader2.read().await.unwrap(), Some(3));
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn continues_when_one_branch_cancels() {
        let data = vec![1, 2, 3, 4];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let (stream1, stream2) = source_stream.tee().spawn(tokio::task::spawn_local).unwrap();
        let (_, reader1) = stream1.get_reader().unwrap();
        let (_, reader2) = stream2.get_reader().unwrap();

        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        reader1
            .cancel(Some("Branch 1 canceled".to_string()))
            .await
            .unwrap();

        // Reader2 should still work
        assert_eq!(reader2.read().await.unwrap(), Some(2));
        assert_eq!(reader2.read().await.unwrap(), Some(3));
        assert_eq!(reader2.read().await.unwrap(), Some(4));
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn stops_when_both_branches_cancel() {
        let data = vec![1, 2, 3, 4];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let (stream1, stream2) = source_stream.tee().spawn(tokio::task::spawn_local).unwrap();
        let (_, reader1) = stream1.get_reader().unwrap();
        let (_, reader2) = stream2.get_reader().unwrap();

        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        reader1
            .cancel(Some("Branch 1 canceled".to_string()))
            .await
            .unwrap();
        reader2
            .cancel(Some("Branch 2 canceled".to_string()))
            .await
            .unwrap();

        // Give coordinator time to process cancellations
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio_localset_test::localset_test]
    async fn handles_empty_source_stream() {
        let data: Vec<i32> = vec![];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let (stream1, stream2) = source_stream.tee().spawn(tokio::task::spawn_local).unwrap();
        let (_, reader1) = stream1.get_reader().unwrap();
        let (_, reader2) = stream2.get_reader().unwrap();

        assert_eq!(reader1.read().await.unwrap(), None);
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn propagates_source_errors_to_both_branches() {
        struct ErrorSource {
            count: i32,
        }

        impl ReadableSource<i32> for ErrorSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<i32>,
            ) -> StreamResult<()> {
                if self.count < 2 {
                    controller.enqueue(self.count)?;
                    self.count += 1;
                    Ok(())
                } else {
                    Err("Test error".into())
                }
            }
        }

        let error_stream =
            ReadableStream::builder(ErrorSource { count: 0 }).spawn(tokio::task::spawn_local);
        let (stream1, stream2) = error_stream.tee().spawn(tokio::task::spawn_local).unwrap();
        let (_, reader1) = stream1.get_reader().unwrap();
        let (_, reader2) = stream2.get_reader().unwrap();

        assert_eq!(reader1.read().await.unwrap(), Some(0));
        assert_eq!(reader2.read().await.unwrap(), Some(0));

        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        let err1 = reader1.read().await.unwrap_err();
        let err2 = reader2.read().await.unwrap_err();

        assert_eq!(err1.to_string(), "Test error");
        assert_eq!(err2.to_string(), "Test error");
    }

    #[tokio_localset_test::localset_test]
    async fn supports_nested_tee_operations() {
        let data = vec![1, 2];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let (stream1, stream2) = source_stream.tee().spawn(tokio::task::spawn_local).unwrap();
        let (stream1a, stream1b) = stream1.tee().spawn(tokio::task::spawn_local).unwrap();

        let (_, reader1a) = stream1a.get_reader().unwrap();
        let (_, reader1b) = stream1b.get_reader().unwrap();
        let (_, reader2) = stream2.get_reader().unwrap();

        assert_eq!(reader1a.read().await.unwrap(), Some(1));
        assert_eq!(reader1b.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        assert_eq!(reader1a.read().await.unwrap(), Some(2));
        assert_eq!(reader1b.read().await.unwrap(), Some(2));
        assert_eq!(reader2.read().await.unwrap(), Some(2));

        assert_eq!(reader1a.read().await.unwrap(), None);
        assert_eq!(reader1b.read().await.unwrap(), None);
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn works_with_different_data_types() {
        let data = vec!["hello".to_string(), "world".to_string()];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let (stream1, stream2) = source_stream.tee().spawn(tokio::task::spawn_local).unwrap();
        let (_, reader1) = stream1.get_reader().unwrap();
        let (_, reader2) = stream2.get_reader().unwrap();

        assert_eq!(reader1.read().await.unwrap(), Some("hello".to_string()));
        assert_eq!(reader2.read().await.unwrap(), Some("hello".to_string()));

        assert_eq!(reader1.read().await.unwrap(), Some("world".to_string()));
        assert_eq!(reader2.read().await.unwrap(), Some("world".to_string()));

        assert_eq!(reader1.read().await.unwrap(), None);
        assert_eq!(reader2.read().await.unwrap(), None);
    }
}

#[cfg(test)]
mod pipe_through_tests {
    use super::super::transform::*;
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    struct UppercaseTransformer;

    impl Transformer<String, String> for UppercaseTransformer {
        fn transform(
            &mut self,
            chunk: String,
            controller: &mut TransformStreamDefaultController<String>,
        ) -> impl Future<Output = StreamResult<()>> {
            let result = controller.enqueue(chunk.to_uppercase());
            futures::future::ready(result)
        }
    }

    struct DoubleTransformer;

    impl Transformer<i32, i32> for DoubleTransformer {
        fn transform(
            &mut self,
            chunk: i32,
            controller: &mut TransformStreamDefaultController<i32>,
        ) -> impl Future<Output = StreamResult<()>> {
            let result = controller.enqueue(chunk * 2);
            futures::future::ready(result)
        }
    }

    #[tokio_localset_test::localset_test]
    async fn transforms_data_through_pipe() {
        let data = vec!["hello".to_string(), "world".to_string()];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let transform =
            TransformStream::builder(UppercaseTransformer).spawn(tokio::task::spawn_local);

        let result_stream = source_stream
            .pipe_through(transform, None)
            .spawn(tokio::task::spawn_local);
        let (_locked, reader) = result_stream.get_reader().unwrap();

        let result1 = timeout(Duration::from_secs(1), reader.read())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result1, Some("HELLO".to_string()));

        let result2 = timeout(Duration::from_secs(1), reader.read())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result2, Some("WORLD".to_string()));

        let result3 = timeout(Duration::from_secs(1), reader.read())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result3, None);
    }

    #[tokio_localset_test::localset_test]
    async fn transforms_numeric_data() {
        let data = vec![1, 2, 3, 4];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let transform = TransformStream::builder(DoubleTransformer).spawn(tokio::task::spawn_local);
        let result_stream = source_stream.pipe_through(transform, None).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = result_stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert_eq!(reader.read().await.unwrap(), Some(4));
        assert_eq!(reader.read().await.unwrap(), Some(6));
        assert_eq!(reader.read().await.unwrap(), Some(8));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_empty_input_stream() {
        let data: Vec<String> = vec![];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let transform =
            TransformStream::builder(UppercaseTransformer).spawn(tokio::task::spawn_local);
        let result_stream = source_stream.pipe_through(transform, None).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = result_stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn chains_multiple_transformations() {
        let data = vec![1, 2, 3];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let transform1 =
            TransformStream::builder(DoubleTransformer).spawn(tokio::task::spawn_local);
        let intermediate_stream = source_stream.pipe_through(transform1, None).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });

        let transform2 =
            TransformStream::builder(DoubleTransformer).spawn(tokio::task::spawn_local);
        let result_stream = intermediate_stream
            .pipe_through(transform2, None)
            .spawn(|fut| {
                tokio::task::spawn_local(fut);
            });
        let (_locked, reader) = result_stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some(4)); // 1 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), Some(8)); // 2 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), Some(12)); // 3 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn respects_pipe_options() {
        let data = vec!["test".to_string()];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let transform =
            TransformStream::builder(UppercaseTransformer).spawn(tokio::task::spawn_local);

        let options = StreamPipeOptions {
            prevent_close: false,
            prevent_abort: false,
            prevent_cancel: false,
            signal: None,
        };

        let result_stream = source_stream
            .pipe_through(transform, Some(options))
            .spawn(|fut| {
                tokio::task::spawn_local(fut);
            });
        let (_locked, reader) = result_stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some("TEST".to_string()));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn handles_transformation_errors() {
        struct ErrorTransformer;

        impl Transformer<i32, i32> for ErrorTransformer {
            fn transform(
                &mut self,
                chunk: i32,
                controller: &mut TransformStreamDefaultController<i32>,
            ) -> impl Future<Output = StreamResult<()>> {
                if chunk == 3 {
                    futures::future::ready(Err("Error on 3".into()))
                } else {
                    let result = controller.enqueue(chunk);
                    futures::future::ready(result)
                }
            }
        }

        let data = vec![1, 2, 3, 4];
        let source_stream =
            ReadableStream::from_iterator(data.into_iter()).spawn(tokio::task::spawn_local);

        let transform = TransformStream::builder(ErrorTransformer).spawn(tokio::task::spawn_local);
        let result_stream = source_stream.pipe_through(transform, None).spawn(|fut| {
            tokio::task::spawn_local(fut);
        });
        let (_locked, reader) = result_stream.get_reader().unwrap();

        match reader.read().await {
            Ok(Some(v)) => assert_eq!(v, 1),
            _ => panic!("Expected first value"),
        }

        match reader.read().await {
            Ok(Some(v)) => assert_eq!(v, 2),
            Err(e) => assert_eq!(e.to_string(), "Error on 3"),
            Ok(None) => panic!("Expected value or error, got end of stream"),
        }

        let read_result = reader.read().await;
        assert!(read_result.is_err());
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    pub struct TestSource {
        pub data: Vec<String>,
        pub index: usize,
    }

    impl TestSource {
        pub fn new(data: Vec<String>) -> Self {
            Self { data, index: 0 }
        }
    }

    impl ReadableSource<String> for TestSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<String>,
        ) -> Result<(), StreamError> {
            if self.index < self.data.len() {
                let item = self.data[self.index].clone();
                self.index += 1;
                controller.enqueue(item)?;
            } else {
                controller.close()?;
            }
            Ok(())
        }
    }

    #[tokio_localset_test::localset_test]
    async fn builder_spawn_creates_working_stream() {
        let source = TestSource::new(vec!["hello".to_string(), "world".to_string()]);
        let stream = ReadableStream::builder(source).spawn(tokio::task::spawn_local);
        let (_, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some("hello".to_string()));
        assert_eq!(reader.read().await.unwrap(), Some("world".to_string()));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn builder_prepare_allows_manual_task_spawn() {
        let source = TestSource::new(vec!["test".to_string()]);
        let (stream, fut) = ReadableStream::builder(source).prepare();

        tokio::task::spawn_local(fut);

        let (_, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some("test".to_string()));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    fn spawn_local_fn(fut: crate::platform::PlatformFuture<'static, ()>) {
        tokio::task::spawn_local(fut);
    }

    #[tokio_localset_test::localset_test]
    async fn builder_spawn_ref_works_with_function_pointer() {
        let source = TestSource::new(vec!["reference".to_string()]);
        let stream = ReadableStream::builder(source).spawn_ref(&spawn_local_fn);
        let (_, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some("reference".to_string()));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn builder_accepts_custom_strategy() {
        let source = TestSource::new(vec!["custom".to_string()]);
        let custom_strategy = CountQueuingStrategy::new(5);
        let stream = ReadableStream::builder(source)
            .strategy(custom_strategy)
            .spawn(tokio::task::spawn_local);

        let (_, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some("custom".to_string()));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn builds_from_vec_data() {
        let data = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let stream = ReadableStreamBuilder::from_vec(data).spawn(tokio::task::spawn_local);
        let (_, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some("a".to_string()));
        assert_eq!(reader.read().await.unwrap(), Some("b".to_string()));
        assert_eq!(reader.read().await.unwrap(), Some("c".to_string()));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn builds_from_iterator_data() {
        let numbers = vec![1, 2, 3];
        let stream = ReadableStreamBuilder::from_iterator(numbers.into_iter())
            .spawn(tokio::task::spawn_local);
        let (_, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert_eq!(reader.read().await.unwrap(), Some(3));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio_localset_test::localset_test]
    async fn builds_from_async_stream() {
        let async_stream = futures::stream::iter(vec!["x", "y", "z"]);
        let stream =
            ReadableStreamBuilder::from_stream(async_stream).spawn(tokio::task::spawn_local);
        let (_, reader) = stream.get_reader().unwrap();

        assert_eq!(reader.read().await.unwrap(), Some("x"));
        assert_eq!(reader.read().await.unwrap(), Some("y"));
        assert_eq!(reader.read().await.unwrap(), Some("z"));
        assert_eq!(reader.read().await.unwrap(), None);
    }
}
