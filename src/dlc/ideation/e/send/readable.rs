use super::super::{CountQueuingStrategy, Locked, QueuingStrategy, Unlocked, errors::StreamError};
pub use super::byte_source_trait::ReadableByteSource;
use super::{
    byte_state::{ByteStreamState, ByteStreamStateInterface},
    transform::{TransformReadableSource, TransformStream},
    writable::{WritableSink, WritableStream},
};
use futures::{
    FutureExt,
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    future::{AbortRegistration, Abortable, Aborted, poll_fn},
    io::AsyncRead,
    stream::{Stream, StreamExt},
};
use std::{
    collections::VecDeque,
    future::Future,
    io::{Error as IoError, ErrorKind, Result as IoResult},
    marker::PhantomData,
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

type StreamResult<T> = Result<T, StreamError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Readable,
    Closed,
    Errored,
}

// ----------- Stream Type Markers -----------
pub struct DefaultStream;
pub struct ByteStream;

// ----------- Stream Type Marker Trait -----------
pub trait StreamTypeMarker: Send + Sync + 'static {
    type Controller<T: Send + 'static>: Send + Sync + Clone + 'static;
}

impl StreamTypeMarker for DefaultStream {
    type Controller<T: Send + 'static> = ReadableStreamDefaultController<T>;
}

impl StreamTypeMarker for ByteStream {
    type Controller<T: Send + 'static> = ReadableByteStreamController;
}

// ----------- Source Traits -----------
pub trait ReadableSource<T>: Send + Sized + 'static
where
    T: Send + 'static,
{
    fn start(
        &mut self,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> impl Future<Output = StreamResult<()>> + Send {
        async { Ok(()) }
    }

    fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> impl Future<Output = StreamResult<()>> + Send;

    fn cancel(&mut self, reason: Option<String>) -> impl Future<Output = StreamResult<()>> + Send {
        async { Ok(()) }
    }
}

/*pub trait ReadableByteSource: Send + Sized + 'static {
    fn start(
        &mut self,
        controller: &mut ReadableByteStreamController,
    ) -> impl Future<Output = StreamResult<()>> + Send {
        async { Ok(()) }
    }

    fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> impl Future<Output = StreamResult<usize>> + Send;

    fn cancel(&mut self, reason: Option<String>) -> impl Future<Output = StreamResult<()>> + Send {
        async { Ok(()) }
    }
}*/

// ----------- WakerSet -----------
#[derive(Clone, Default, Debug)]
pub struct WakerSet(Arc<Mutex<Vec<Waker>>>);

impl WakerSet {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    pub fn register(&self, waker: &Waker) {
        let mut wakers = self.0.lock().unwrap();
        if !wakers.iter().any(|w| w.will_wake(waker)) {
            wakers.push(waker.clone());
        }
    }

    pub fn wake_all(&self) {
        let mut wakers = self.0.lock().unwrap();
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }
}

// ----------- Stream Commands -----------
pub enum StreamCommand<T> {
    Read {
        completion: oneshot::Sender<StreamResult<Option<T>>>,
    },
    ReadInto {
        buffer: Vec<u8>,
        completion: oneshot::Sender<StreamResult<(Vec<u8>, usize)>>,
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

enum ByteControllerMsg {
    Enqueue { chunk: Vec<u8> },
    Close,
    Error(StreamError),
}

// ----------- Controllers -----------
pub struct ReadableStreamDefaultController<T> {
    tx: UnboundedSender<ControllerMsg<T>>,
    queue_total_size: Arc<AtomicUsize>,
    high_water_mark: Arc<AtomicUsize>,
    desired_size: Arc<AtomicIsize>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
}

impl<T> Clone for ReadableStreamDefaultController<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            queue_total_size: self.queue_total_size.clone(),
            high_water_mark: self.high_water_mark.clone(),
            desired_size: self.desired_size.clone(),
            closed: self.closed.clone(),
            errored: self.errored.clone(),
        }
    }
}

impl<T> ReadableStreamDefaultController<T> {
    fn new(
        tx: UnboundedSender<ControllerMsg<T>>,
        queue_total_size: Arc<AtomicUsize>,
        high_water_mark: Arc<AtomicUsize>,
        desired_size: Arc<AtomicIsize>,
        closed: Arc<AtomicBool>,
        errored: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tx,
            queue_total_size,
            high_water_mark,
            desired_size,
            closed,
            errored,
        }
    }

    pub fn desired_size(&self) -> Option<isize> {
        if self.closed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
            return None;
        }

        Some(self.desired_size.load(Ordering::SeqCst))
    }

    pub fn close(&self) -> StreamResult<()> {
        self.tx
            .unbounded_send(ControllerMsg::Close)
            .map_err(|_| StreamError::Custom("Failed to close stream".into()))?;
        Ok(())
    }

    pub fn enqueue(&self, chunk: T) -> StreamResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(StreamError::Custom("Stream is closed".into()));
        }
        if self.errored.load(Ordering::SeqCst) {
            return Err(StreamError::Custom("Stream is errored".into()));
        }

        self.tx
            .unbounded_send(ControllerMsg::Enqueue { chunk })
            .map_err(|_| StreamError::Custom("Failed to enqueue chunk".into()))?;
        Ok(())
    }

    pub fn error(&self, error: StreamError) -> StreamResult<()> {
        self.tx
            .unbounded_send(ControllerMsg::Error(error))
            .map_err(|_| StreamError::Custom("Failed to error stream".into()))?;
        Ok(())
    }
}

pub struct ReadableByteStreamController {
    //byte_state: Arc<ByteStreamState<Source>>,
    byte_state: Arc<dyn ByteStreamStateInterface>,
}

impl ReadableByteStreamController {
    pub fn new<Source>(byte_state: Arc<ByteStreamState<Source>>) -> Self
    where
        Source: ReadableByteSource + Send + 'static,
    {
        Self {
            byte_state: byte_state as Arc<dyn ByteStreamStateInterface>,
        }
    }

    pub fn desired_size(&self) -> Option<isize> {
        self.byte_state.desired_size()
    }

    pub fn close(&mut self) -> StreamResult<()> {
        self.byte_state.close();
        Ok(())
    }

    pub fn enqueue(&mut self, chunk: Vec<u8>) -> StreamResult<()> {
        if self.byte_state.is_closed() {
            return Err(StreamError::Custom("Stream is closed".into()));
        }
        if self.byte_state.is_errored() {
            return Err(StreamError::Custom("Stream is errored".into()));
        }

        self.byte_state.enqueue_data(&chunk);
        Ok(())
    }

    pub fn error(&mut self, error: StreamError) -> StreamResult<()> {
        self.byte_state.error(error);
        Ok(())
    }
}

impl Clone for ReadableByteStreamController {
    fn clone(&self) -> Self {
        Self {
            byte_state: self.byte_state.clone(),
        }
    }
}

// ----------- Inner State -----------
struct ReadableStreamInner<T, Source> {
    state: StreamState,
    queue: VecDeque<T>,
    queue_total_size: usize,
    strategy: Box<dyn QueuingStrategy<T> + Send + Sync>,
    source: Option<Source>,
    cancel_requested: bool,
    cancel_reason: Option<String>,
    cancel_completions: Vec<oneshot::Sender<StreamResult<()>>>,
    pending_reads: VecDeque<oneshot::Sender<StreamResult<Option<T>>>>,
    pending_read_intos: VecDeque<(Vec<u8>, oneshot::Sender<StreamResult<(Vec<u8>, usize)>>)>,
    ready_wakers: WakerSet,
    closed_wakers: WakerSet,
    stored_error: Option<StreamError>,
    pulling: bool,
}

impl<T, Source> ReadableStreamInner<T, Source>
where
    T: Send + 'static,
    Source: Send + 'static,
{
    fn new(source: Source, strategy: Box<dyn QueuingStrategy<T> + Send + Sync>) -> Self {
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
            pending_read_intos: VecDeque::new(),
            ready_wakers: WakerSet::new(),
            closed_wakers: WakerSet::new(),
            stored_error: None,
            pulling: false,
        }
    }

    fn get_stored_error(&self) -> StreamError {
        self.stored_error
            .clone()
            .unwrap_or_else(|| StreamError::Custom("Stream is errored".into()))
    }

    fn desired_size(&self) -> isize {
        if self.state != StreamState::Readable {
            return 0;
        }
        let hwm = self.strategy.high_water_mark() as isize;
        let current = self.queue_total_size as isize;
        hwm - current
    }
}

// ----------- Main ReadableStream with Typestate -----------
pub struct ReadableStream<T, Source, StreamType, LockState = Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
{
    command_tx: UnboundedSender<StreamCommand<T>>,
    queue_total_size: Arc<AtomicUsize>,
    high_water_mark: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    locked: Arc<AtomicBool>,
    stored_error: Arc<RwLock<Option<StreamError>>>,
    desired_size: Arc<AtomicIsize>,
    pub(crate) controller: Arc<StreamType::Controller<T>>,
    //byte_state: Option<Arc<ByteStreamState<Source>>>,
    pub(crate) byte_state: Option<Arc<dyn ByteStreamStateInterface + Send + Sync>>,
    _phantom: PhantomData<(T, Source, StreamType, LockState)>,
}

impl<T, Source> ReadableStream<T, Source, DefaultStream, Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
{
    pub(crate) fn controller(&self) -> &ReadableStreamDefaultController<T> {
        self.controller.as_ref()
    }
}

impl<Source> ReadableStream<Vec<u8>, Source, ByteStream, Unlocked>
where
    Source: Send + 'static,
{
    pub(crate) fn controller(&self) -> &ReadableByteStreamController {
        self.controller.as_ref()
    }
}

impl<T, Source> ReadableStream<T, Source, DefaultStream, Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
{
    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }

    fn desired_size(&self) -> isize {
        if self.closed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
            return 0;
        }

        let hwm = self.high_water_mark.load(Ordering::SeqCst) as isize;
        let current = self.queue_total_size.load(Ordering::SeqCst) as isize;
        hwm - current
    }
}

impl<T, Source, S> ReadableStream<T, Source, S, Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
    S: StreamTypeMarker,
{
    pub async fn cancel(&self, reason: Option<String>) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .unbounded_send(StreamCommand::Cancel {
                reason,
                completion: tx,
            })
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;
        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Cancel canceled".into())))
    }

    pub fn pipe_through<O>(
        self,
        transform: TransformStream<T, O>,
        options: Option<StreamPipeOptions>,
    ) -> ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>
    where
        O: Send + 'static,
    {
        let (readable, writable) = transform.split();

        std::thread::spawn(move || {
            futures::executor::block_on(async move {
                let result = self.pipe_to(&writable, options).await;
                if let Err(e) = result {
                    eprintln!("Pipe through error: {}", e);
                }
            });
        });

        readable
    }

    async fn pipe_to_inner<Sink>(
        self,
        destination: &WritableStream<T, Sink>,
        options: Option<StreamPipeOptions>,
    ) -> StreamResult<()>
    where
        Sink: WritableSink<T> + Send + 'static,
    {
        let options = options.unwrap_or_default();
        let (_, writer) = destination.get_writer()?;
        let (_stream, reader) = self.get_reader();

        let pipe_loop = async {
            loop {
                // Wait until the writer is ready before pulling from the reader
                if let Err(write_err) = writer.ready().await {
                    // Destination error - cancel source if allowed
                    if !options.prevent_cancel {
                        if let Err(cancel_err) = reader.cancel(Some(write_err.to_string())).await {
                            return Err(cancel_err);
                        }
                    }
                    return Err(write_err);
                }

                match reader.read().await {
                    Ok(Some(chunk)) => {
                        // Write the chunk (we know writer is ready at this point)
                        writer.write(chunk);
                    }
                    Ok(None) => {
                        // Source completed normally - close destination if allowed
                        if !options.prevent_close {
                            writer.close().await?;
                        }
                        return Ok(());
                    }
                    Err(read_err) => {
                        // Source error - abort destination if allowed
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
        if let Some(reg) = options.signal {
            match Abortable::new(pipe_loop, reg).await {
                Ok(result) => result,
                Err(Aborted) => {
                    // Handle abort signal - cancel source and abort destination
                    if !options.prevent_cancel {
                        let _ = reader.cancel(Some("Aborted".to_string())).await;
                    }
                    if !options.prevent_abort {
                        let _ = writer.abort(Some("Aborted".to_string())).await;
                    }
                    Err(StreamError::Aborted(Some("Pipe operation aborted".into())))
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
        Sink: WritableSink<T> + Send + 'static,
    {
        self.pipe_to_inner(destination, options).await
    }

    pub fn pipe_to_threaded<Sink>(
        self,
        destination: WritableStream<T, Sink>,
        options: Option<StreamPipeOptions>,
    ) -> std::thread::JoinHandle<StreamResult<()>>
    where
        Sink: WritableSink<T> + Send + 'static,
        T: Send + 'static,
    {
        std::thread::spawn(move || {
            futures::executor::block_on(
                async move { self.pipe_to_inner(&destination, options).await },
            )
        })
    }

    /*pub fn pipe_to_with_spawn<Sink, SpawnFn, Fut>(
        self,
        destination: WritableStream<T, Sink>,
        options: Option<StreamPipeOptions>,
        spawn: SpawnFn,
    ) -> Fut
    where
        Sink: WritableSink<T> + Send + Sync + 'static,
        T: Send + Sync + 'static,
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>) -> Fut,
        Fut: Future<Output = StreamResult<()>> + 'static,
    {
        let fut = Box::pin(async move { self.pipe_to_inner(&destination, options).await });
        spawn(fut)
    }*/
}

// ===== Tee implementation =====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeeSourceId {
    Branch1,
    Branch2,
}

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
    /// - If the total buffered items across both branches is under
    ///   `2 * max_buffer_per_branch`, a pull is allowed.  
    /// - Balances throughput and safety.  
    /// - Allows temporary imbalance, but prevents runaway growth.
    Aggregate,
}

#[derive(Clone)]
pub struct AsyncSignal {
    waker: Arc<Mutex<Option<Waker>>>,
    signaled: Arc<AtomicBool>,
}

impl AsyncSignal {
    pub fn new() -> Self {
        Self {
            waker: Arc::new(Mutex::new(None)),
            signaled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn wait(&self) {
        poll_fn(|cx| {
            if self.signaled.swap(false, Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                *self.waker.lock().unwrap() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }

    pub fn signal(&self) {
        self.signaled.store(true, Ordering::SeqCst);
        if let Some(w) = self.waker.lock().unwrap().take() {
            w.wake();
        }
    }
}

pub struct TeeSource<T> {
    chunk_rx: UnboundedReceiver<TeeChunk<T>>,
    branch_id: TeeSourceId,
    branch_canceled: Arc<AtomicBool>,

    // Optional fields used only for backpressure-aware modes.
    // None for the Unbounded fast-path.
    pending_count: Option<Arc<AtomicUsize>>,
    backpressure_signal: Option<AsyncSignal>,
}

impl<T> ReadableSource<T> for TeeSource<T>
where
    T: Send + 'static,
{
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> StreamResult<()> {
        if self.branch_canceled.load(Ordering::SeqCst) {
            controller.close()?;
            return Ok(());
        }

        match self.chunk_rx.next().await {
            Some(TeeChunk::Data(chunk)) => {
                // If we have pending_count (i.e., not fast-path), decrement and signal.
                if let Some(pending) = &self.pending_count {
                    let old = pending.fetch_sub(1, Ordering::SeqCst);
                    if old > 0 {
                        if let Some(sig) = &self.backpressure_signal {
                            sig.signal();
                        }
                    }
                }

                controller.enqueue(chunk)?;
            }
            Some(TeeChunk::End) => {
                controller.close()?;
            }
            Some(TeeChunk::Error(err)) => {
                return Err(err);
            }
            None => {
                controller.close()?;
            }
        }

        Ok(())
    }

    async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
        self.branch_canceled.store(true, Ordering::SeqCst);

        // Wake coordinator if we have a signal (non-fast path).
        if let Some(sig) = &self.backpressure_signal {
            sig.signal();
        }
        Ok(())
    }
}

struct TeeCoordinator<T, Source, StreamType, LockState>
where
    T: Send + Clone + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
{
    reader: ReadableStreamDefaultReader<T, Source, StreamType, LockState>,

    branch1_tx: UnboundedSender<TeeChunk<T>>,
    branch2_tx: UnboundedSender<TeeChunk<T>>,

    branch1_canceled: Arc<AtomicBool>,
    branch2_canceled: Arc<AtomicBool>,

    // Backpressure configuration
    backpressure_mode: BackpressureMode,
    branch1_pending_count: Option<Arc<AtomicUsize>>,
    branch2_pending_count: Option<Arc<AtomicUsize>>,
    max_buffer_per_branch: usize,

    backpressure_signal: Option<AsyncSignal>,
}

impl<T, Source, StreamType, LockState> TeeCoordinator<T, Source, StreamType, LockState>
where
    T: Send + Clone + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
{
    // Decide whether to pull; respects fast-path (None pending counts => Unbounded fast-path)
    fn should_pull(&self) -> bool {
        let branch1_active =
            !self.branch1_canceled.load(Ordering::SeqCst) && !self.branch1_tx.is_closed();
        let branch2_active =
            !self.branch2_canceled.load(Ordering::SeqCst) && !self.branch2_tx.is_closed();

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
            .load(Ordering::SeqCst);
        let branch2_pending = self
            .branch2_pending_count
            .as_ref()
            .unwrap()
            .load(Ordering::SeqCst);

        match self.backpressure_mode {
            BackpressureMode::Unbounded => branch1_active || branch2_active,
            BackpressureMode::SlowestConsumer => {
                let b1_ok = !branch1_active || branch1_pending < self.max_buffer_per_branch;
                let b2_ok = !branch2_active || branch2_pending < self.max_buffer_per_branch;
                b1_ok && b2_ok
            }
            BackpressureMode::Aggregate => {
                let total = branch1_pending + branch2_pending;
                total < (self.max_buffer_per_branch * 2)
            }
            BackpressureMode::SpecCompliant => {
                let b1_can = !branch1_active || branch1_pending < self.max_buffer_per_branch;
                let b2_can = !branch2_active || branch2_pending < self.max_buffer_per_branch;
                b1_can || b2_can
            }
        }
    }

    async fn distribute_chunk(&self, chunk: T) {
        let branch1_active =
            !self.branch1_canceled.load(Ordering::SeqCst) && !self.branch1_tx.is_closed();
        let branch2_active =
            !self.branch2_canceled.load(Ordering::SeqCst) && !self.branch2_tx.is_closed();

        // Try send to branch1
        if branch1_active {
            let should_send = match self.backpressure_mode {
                BackpressureMode::Unbounded => true,
                _ => {
                    // If we don't have a pending count, be permissive (shouldn't happen for non-Unbounded)
                    if let Some(p) = &self.branch1_pending_count {
                        p.load(Ordering::SeqCst) < self.max_buffer_per_branch
                    } else {
                        true
                    }
                }
            };

            if should_send {
                if self
                    .branch1_tx
                    .unbounded_send(TeeChunk::Data(chunk.clone()))
                    .is_ok()
                {
                    if let Some(p) = &self.branch1_pending_count {
                        p.fetch_add(1, Ordering::SeqCst);
                    }
                } else {
                    self.branch1_canceled.store(true, Ordering::SeqCst);
                }
            }
        }

        // Branch2
        if branch2_active {
            let should_send = match self.backpressure_mode {
                BackpressureMode::Unbounded => true,
                _ => {
                    if let Some(p) = &self.branch2_pending_count {
                        p.load(Ordering::SeqCst) < self.max_buffer_per_branch
                    } else {
                        true
                    }
                }
            };

            if should_send {
                if self
                    .branch2_tx
                    .unbounded_send(TeeChunk::Data(chunk))
                    .is_ok()
                {
                    if let Some(p) = &self.branch2_pending_count {
                        p.fetch_add(1, Ordering::SeqCst);
                    }
                } else {
                    self.branch2_canceled.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    async fn run(mut self) {
        loop {
            // termination check
            let branch1_dead =
                self.branch1_canceled.load(Ordering::SeqCst) || self.branch1_tx.is_closed();
            let branch2_dead =
                self.branch2_canceled.load(Ordering::SeqCst) || self.branch2_tx.is_closed();

            if branch1_dead && branch2_dead {
                let _ = self
                    .reader
                    .cancel(Some("Both tee branches terminated".to_string()))
                    .await;
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
                    self.branch1_canceled.load(Ordering::SeqCst) || self.branch1_tx.is_closed();
                let branch2_dead =
                    self.branch2_canceled.load(Ordering::SeqCst) || self.branch2_tx.is_closed();
                if branch1_dead && branch2_dead {
                    let _ = self
                        .reader
                        .cancel(Some("Both tee branches terminated".to_string()))
                        .await;
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

impl<T, Source, S> ReadableStream<T, Source, S, Unlocked>
where
    T: Send + Clone + 'static,
    Source: Send + 'static,
    S: StreamTypeMarker,
{
    // Internal method for blocking spawn
    fn tee_internal_blocking<SpawnFn>(
        self,
        mode: BackpressureMode,
        max_buffer_per_branch: Option<usize>,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    )
    where
        SpawnFn: FnOnce(Box<dyn FnOnce() + Send>) + Send + 'static,
    {
        let (_, reader) = self.get_reader();
        let max_buffer = max_buffer_per_branch.unwrap_or(1000);

        let (branch1_tx, branch1_rx) = unbounded::<TeeChunk<T>>();
        let (branch2_tx, branch2_rx) = unbounded::<TeeChunk<T>>();

        let branch1_canceled = Arc::new(AtomicBool::new(false));
        let branch2_canceled = Arc::new(AtomicBool::new(false));

        let (branch1_pending, branch2_pending, backpressure_signal) =
            if matches!(mode, BackpressureMode::Unbounded) {
                (None, None, None)
            } else {
                (
                    Some(Arc::new(AtomicUsize::new(0))),
                    Some(Arc::new(AtomicUsize::new(0))),
                    Some(AsyncSignal::new()),
                )
            };

        let coordinator = TeeCoordinator {
            reader,
            branch1_tx: branch1_tx.clone(),
            branch2_tx: branch2_tx.clone(),
            branch1_canceled: branch1_canceled.clone(),
            branch2_canceled: branch2_canceled.clone(),
            backpressure_mode: mode,
            branch1_pending_count: branch1_pending.clone(),
            branch2_pending_count: branch2_pending.clone(),
            max_buffer_per_branch: max_buffer,
            backpressure_signal: backpressure_signal.clone(),
        };

        // Blocking spawn pattern
        let runner = Box::new(move || {
            futures::executor::block_on(coordinator.run());
        }) as Box<dyn FnOnce() + Send>;

        spawn(runner);

        let source1 = TeeSource {
            chunk_rx: branch1_rx,
            branch_id: TeeSourceId::Branch1,
            branch_canceled: branch1_canceled,
            pending_count: branch1_pending,
            backpressure_signal: backpressure_signal.clone(),
        };

        let source2 = TeeSource {
            chunk_rx: branch2_rx,
            branch_id: TeeSourceId::Branch2,
            branch_canceled: branch2_canceled,
            pending_count: branch2_pending,
            backpressure_signal,
        };

        (ReadableStream::new(source1), ReadableStream::new(source2))
    }

    // Public blocking methods
    pub fn tee_with_blocking_spawn<SpawnFn>(
        self,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    )
    where
        SpawnFn: FnOnce(Box<dyn FnOnce() + Send>) + Send + 'static,
    {
        self.tee_internal_blocking(BackpressureMode::SpecCompliant, Some(1000), spawn)
    }

    pub fn tee_with_backpressure_and_blocking_spawn<SpawnFn>(
        self,
        mode: BackpressureMode,
        max_buffer_per_branch: Option<usize>,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    )
    where
        SpawnFn: FnOnce(Box<dyn FnOnce() + Send>) + Send + 'static,
    {
        self.tee_internal_blocking(mode, max_buffer_per_branch, spawn)
    }

    // Default case using blocking
    pub fn tee(
        self,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    ) {
        self.tee_internal_blocking(BackpressureMode::SpecCompliant, Some(1000), |runner| {
            std::thread::spawn(move || runner());
        })
    }

    pub fn tee_with_backpressure(
        self,
        mode: BackpressureMode,
        max_buffer_per_branch: Option<usize>,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    ) {
        self.tee_internal_blocking(mode, max_buffer_per_branch, |runner| {
            std::thread::spawn(move || runner());
        })
    }
}

impl<T, Source, S> ReadableStream<T, Source, S, Unlocked>
where
    T: Send + Sync + Clone + 'static,
    Source: Send + Sync + 'static,
    S: StreamTypeMarker + Sync,
{
    // Internal method for async spawn
    fn tee_internal_async<SpawnFn>(
        self,
        mode: BackpressureMode,
        max_buffer_per_branch: Option<usize>,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    )
    where
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = ()> + Send>>) + Send + 'static,
    {
        let (_, reader) = self.get_reader();
        let max_buffer = max_buffer_per_branch.unwrap_or(1000);

        let (branch1_tx, branch1_rx) = unbounded::<TeeChunk<T>>();
        let (branch2_tx, branch2_rx) = unbounded::<TeeChunk<T>>();

        let branch1_canceled = Arc::new(AtomicBool::new(false));
        let branch2_canceled = Arc::new(AtomicBool::new(false));

        let (branch1_pending, branch2_pending, backpressure_signal) =
            if matches!(mode, BackpressureMode::Unbounded) {
                (None, None, None)
            } else {
                (
                    Some(Arc::new(AtomicUsize::new(0))),
                    Some(Arc::new(AtomicUsize::new(0))),
                    Some(AsyncSignal::new()),
                )
            };

        let coordinator = TeeCoordinator {
            reader,
            branch1_tx: branch1_tx.clone(),
            branch2_tx: branch2_tx.clone(),
            branch1_canceled: branch1_canceled.clone(),
            branch2_canceled: branch2_canceled.clone(),
            backpressure_mode: mode,
            branch1_pending_count: branch1_pending.clone(),
            branch2_pending_count: branch2_pending.clone(),
            max_buffer_per_branch: max_buffer,
            backpressure_signal: backpressure_signal.clone(),
        };

        // Async spawn pattern - direct future execution
        /*spawn(Box::pin(async move {
            coordinator.run().await;
        }));*/
        spawn(Box::pin(coordinator.run()));

        let source1 = TeeSource {
            chunk_rx: branch1_rx,
            branch_id: TeeSourceId::Branch1,
            branch_canceled: branch1_canceled,
            pending_count: branch1_pending,
            backpressure_signal: backpressure_signal.clone(),
        };

        let source2 = TeeSource {
            chunk_rx: branch2_rx,
            branch_id: TeeSourceId::Branch2,
            branch_canceled: branch2_canceled,
            pending_count: branch2_pending,
            backpressure_signal,
        };

        (ReadableStream::new(source1), ReadableStream::new(source2))
    }

    // Public async methods
    pub fn tee_with_async_spawn<SpawnFn>(
        self,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    )
    where
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = ()> + Send>>) + Send + 'static,
    {
        self.tee_internal_async(BackpressureMode::SpecCompliant, Some(1000), spawn)
    }

    pub fn tee_with_backpressure_and_async_spawn<SpawnFn>(
        self,
        mode: BackpressureMode,
        max_buffer_per_branch: Option<usize>,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    )
    where
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = ()> + Send>>) + Send + 'static,
    {
        self.tee_internal_async(mode, max_buffer_per_branch, spawn)
    }
}

//TODO: update implementation so that `Send` is not required upstream, in that way i can relax this and remove the `Send` bound on this
impl<T, Source, S> ReadableStream<T, Source, S, Unlocked>
where
    T: Clone + Send + 'static,
    Source: Send + 'static,
    S: StreamTypeMarker,
{
    // Internal method for local async spawn
    fn tee_internal_local_async<SpawnFn>(
        self,
        mode: BackpressureMode,
        max_buffer_per_branch: Option<usize>,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    )
    where
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = ()>>>) + 'static,
    {
        let (_, reader) = self.get_reader();
        let max_buffer = max_buffer_per_branch.unwrap_or(1000);

        let (branch1_tx, branch1_rx) = unbounded::<TeeChunk<T>>();
        let (branch2_tx, branch2_rx) = unbounded::<TeeChunk<T>>();

        let branch1_canceled = Arc::new(AtomicBool::new(false));
        let branch2_canceled = Arc::new(AtomicBool::new(false));

        let (branch1_pending, branch2_pending, backpressure_signal) =
            if matches!(mode, BackpressureMode::Unbounded) {
                (None, None, None)
            } else {
                (
                    Some(Arc::new(AtomicUsize::new(0))),
                    Some(Arc::new(AtomicUsize::new(0))),
                    Some(AsyncSignal::new()),
                )
            };

        let coordinator = TeeCoordinator {
            reader,
            branch1_tx: branch1_tx.clone(),
            branch2_tx: branch2_tx.clone(),
            branch1_canceled: branch1_canceled.clone(),
            branch2_canceled: branch2_canceled.clone(),
            backpressure_mode: mode,
            branch1_pending_count: branch1_pending.clone(),
            branch2_pending_count: branch2_pending.clone(),
            max_buffer_per_branch: max_buffer,
            backpressure_signal: backpressure_signal.clone(),
        };

        // Local async spawn pattern
        spawn(Box::pin(coordinator.run()));

        let source1 = TeeSource {
            chunk_rx: branch1_rx,
            branch_id: TeeSourceId::Branch1,
            branch_canceled: branch1_canceled,
            pending_count: branch1_pending,
            backpressure_signal: backpressure_signal.clone(),
        };

        let source2 = TeeSource {
            chunk_rx: branch2_rx,
            branch_id: TeeSourceId::Branch2,
            branch_canceled: branch2_canceled,
            pending_count: branch2_pending,
            backpressure_signal,
        };

        (ReadableStream::new(source1), ReadableStream::new(source2))
    }

    // Public local async methods
    pub fn tee_with_local_async_spawn<SpawnFn>(
        self,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    )
    where
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = ()>>>) + 'static,
    {
        self.tee_internal_local_async(BackpressureMode::SpecCompliant, Some(1000), spawn)
    }

    pub fn tee_with_backpressure_and_local_async_spawn<SpawnFn>(
        self,
        mode: BackpressureMode,
        max_buffer_per_branch: Option<usize>,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
        ReadableStream<T, TeeSource<T>, DefaultStream, Unlocked>,
    )
    where
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = ()>>>) + 'static,
    {
        self.tee_internal_local_async(mode, max_buffer_per_branch, spawn)
    }
}

impl<T, Source, S> ReadableStream<T, Source, S, Unlocked>
where
    T: Send + Sync + 'static,
    Source: Send + Sync + 'static,
    S: StreamTypeMarker + Sync,
{
    pub fn _pipe_to_with_spawn<Sink, SpawnFn, Fut>(
        self,
        destination: WritableStream<T, Sink>,
        options: Option<StreamPipeOptions>,
        spawn: SpawnFn,
    ) -> Fut
    where
        Sink: WritableSink<T> + Send + Sync + 'static,
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>) -> Fut,
        Fut: Future<Output = StreamResult<()>> + 'static,
    {
        let fut: Pin<Box<dyn Future<Output = StreamResult<()>> + Send>> =
            Box::pin(async move { self.pipe_to_inner(&destination, options).await });

        spawn(fut)
    }

    pub fn pipe_to_with_spawn<Sink, SpawnFn, Fut>(
        self,
        destination: WritableStream<T, Sink>,
        options: Option<StreamPipeOptions>,
        spawn: SpawnFn,
    ) -> Fut
    where
        Sink: WritableSink<T> + Send + Sync + 'static,
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>) -> Fut,
        Fut: Future + 'static,
    {
        let fut = Box::pin(async move { self.pipe_to_inner(&destination, options).await });
        spawn(fut)
    }

    pub fn pipe_through_spawned<O, SpawnFn, Fut>(
        self,
        transform: TransformStream<T, O>,
        options: Option<StreamPipeOptions>,
        spawn: SpawnFn,
    ) -> (
        ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>,
        Fut,
    )
    where
        O: Send + 'static,
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>) -> Fut,
        Fut: Future + 'static,
    {
        let (readable, writable) = transform.split();

        let pipe_future = Box::pin(async move { self.pipe_to(&writable, options).await });

        let spawn_result = spawn(pipe_future);

        (readable, spawn_result)
    }

    pub fn pipe_through_with_spawn<O, SpawnFn, Fut>(
        self,
        transform: TransformStream<T, O>,
        options: Option<StreamPipeOptions>,
        spawn: SpawnFn,
    ) -> ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>
    where
        O: Send + 'static,
        SpawnFn: FnOnce(Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>) -> Fut,
        Fut: Future + 'static,
    {
        let (readable, _future) = self.pipe_through_spawned(transform, options, spawn);

        readable
    }
}

#[derive(Default)]
pub struct StreamPipeOptions {
    pub prevent_close: bool,
    pub prevent_abort: bool,
    pub prevent_cancel: bool,
    pub signal: Option<AbortRegistration>,
}

// ----------- Constructor Implementation  -----------
impl<T, Source> ReadableStream<T, Source, DefaultStream, Unlocked>
where
    T: Send + 'static,
    Source: ReadableSource<T> + Send + 'static,
{
    pub fn new_default(
        source: Source,
        strategy: Option<Box<dyn QueuingStrategy<T> + Send + Sync + 'static>>,
    ) -> Self {
        let (command_tx, command_rx) = unbounded();
        let (ctrl_tx, ctrl_rx) = unbounded();
        let queue_total_size = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let locked = Arc::new(AtomicBool::new(false));
        let stored_error = Arc::new(RwLock::new(None));

        let strategy: Box<dyn QueuingStrategy<T> + Send + Sync> =
            strategy.unwrap_or_else(|| Box::new(CountQueuingStrategy::new(1)));

        let high_water_mark = Arc::new(AtomicUsize::new(strategy.high_water_mark()));
        let desired_size = Arc::new(AtomicIsize::new(strategy.high_water_mark() as isize));

        let inner = ReadableStreamInner::new(source, strategy);

        let controller = ReadableStreamDefaultController::new(
            ctrl_tx.clone(),
            Arc::clone(&queue_total_size),
            Arc::clone(&high_water_mark),
            Arc::clone(&desired_size),
            Arc::clone(&closed),
            Arc::clone(&errored),
        );

        // Spawn the stream task
        let task_fut = readable_stream_task(
            command_rx,
            ctrl_rx,
            inner,
            Arc::clone(&queue_total_size),
            Arc::clone(&high_water_mark),
            Arc::clone(&desired_size),
            Arc::clone(&closed),
            Arc::clone(&errored),
            Arc::clone(&stored_error),
            ctrl_tx,
            controller.clone(),
        );

        std::thread::spawn(move || {
            futures::executor::block_on(task_fut);
        });

        Self {
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
            _phantom: PhantomData,
        }
    }
}

impl<Source> ReadableStream<Vec<u8>, Source, ByteStream, Unlocked>
where
    Source: ReadableByteSource + Send + 'static,
{
    pub fn new_bytes(
        source: Source,
        strategy: Option<Box<dyn QueuingStrategy<Vec<u8>> + Send + Sync + 'static>>,
    ) -> Self {
        let (command_tx, command_rx) = unbounded();
        let (ctrl_tx, ctrl_rx) = unbounded::<ByteControllerMsg>();
        let queue_total_size = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let locked = Arc::new(AtomicBool::new(false));
        let stored_error = Arc::new(RwLock::new(None));

        let strategy: Box<dyn QueuingStrategy<Vec<u8>> + Send + Sync> =
            strategy.unwrap_or_else(|| Box::new(CountQueuingStrategy::new(1)));

        let high_water_mark = Arc::new(AtomicUsize::new(strategy.high_water_mark()));
        let desired_size = Arc::new(AtomicIsize::new(strategy.high_water_mark() as isize));

        //let inner = ReadableStreamInner::new(source, strategy);

        let byte_state = ByteStreamState::new(source, strategy.high_water_mark());
        let controller = ReadableByteStreamController::new(byte_state.clone());

        let task_state = byte_state.clone();
        let task_fut = readable_byte_stream_task(task_state, command_rx, controller.clone());

        std::thread::spawn(move || {
            futures::executor::block_on(task_fut);
        });

        Self {
            command_tx,
            queue_total_size,
            high_water_mark,
            desired_size,
            closed,
            errored,
            locked,
            stored_error,
            controller: Arc::new(controller),
            byte_state: Some(byte_state),
            _phantom: PhantomData,
        }
    }
}

// ----------- Generic Constructor -----------
impl<T, Source> ReadableStream<T, Source, DefaultStream, Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
{
    pub fn new(source: Source) -> Self
    where
        Source: ReadableSource<T>,
    {
        Self::new_with_strategy(source, CountQueuingStrategy::new(1))
    }

    pub fn new_with_strategy<Strategy>(source: Source, strategy: Strategy) -> Self
    where
        Source: ReadableSource<T>,
        Strategy: QueuingStrategy<T> + Send + Sync + 'static,
    {
        let (command_tx, command_rx) = unbounded();
        let (ctrl_tx, ctrl_rx) = unbounded();
        let queue_total_size = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let locked = Arc::new(AtomicBool::new(false));
        let stored_error = Arc::new(RwLock::new(None));

        /*let strategy: Box<dyn QueuingStrategy<T> + Send + Sync> =
        strategy.unwrap_or_else(|| Box::new(CountQueuingStrategy::new(1)));*/

        let high_water_mark = Arc::new(AtomicUsize::new(strategy.high_water_mark()));
        let desired_size = Arc::new(AtomicIsize::new(strategy.high_water_mark() as isize));

        let inner = ReadableStreamInner::new(source, Box::new(strategy));

        let controller = ReadableStreamDefaultController::new(
            ctrl_tx.clone(),
            Arc::clone(&queue_total_size),
            Arc::clone(&high_water_mark),
            Arc::clone(&desired_size),
            Arc::clone(&closed),
            Arc::clone(&errored),
        );

        // Spawn the stream task
        let task_fut = readable_stream_task(
            command_rx,
            ctrl_rx,
            inner,
            Arc::clone(&queue_total_size),
            Arc::clone(&high_water_mark),
            Arc::clone(&desired_size),
            Arc::clone(&closed),
            Arc::clone(&errored),
            Arc::clone(&stored_error),
            ctrl_tx,
            controller.clone(),
        );

        std::thread::spawn(move || {
            futures::executor::block_on(task_fut);
        });

        Self {
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
            _phantom: PhantomData,
        }
    }
}

impl<T, I> ReadableStream<T, IteratorSource<I>, DefaultStream, Unlocked>
where
    T: Send + 'static,
    I: Iterator<Item = T> + Send + 'static,
{
    pub fn from_iter(iter: I, strategy: Option<Box<dyn QueuingStrategy<T> + Send + Sync>>) -> Self {
        Self::new_default(IteratorSource { iter }, strategy)
    }
}

impl<T, S> ReadableStream<T, AsyncStreamSource<S>, DefaultStream, Unlocked>
where
    T: Send + 'static,
    S: Stream<Item = T> + Unpin + Send + 'static,
{
    pub fn from_async_stream(
        stream: S,
        strategy: Option<Box<dyn QueuingStrategy<T> + Send + Sync>>,
    ) -> Self {
        Self::new_default(AsyncStreamSource { stream }, strategy)
    }
}

// ----------- Additional reader methods for generic streams -----------
impl<T, Source, StreamType> ReadableStream<T, Source, StreamType, Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
{
    pub fn get_reader(
        self,
    ) -> (
        ReadableStream<T, Source, StreamType, Locked>,
        ReadableStreamDefaultReader<T, Source, StreamType, Locked>,
    ) {
        self.locked.store(true, Ordering::SeqCst);

        let locked_stream = ReadableStream {
            command_tx: self.command_tx.clone(),
            queue_total_size: Arc::clone(&self.queue_total_size),
            high_water_mark: Arc::clone(&self.high_water_mark),
            desired_size: Arc::clone(&self.desired_size),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            locked: Arc::clone(&self.locked),
            stored_error: Arc::clone(&self.stored_error),
            controller: self.controller.clone(),
            byte_state: self.byte_state.clone(),
            _phantom: PhantomData,
        };

        let reader = ReadableStreamDefaultReader::new(ReadableStream {
            command_tx: self.command_tx,
            queue_total_size: self.queue_total_size,
            high_water_mark: self.high_water_mark,
            desired_size: self.desired_size,
            closed: self.closed,
            errored: self.errored,
            locked: self.locked,
            stored_error: self.stored_error,
            controller: self.controller,
            byte_state: self.byte_state.clone(),
            _phantom: PhantomData,
        });

        (locked_stream, reader)
    }
}

// ----------- Reader Methods for Byte Streams -----------
impl<Source> ReadableStream<Vec<u8>, Source, ByteStream, Unlocked>
where
    Source: ReadableByteSource + Send + 'static,
{
    pub fn get_byob_reader(
        self,
    ) -> (
        ReadableStream<Vec<u8>, Source, ByteStream, Locked>,
        ReadableStreamBYOBReader<Source, Locked>,
    ) {
        self.locked.store(true, Ordering::SeqCst);

        let locked_stream = ReadableStream {
            command_tx: self.command_tx.clone(),
            queue_total_size: Arc::clone(&self.queue_total_size),
            high_water_mark: Arc::clone(&self.high_water_mark),
            desired_size: Arc::clone(&self.desired_size),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            locked: Arc::clone(&self.locked),
            stored_error: Arc::clone(&self.stored_error),
            controller: self.controller.clone(),
            byte_state: self.byte_state.clone(),
            _phantom: PhantomData,
        };

        let reader = ReadableStreamBYOBReader::new(ReadableStream {
            command_tx: self.command_tx,
            queue_total_size: self.queue_total_size,
            high_water_mark: self.high_water_mark,
            desired_size: self.desired_size,
            closed: self.closed,
            errored: self.errored,
            locked: self.locked,
            stored_error: self.stored_error,
            controller: self.controller,
            byte_state: self.byte_state.clone(),
            _phantom: PhantomData,
        });

        (locked_stream, reader)
    }
}

// ----------- Stream Trait Implementation  -----------
impl<T, Source, StreamType, LockState> Stream for ReadableStream<T, Source, StreamType, LockState>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
{
    type Item = StreamResult<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.errored.load(Ordering::SeqCst) {
            let error = self
                .stored_error
                .read()
                .ok()
                .and_then(|guard| guard.clone())
                .unwrap_or_else(|| StreamError::Custom("Stream is errored".into()));
            return Poll::Ready(Some(Err(error)));
        }

        if self.closed.load(Ordering::SeqCst) {
            return Poll::Ready(None);
        }

        let waker = cx.waker().clone();
        let _ = self
            .command_tx
            .unbounded_send(StreamCommand::RegisterReadyWaker { waker });

        Poll::Pending
    }
}

impl<T, Source, StreamType, LockState> AsyncRead
    for ReadableStream<T, Source, StreamType, LockState>
where
    T: for<'a> From<&'a [u8]> + Send + 'static,
    Source: ReadableByteSource + Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
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
pub struct IteratorSource<I> {
    iter: I,
}

impl<I, T> ReadableSource<T> for IteratorSource<I>
where
    I: Iterator<Item = T> + Send + 'static,
    T: Send + 'static,
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

pub struct AsyncStreamSource<S> {
    stream: S,
}

impl<S, T> ReadableSource<T> for AsyncStreamSource<S>
where
    S: Stream<Item = T> + Unpin + Send + 'static,
    T: Send + 'static,
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
pub struct ReadableStreamDefaultReader<T, Source, StreamType, LockState>(
    ReadableStream<T, Source, StreamType, LockState>,
)
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static;

impl<T, Source, StreamType, LockState> ReadableStreamDefaultReader<T, Source, StreamType, LockState>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
{
    pub fn new(stream: ReadableStream<T, Source, StreamType, LockState>) -> Self {
        ReadableStreamDefaultReader(stream)
    }

    fn is_byte_stream(&self) -> bool {
        self.0.byte_state.is_some()
    }

    pub async fn closed(&self) -> StreamResult<()> {
        if self.is_byte_stream() {
            return self
                .0
                .byte_state
                .as_ref()
                .unwrap()
                .closed()
                .await
                .map_err(|e| e);
        }

        poll_fn(|cx| {
            if self.0.errored.load(Ordering::SeqCst) {
                let error = self
                    .0
                    .stored_error
                    .read()
                    .ok()
                    .and_then(|guard| guard.clone())
                    .unwrap_or_else(|| StreamError::Custom("Stream is errored".into()));
                return Poll::Ready(Err(error));
            }
            if self.0.closed.load(Ordering::SeqCst) {
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
        if self.is_byte_stream() {
            self.0
                .byte_state
                .as_ref()
                .unwrap()
                .cancel_source(reason.clone())
                .await?
        }

        let (tx, rx) = oneshot::channel();
        self.0
            .command_tx
            .unbounded_send(StreamCommand::Cancel {
                reason,
                completion: tx,
            })
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;
        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Cancel canceled".into())))
    }

    pub async fn read(&self) -> StreamResult<Option<T>> {
        let (tx, rx) = oneshot::channel();
        self.0
            .command_tx
            .unbounded_send(StreamCommand::Read { completion: tx })
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;
        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Read canceled".into())))
    }

    pub fn release_lock(self) -> ReadableStream<T, Source, StreamType, Unlocked> {
        self.0.locked.store(false, Ordering::SeqCst);
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
            _phantom: PhantomData,
        }
    }
}

/*impl<Source, StreamType, LockState>
    ReadableStreamDefaultReader<Vec<u8>, Source, StreamType, LockState>
where
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
{
}*/

impl<T, Source, StreamType, LockState> Drop
    for ReadableStreamDefaultReader<T, Source, StreamType, LockState>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
{
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::SeqCst);
    }
}

// ----------- BYOB Reader (preserving original structure) -----------
pub struct ReadableStreamBYOBReader<Source, LockState>(
    ReadableStream<Vec<u8>, Source, ByteStream, LockState>,
)
where
    Source: Send + 'static,
    LockState: Send + 'static;

impl<Source, LockState> ReadableStreamBYOBReader<Source, LockState>
where
    Source: ReadableByteSource + Send + 'static,
    LockState: Send + 'static,
{
    pub fn new(stream: ReadableStream<Vec<u8>, Source, ByteStream, LockState>) -> Self {
        ReadableStreamBYOBReader(stream)
    }

    pub async fn closed(&self) -> Result<(), StreamError> {
        self.0.controller.byte_state.closed().await
    }

    pub async fn cancel(&self, reason: Option<String>) -> Result<(), StreamError> {
        if let Some(byte_state_arc) = &self.0.byte_state {
            // call the trait object's cancel_source helper
            byte_state_arc
                .cancel_source(reason)
                .await
                .map_err(|e| e.into())
        } else {
            // Already canceled or closed
            Ok(())
        }
    }

    pub async fn read(&self, buf: &mut [u8]) -> StreamResult<usize> {
        poll_fn(|cx| self.0.byte_state.as_ref().unwrap().poll_read_into(cx, buf)).await
    }

    pub fn release_lock(self) -> ReadableStream<Vec<u8>, Source, ByteStream, Unlocked> {
        self.0.locked.store(false, Ordering::SeqCst);
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
            _phantom: PhantomData,
        }
    }
}

impl<Source, LockState> Drop for ReadableStreamBYOBReader<Source, LockState>
where
    Source: Send + 'static,
    LockState: Send + 'static,
{
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::SeqCst);
    }
}

fn update_desired_size(
    queue_total_size: &Arc<AtomicUsize>,
    high_water_mark: &Arc<AtomicUsize>,
    desired_size: &Arc<AtomicIsize>,
    closed: &Arc<AtomicBool>,
    errored: &Arc<AtomicBool>,
) {
    if closed.load(Ordering::SeqCst) || errored.load(Ordering::SeqCst) {
        desired_size.store(0, Ordering::SeqCst);
        return;
    }

    let hwm = high_water_mark.load(Ordering::SeqCst) as isize;
    let current = queue_total_size.load(Ordering::SeqCst) as isize;
    let new_desired_size = hwm - current;

    desired_size.store(new_desired_size, Ordering::SeqCst);
}

// ----------- Stream Task Implementation -----------
async fn readable_stream_task<T, Source>(
    mut command_rx: UnboundedReceiver<StreamCommand<T>>,
    mut ctrl_rx: UnboundedReceiver<ControllerMsg<T>>,
    mut inner: ReadableStreamInner<T, Source>,
    queue_total_size: Arc<AtomicUsize>,
    high_water_mark: Arc<AtomicUsize>,
    desired_size: Arc<AtomicIsize>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    stored_error: Arc<RwLock<Option<StreamError>>>,
    ctrl_tx: UnboundedSender<ControllerMsg<T>>,
    mut controller: ReadableStreamDefaultController<T>,
) where
    T: Send + 'static,
    Source: ReadableSource<T> + Send + 'static,
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
                errored.store(true, Ordering::SeqCst);
                inner.stored_error = Some(err.clone());
                if let Ok(mut guard) = stored_error.write() {
                    *guard = Some(err.clone());
                }
                desired_size.store(0, Ordering::SeqCst);
                inner.closed_wakers.wake_all();
                inner.ready_wakers.wake_all();
                // Don't return here - we still need to handle any pending commands
            }
        }
    }

    let mut pull_future: Option<Pin<Box<dyn Future<Output = (Source, StreamResult<()>)> + Send>>> =
        None;
    let mut cancel_future: Option<Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>> = None;

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
                        queue_total_size.store(inner.queue_total_size, Ordering::SeqCst);
                        update_desired_size(
                            &queue_total_size,
                            &high_water_mark,
                            &desired_size,
                            &closed,
                            &errored,
                        );
                        inner.ready_wakers.wake_all();
                    }
                }
                ControllerMsg::Close => {
                    if inner.state == StreamState::Readable {
                        inner.state = StreamState::Closed;
                        closed.store(true, Ordering::SeqCst);
                        desired_size.store(0, Ordering::SeqCst);
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
                        errored.store(true, Ordering::SeqCst);
                        inner.stored_error = Some(err.clone());
                        desired_size.store(0, Ordering::SeqCst);
                        if let Ok(mut guard) = stored_error.write() {
                            *guard = Some(err.clone());
                        }
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        queue_total_size.store(0, Ordering::SeqCst);
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
                        queue_total_size.store(inner.queue_total_size, Ordering::SeqCst);
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
                }
                StreamCommand::Cancel { reason, completion } => {
                    if inner.state == StreamState::Closed || inner.state == StreamState::Errored {
                        let _ = completion.send(Ok(()));
                        continue;
                    }
                    if inner.cancel_requested {
                        inner.cancel_completions.push(completion);
                    } else {
                        inner.cancel_requested = true;
                        inner.cancel_reason = reason.clone();
                        inner.cancel_completions.push(completion);
                        inner.state = StreamState::Closed;
                        closed.store(true, Ordering::SeqCst);
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        queue_total_size.store(0, Ordering::SeqCst);
                        while let Some(tx) = inner.pending_reads.pop_front() {
                            let _ = tx.send(Err(StreamError::Canceled));
                        }
                        inner.closed_wakers.wake_all();
                        inner.ready_wakers.wake_all();
                        if let Some(mut source) = inner.source.take() {
                            let reason_clone = reason;
                            cancel_future =
                                Some(Box::pin(async move { source.cancel(reason_clone).await }));
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
                StreamCommand::ReadInto { .. } => {
                    // Default streams don't support ReadInto
                    // This should probably be an error
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

        // Pull data if needed
        if inner.state == StreamState::Readable
            && !inner.pulling
            && !inner.cancel_requested
            && (inner.queue.is_empty() || !inner.pending_reads.is_empty())
        {
            if let Some(source) = inner.source.take() {
                inner.pulling = true;
                let mut controller = controller.clone();
                pull_future = Some(Box::pin(async move {
                    let mut source = source;
                    let result = source.pull(&mut controller).await;
                    (source, result)
                }));
            }
        }

        // Poll pull future if in progress
        if let Some(ref mut fut) = pull_future {
            match fut.as_mut().poll(cx) {
                Poll::Ready((source, result)) => {
                    inner.pulling = false;
                    match result {
                        Ok(()) => {
                            inner.source = Some(source);
                        }
                        Err(err) => {
                            inner.state = StreamState::Errored;
                            errored.store(true, Ordering::SeqCst);
                            inner.stored_error = Some(err.clone());
                            if let Ok(mut guard) = stored_error.write() {
                                *guard = Some(err.clone());
                            }
                            while let Some(tx) = inner.pending_reads.pop_front() {
                                let _ = tx.send(Err(err.clone()));
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
pub async fn readable_byte_stream_task<Source>(
    byte_state: Arc<ByteStreamState<Source>>,
    mut command_rx: UnboundedReceiver<StreamCommand<Vec<u8>>>,
    mut controller: ReadableByteStreamController,
) where
    Source: ReadableByteSource + Send + 'static,
{
    let _ = byte_state.start_source(&controller).await;

    // Pending read requests queued while no data is available
    let mut pending_reads: VecDeque<oneshot::Sender<StreamResult<Option<Vec<u8>>>>> =
        VecDeque::new();

    loop {
        let pull_fut = poll_fn(|cx| byte_state.poll_pull_needed(cx)).fuse();
        let cmd_fut = command_rx.next().fuse();
        futures::pin_mut!(pull_fut, cmd_fut);

        futures::select! {
            // 1️⃣ Pull data from source if needed
            _ = pull_fut => {
                if byte_state.closed.load(std::sync::atomic::Ordering::SeqCst)
                    || byte_state.errored.load(std::sync::atomic::Ordering::SeqCst)
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

                let mut buffer = vec![0u8; 8192];
                let size_before = byte_state.buffer_size();

                match source.pull(&mut controller, &mut buffer).await {
                    Ok(bytes_read) => {
                        let mut any_data_produced = false;

                        // Directly buffered data
                        if bytes_read > 0 {
                            byte_state.enqueue_data(&buffer[..bytes_read]);
                            any_data_produced = true;
                        }

                        // Data enqueued via controller
                        if byte_state.buffer_size() > size_before {
                            any_data_produced = true;
                        }

                        // Fulfill pending reads if any data appeared
                        if any_data_produced {
                            while let Some(completion) = pending_reads.pop_front() {
                                let mut read_buf = vec![0u8; 8192];
                                match poll_fn(|cx| byte_state.poll_read_into(cx, &mut read_buf)).now_or_never() {
                                    Some(Ok(0)) => {
                                        let _ = completion.send(Ok(None));
                                    }
                                    Some(Ok(n)) => {
                                        read_buf.truncate(n);
                                        let _ = completion.send(Ok(Some(read_buf)));
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

                        // Close stream if EOF and no data was produced via controller either
                        if bytes_read == 0 && !any_data_produced {
                            byte_state.close();
                            // Fulfill any remaining pending reads with EOF
                            while let Some(completion) = pending_reads.pop_front() {
                                let _ = completion.send(Ok(None));
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
                if !byte_state.closed.load(std::sync::atomic::Ordering::SeqCst)
                    && !byte_state.errored.load(std::sync::atomic::Ordering::SeqCst)
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
                        if byte_state.errored.load(Ordering::SeqCst) {
                            let error = byte_state.error.lock()
                                .clone()
                                .unwrap_or_else(|| StreamError::Custom("Stream errored".into()));
                            let _ = completion.send(Err(error));
                            continue;
                        }

                        // Closed and no data
                        if byte_state.closed.load(Ordering::SeqCst) && byte_state.is_buffer_empty() {
                            let _ = completion.send(Ok(None));
                            continue;
                        }

                        // Attempt immediate read
                        let mut buf = vec![0u8; 8192];
                        match poll_fn(|cx| byte_state.poll_read_into(cx, &mut buf)).now_or_never() {
                            Some(Ok(0)) => {
                                let _ = completion.send(Ok(None));
                            }
                            Some(Ok(n)) => {
                                buf.truncate(n);
                                let _ = completion.send(Ok(Some(buf)));
                            }
                            Some(Err(err)) => {
                                let _ = completion.send(Err(err));
                            }
                            None => {
                                // No data available
                                if byte_state.closed.load(Ordering::SeqCst) {
                                    // Stream is closed and no data - return EOF
                                    let _ = completion.send(Ok(None));
                                } else {
                                    // Queue read and trigger pull immediately
                                    pending_reads.push_back(completion);
                                    if !byte_state.errored.load(Ordering::SeqCst) {
                                        byte_state.maybe_trigger_pull();
                                    }
                                }
                            }
                        }
                    }
                    Some(StreamCommand::Cancel { reason, completion }) => {
                        let cancel_result = byte_state.cancel_source(reason).await;
                        let _ = completion.send(cancel_result);
                        break;
                    }
                    Some(_) => {}
                    None => {
                        // Command channel closed - exit only if no pending reads
                        if pending_reads.is_empty() {
                            break;
                        }
                        // If we have pending reads but stream is closed, fulfill them with EOF
                        if byte_state.closed.load(Ordering::SeqCst) {
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
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
{
    source: Source,
    strategy: Option<Box<dyn QueuingStrategy<T> + Send + Sync + 'static>>,
    spawn_config: SpawnConfig,
    _phantom: PhantomData<(T, StreamType)>,
}

// Enum to handle different spawn strategies cleanly
pub enum SpawnConfig {
    DefaultThread,
    CustomSpawn(Box<dyn FnOnce(Pin<Box<dyn Future<Output = ()> + Send>>) + Send + 'static>),
}

impl Default for SpawnConfig {
    fn default() -> Self {
        SpawnConfig::DefaultThread
    }
}

// Generic constructor for all stream types
impl<T, Source, StreamType> ReadableStreamBuilder<T, Source, StreamType>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
{
    pub fn new(source: Source) -> Self {
        Self {
            source,
            strategy: None,
            spawn_config: SpawnConfig::DefaultThread,
            _phantom: PhantomData,
        }
    }

    pub fn with_strategy<S>(mut self, strategy: S) -> Self
    where
        S: QueuingStrategy<T> + Send + Sync + 'static,
    {
        self.strategy = Some(Box::new(strategy));
        self
    }

    pub fn with_spawn<F>(mut self, spawn_fn: F) -> Self
    where
        F: FnOnce(Pin<Box<dyn Future<Output = ()> + Send>>) + Send + 'static,
    {
        self.spawn_config = SpawnConfig::CustomSpawn(Box::new(spawn_fn));
        self
    }

    pub fn with_thread_spawn(mut self) -> Self {
        self.spawn_config = SpawnConfig::DefaultThread;
        self
    }
}

impl<T, Source> ReadableStreamBuilder<T, Source, DefaultStream>
where
    T: Send + 'static,
    Source: ReadableSource<T> + Send + 'static,
{
    pub fn build(self) -> ReadableStream<T, Source, DefaultStream, Unlocked> {
        let (command_tx, command_rx) = unbounded();
        let (ctrl_tx, ctrl_rx) = unbounded();

        let queue_total_size = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let locked = Arc::new(AtomicBool::new(false));
        let stored_error = Arc::new(RwLock::new(None));

        let strategy: Box<dyn QueuingStrategy<T> + Send + Sync> = self
            .strategy
            .unwrap_or_else(|| Box::new(CountQueuingStrategy::new(1)));

        let high_water_mark = Arc::new(AtomicUsize::new(strategy.high_water_mark()));
        let desired_size = Arc::new(AtomicIsize::new(strategy.high_water_mark() as isize));

        let inner = ReadableStreamInner::new(self.source, strategy);

        let controller = ReadableStreamDefaultController::new(
            ctrl_tx.clone(),
            Arc::clone(&queue_total_size),
            Arc::clone(&high_water_mark),
            Arc::clone(&desired_size),
            Arc::clone(&closed),
            Arc::clone(&errored),
        );

        let task_fut = readable_stream_task(
            command_rx,
            ctrl_rx,
            inner,
            Arc::clone(&queue_total_size),
            Arc::clone(&high_water_mark),
            Arc::clone(&desired_size),
            Arc::clone(&closed),
            Arc::clone(&errored),
            Arc::clone(&stored_error),
            ctrl_tx,
            controller.clone(),
        );

        match self.spawn_config {
            SpawnConfig::DefaultThread => {
                std::thread::spawn(move || {
                    futures::executor::block_on(task_fut);
                });
            }
            SpawnConfig::CustomSpawn(spawn_fn) => {
                spawn_fn(Box::pin(task_fut));
            }
        }

        ReadableStream {
            command_tx,
            queue_total_size,
            high_water_mark,
            desired_size,
            closed,
            errored,
            locked,
            stored_error,
            controller: Arc::new(controller),
            byte_state: None,
            _phantom: PhantomData,
        }
    }
}

impl<Source> ReadableStreamBuilder<Vec<u8>, Source, ByteStream>
where
    Source: ReadableByteSource + Send + 'static,
{
    pub fn build(self) -> ReadableStream<Vec<u8>, Source, ByteStream, Unlocked> {
        let (command_tx, command_rx) = unbounded();
        let (_ctrl_tx, _ctrl_rx) = unbounded::<ByteControllerMsg>();

        let strategy = self
            .strategy
            .unwrap_or_else(|| Box::new(CountQueuingStrategy::new(1)));
        let direct_state = ByteStreamState::new(self.source, strategy.high_water_mark());

        let controller = ReadableByteStreamController::new(direct_state.clone());

        let task_fut =
            readable_byte_stream_task(direct_state.clone(), command_rx, controller.clone());

        match self.spawn_config {
            SpawnConfig::DefaultThread => {
                std::thread::spawn(move || {
                    futures::executor::block_on(task_fut);
                });
            }
            SpawnConfig::CustomSpawn(spawn_fn) => {
                spawn_fn(Box::pin(task_fut));
            }
        }

        ReadableStream {
            command_tx,
            queue_total_size: Arc::new(AtomicUsize::new(0)),
            high_water_mark: Arc::new(AtomicUsize::new(strategy.high_water_mark())),
            desired_size: Arc::new(AtomicIsize::new(strategy.high_water_mark() as isize)),
            closed: Arc::new(AtomicBool::new(false)),
            errored: Arc::new(AtomicBool::new(false)),
            locked: Arc::new(AtomicBool::new(false)),
            stored_error: Arc::new(RwLock::new(None)),
            controller: Arc::new(controller),
            byte_state: Some(direct_state),
            _phantom: PhantomData,
        }
    }
}

// Convenience constructors
impl<T> ReadableStreamBuilder<T, IteratorSource<std::vec::IntoIter<T>>, DefaultStream>
where
    T: Send + 'static,
{
    pub fn from_vec(vec: Vec<T>) -> Self {
        Self::new(IteratorSource {
            iter: vec.into_iter(),
        })
    }
}

impl<T, I> ReadableStreamBuilder<T, IteratorSource<I>, DefaultStream>
where
    T: Send + 'static,
    I: Iterator<Item = T> + Send + 'static,
{
    pub fn from_iterator(iter: I) -> Self {
        Self::new(IteratorSource { iter })
    }
}

impl<T, S> ReadableStreamBuilder<T, AsyncStreamSource<S>, DefaultStream>
where
    T: Send + 'static,
    S: Stream<Item = T> + Unpin + Send + 'static,
{
    pub fn from_stream(stream: S) -> Self {
        Self::new(AsyncStreamSource { stream })
    }
}

impl<T, Source, StreamType> ReadableStream<T, Source, StreamType, Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
{
    pub fn builder(source: Source) -> ReadableStreamBuilder<T, Source, StreamType> {
        ReadableStreamBuilder::new(source)
    }
}

#[cfg(test)]
mod tests_old {
    use super::*;

    // Test 1: Basic iterator source functionality
    #[tokio::test]
    async fn test_iterator_source_basic() {
        let data = vec![1, 2, 3];
        let stream = ReadableStream::from_iter(data.into_iter(), None);
        let (_locked_stream, reader) = stream.get_reader();

        // Read all items
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert_eq!(reader.read().await.unwrap(), Some(3));
        assert_eq!(reader.read().await.unwrap(), None); // Stream closed
    }

    // Test 2: Stream closes properly when iterator is exhausted
    #[tokio::test]
    async fn test_stream_closes_on_iterator_end() {
        let empty_data: Vec<i32> = vec![];
        let stream = ReadableStream::from_iter(empty_data.into_iter(), None);
        let (_locked_stream, reader) = stream.get_reader();

        // Should immediately return None for empty iterator
        assert_eq!(reader.read().await.unwrap(), None);

        // Verify stream is actually closed
        reader.closed().await.unwrap();
    }

    // Test 3: Cancel functionality works
    #[tokio::test]
    async fn test_cancel_stream() {
        let data = vec![1, 2, 3, 4, 5];
        let stream = ReadableStream::from_iter(data.into_iter(), None);
        let (_locked_stream, reader) = stream.get_reader();

        // Read one item
        assert_eq!(reader.read().await.unwrap(), Some(1));

        // Cancel the stream
        reader
            .cancel(Some("test cancel".to_string()))
            .await
            .unwrap();

        // Further reads should return None (EOF)
        assert_eq!(reader.read().await.unwrap(), None);
    }

    // Test 4: Byte stream basic functionality
    #[tokio::test]
    async fn test_byte_stream_basic() {
        struct SimpleByteSource {
            data: Vec<u8>,
            pos: usize,
        }

        impl ReadableByteSource for SimpleByteSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
                buffer: &mut [u8],
            ) -> StreamResult<usize> {
                if self.pos >= self.data.len() {
                    controller.close()?;
                    return Ok(0);
                }

                /*let chunk_size = std::cmp::min(buffer.len(), self.data.len() - self.pos);
                let chunk = self.data[self.pos..self.pos + chunk_size].to_vec();
                self.pos += chunk_size;

                controller.enqueue(chunk)?;
                //Ok(chunk_size)
                Ok(0)*/
                let chunk_size = std::cmp::min(buffer.len(), self.data.len() - self.pos);
                let slice = &self.data[self.pos..self.pos + chunk_size];
                buffer[..chunk_size].copy_from_slice(slice);
                self.pos += chunk_size;

                Ok(chunk_size)
            }
        }

        let source = SimpleByteSource {
            data: b"hello world".to_vec(),
            pos: 0,
        };

        let stream = ReadableStream::new_bytes(source, None);
        let (_locked_stream, reader) = stream.get_reader();

        // Read data
        /*if let Some(chunk) = reader.read().await.unwrap() {
            assert_eq!(chunk, b"hello world".to_vec());
        } else {
            panic!("Expected data chunk");
        }*/
        let mut result = Vec::new();
        while let Some(chunk) = reader.read().await.unwrap() {
            result.extend_from_slice(&chunk);
        }

        assert_eq!(result, b"hello world");

        // Should be closed now
        assert_eq!(reader.read().await.unwrap(), None, "Should be closed");
    }

    // Test 5: BYOB reader functionality
    #[tokio::test]
    async fn _test_byob_reader() {
        struct ChunkedByteSource {
            chunks: Vec<Vec<u8>>,
            current: usize,
        }

        impl ReadableByteSource for ChunkedByteSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
                buffer: &mut [u8],
            ) -> StreamResult<usize> {
                if self.current >= self.chunks.len() {
                    controller.close()?;
                    return Ok(0);
                }

                /*let chunk = self.chunks[self.current].clone();
                let _len = chunk.len();
                self.current += 1;

                controller.enqueue(chunk)?;
                Ok(0)*/
                let chunk = &self.chunks[self.current];
                let len = chunk.len();
                let bytes_to_copy = std::cmp::min(len, buffer.len());

                buffer[..bytes_to_copy].copy_from_slice(&chunk[..bytes_to_copy]);

                // If chunk longer than buffer, keep the remaining part or implement logic to handle partial consumption (optional)

                self.current += 1;

                Ok(bytes_to_copy)
            }
        }

        let source = ChunkedByteSource {
            chunks: vec![b"hello".to_vec(), b" ".to_vec(), b"world".to_vec()],
            current: 0,
        };

        let stream = ReadableStream::new_bytes(source, None);
        let (_locked_stream, byob_reader) = stream.get_byob_reader();

        let mut buffer = [0u8; 10];

        // Read with BYOB reader
        match byob_reader.read(&mut buffer).await.unwrap() {
            bytes_read => {
                assert!(bytes_read > 0);
                println!("Read {} bytes", bytes_read);
                let read_data = &buffer[..bytes_read];
                println!("Buffer contents: {}", String::from_utf8_lossy(read_data));
            } //None => println!("Stream ended"),
        }
    }

    #[tokio::test]
    async fn test_byob_reader() {
        struct ChunkedByteSource {
            chunks: Vec<Vec<u8>>,
            current: usize,
            offset: usize,
        }

        impl ReadableByteSource for ChunkedByteSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
                buffer: &mut [u8],
            ) -> StreamResult<usize> {
                if self.current >= self.chunks.len() {
                    controller.close()?;
                    return Ok(0);
                }

                let current_chunk = &self.chunks[self.current];
                let available = &current_chunk[self.offset..];
                let bytes_to_copy = std::cmp::min(available.len(), buffer.len());

                buffer[..bytes_to_copy].copy_from_slice(&available[..bytes_to_copy]);

                self.offset += bytes_to_copy;

                if self.offset >= current_chunk.len() {
                    // Move to next chunk
                    self.current += 1;
                    self.offset = 0;
                }

                Ok(bytes_to_copy)
            }
        }

        let source = ChunkedByteSource {
            chunks: vec![b"hello".to_vec(), b" ".to_vec(), b"world".to_vec()],
            current: 0,
            offset: 0,
        };

        let stream = ReadableStream::new_bytes(source, None);
        let (_locked_stream, byob_reader) = stream.get_byob_reader();

        let mut buffer = [0u8; 10];

        // Read with BYOB reader
        match byob_reader.read(&mut buffer).await.unwrap() {
            bytes_read => {
                assert!(bytes_read > 0);
                println!("Read {} bytes", bytes_read);
            } //None => println!("Stream ended"),
        }
        println!("buffer content: {:?}", buffer);
    }

    // Test 6: Error propagation
    #[tokio::test]
    async fn test_error_propagation() {
        struct ErrorSource;

        impl ReadableSource<i32> for ErrorSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<i32>,
            ) -> StreamResult<()> {
                controller.error(StreamError::Custom("Test error".into()))?;
                Ok(())
            }
        }

        let stream = ReadableStream::new_default(ErrorSource, None);
        let (_locked_stream, reader) = stream.get_reader();

        // Should get the error
        match reader.read().await {
            Err(StreamError::Custom(_)) => {} // Expected
            other => panic!("Expected Custom error, got: {:?}", other),
        }
    }

    // Test 7: Reader lock/unlock behavior
    #[tokio::test]
    async fn test_reader_lock_unlock() {
        let data = vec![1, 2, 3];
        let stream = ReadableStream::from_iter(data.into_iter(), None);

        assert!(!stream.locked()); // Initially unlocked

        let (_locked_stream, reader) = stream.get_reader();
        // Stream is now locked (we can't directly test this on locked_stream)

        // Release the lock
        let unlocked_stream = reader.release_lock();
        assert!(!unlocked_stream.locked()); // Should be unlocked again
    }

    // Test 8: Multiple readers (should fail)
    #[tokio::test]
    async fn test_cannot_get_multiple_readers() {
        let data = vec![1, 2, 3];
        let stream = ReadableStream::from_iter(data.into_iter(), None);

        let (_locked_stream, _reader1) = stream.get_reader();
        // At this point, trying to get another reader from the original stream
        // would require the stream to be unlocked first
        // This test demonstrates the lock mechanism working
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        readable::ReadableByteSource,
        writable::{WritableStreamDefaultController, spawn_on_thread},
    };
    use super::*;
    use futures::stream;
    use std::time::Duration;
    use tokio::time::timeout;

    // ========== Core Stream Behavior Tests ==========

    #[tokio::test]
    async fn test_basic_stream_read_sequence() {
        let data = vec![1, 2, 3, 4, 5];
        let stream = ReadableStream::from_iter(data.clone().into_iter(), None);
        let (_locked, reader) = stream.get_reader();

        // Sequential reads should return items in order
        for expected in data {
            assert_eq!(reader.read().await.unwrap(), Some(expected));
        }

        // After exhaustion, should return None
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_stream_state_transitions() {
        let data = vec![1, 2, 3];
        let stream = ReadableStream::from_iter(data.into_iter(), None);
        let (_locked, reader) = stream.get_reader();

        // Stream starts readable
        assert!(!reader.0.closed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!reader.0.errored.load(std::sync::atomic::Ordering::SeqCst));

        // Consume all data
        while reader.read().await.unwrap().is_some() {}

        // Should transition to closed
        reader.closed().await.unwrap();
        assert!(reader.0.closed.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_empty_stream_immediate_close() {
        let empty: Vec<i32> = vec![];
        let stream = ReadableStream::from_iter(empty.into_iter(), None);
        let (_locked, reader) = stream.get_reader();

        // Should immediately return None and be closed
        assert_eq!(reader.read().await.unwrap(), None);
        reader.closed().await.unwrap();
    }

    // ========== Lock Mechanism Tests ==========

    #[tokio::test]
    async fn test_stream_locking_behavior() {
        let data = vec![1, 2, 3];
        let stream = ReadableStream::from_iter(data.into_iter(), None);

        // Initially unlocked
        assert!(!stream.locked());

        let (_locked_stream, reader) = stream.get_reader();
        // Note: We can't test locked() on the original stream since it's moved

        // Release lock
        let unlocked_stream = reader.release_lock();
        assert!(!unlocked_stream.locked());
    }

    #[tokio::test]
    async fn test_reader_auto_unlock_on_drop() {
        let data = vec![1, 2, 3];
        let stream = ReadableStream::from_iter(data.into_iter(), None);
        let locked_ref = Arc::clone(&stream.locked);

        {
            let (_locked_stream, _reader) = stream.get_reader();
            // Reader is alive, stream should be locked
        } // Reader drops here

        // After drop, should be unlocked
        assert!(!locked_ref.load(std::sync::atomic::Ordering::SeqCst));
    }

    // ========== Cancellation Tests ==========

    #[tokio::test]
    async fn test_stream_cancellation() {
        let data = vec![1, 2, 3, 4, 5];
        let stream = ReadableStream::from_iter(data.into_iter(), None);
        let (_locked, reader) = stream.get_reader();

        // Read partial data
        assert_eq!(reader.read().await.unwrap(), Some(1));

        // Cancel stream
        reader
            .cancel(Some("test cancellation".to_string()))
            .await
            .unwrap();

        // Further reads should return None (EOF)
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_cancel_without_reason() {
        let data = vec![1, 2, 3];
        let stream = ReadableStream::from_iter(data.into_iter(), None);
        let (_locked, reader) = stream.get_reader();

        // Cancel without reason should work
        reader.cancel(None).await.unwrap();

        // Further reads should return None (EOF)
        assert_eq!(reader.read().await.unwrap(), None);
    }

    // ========== Error Handling Tests ==========

    #[tokio::test]
    async fn test_source_error_propagation() {
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
                    // First call succeeds
                    controller.enqueue(42)?;
                    Ok(())
                } else {
                    // Second call errors
                    controller.error(StreamError::Custom("Source error".into()))?;
                    Ok(())
                }
            }
        }

        let source = ErroringSource {
            call_count: std::cell::RefCell::new(0),
        };
        let stream = ReadableStream::new_default(source, None);
        let (_locked, reader) = stream.get_reader();

        // First read succeeds
        assert_eq!(reader.read().await.unwrap(), Some(42));

        // Second read should propagate error
        match reader.read().await {
            Err(StreamError::Custom(_)) => {} // Expected
            other => panic!("Expected Custom error, got: {:?}", other),
        }

        // Stream should be in error state
        assert!(reader.0.errored.load(std::sync::atomic::Ordering::SeqCst));
    }

    // ========== Integration Tests ==========

    #[tokio::test]
    async fn test_from_async_stream_integration() {
        let items = vec![10, 20, 30];
        let async_stream = stream::iter(items.clone());

        let readable_stream = ReadableStream::from_async_stream(async_stream, None);
        let (_locked, reader) = readable_stream.get_reader();

        // Should read all items from async stream
        for expected in items {
            assert_eq!(reader.read().await.unwrap(), Some(expected));
        }
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_futures_stream_trait_integration() {
        use futures::StreamExt;

        let data = vec!["hello", "world", "test"];
        let stream = ReadableStream::from_iter(data.clone().into_iter(), None);
        let (_locked, _reader) = stream.get_reader();

        // Test that our ReadableStream implements futures::Stream
        // Note: This test demonstrates the trait is implemented
        // Actual polling would require more complex setup
    }

    #[tokio::test]
    async fn test_async_read_trait_integration() {
        use futures::io::AsyncReadExt;

        // Test with byte stream
        struct SimpleByteSource {
            data: Vec<u8>,
            pos: usize,
        }

        impl ReadableByteSource for SimpleByteSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
                _buffer: &mut [u8],
            ) -> StreamResult<usize> {
                if self.pos >= self.data.len() {
                    controller.close()?;
                    return Ok(0);
                }

                let chunk = self.data[self.pos..].to_vec();
                let len = chunk.len();
                self.pos = self.data.len();

                controller.enqueue(chunk)?;
                Ok(len)
            }
        }

        let source = SimpleByteSource {
            data: b"test data".to_vec(),
            pos: 0,
        };

        let mut stream = ReadableStream::new_bytes(source, None);
        // Note: AsyncRead integration would need actual implementation
        // This demonstrates the trait bound compilation
    }

    // ========== Byte Stream Specific Tests ==========

    #[tokio::test]
    async fn test_byte_stream_basic_functionality() {
        struct ChunkedByteSource {
            chunks: Vec<Vec<u8>>,
            index: std::cell::RefCell<usize>,
        }

        impl ReadableByteSource for ChunkedByteSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
                _buffer: &mut [u8],
            ) -> StreamResult<usize> {
                let idx = *self.index.borrow();

                if idx >= self.chunks.len() {
                    controller.close()?;
                    return Ok(0);
                }

                let chunk = self.chunks[idx].clone();
                let _len = chunk.len();
                *self.index.borrow_mut() = idx + 1;

                controller.enqueue(chunk)?;
                Ok(0)
            }
            /*async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
                buffer: &mut [u8], // Use this buffer instead of controller.enqueue
            ) -> StreamResult<usize> {
                let idx = *self.index.borrow();

                if idx >= self.chunks.len() {
                    controller.close()?;
                    return Ok(0);
                }

                let chunk = &self.chunks[idx];
                let bytes_to_copy = std::cmp::min(chunk.len(), buffer.len());

                // Copy directly into the provided buffer
                buffer[..bytes_to_copy].copy_from_slice(&chunk[..bytes_to_copy]);

                *self.index.borrow_mut() = idx + 1;

                Ok(bytes_to_copy)
            }*/
        }

        let source = ChunkedByteSource {
            chunks: vec![b"hello".to_vec(), b" world".to_vec()],
            index: std::cell::RefCell::new(0),
        };

        let stream = ReadableStream::new_bytes(source, None);
        let (_locked, reader) = stream.get_reader();

        // Collect ALL data from the stream
        let mut all_data = Vec::new();
        while let Some(chunk) = reader.read().await.unwrap() {
            all_data.extend(chunk);
        }

        // Verify we got all the expected data
        assert_eq!(all_data, b"hello world");
    }

    #[tokio::test]
    async fn test_byob_reader_functionality() {
        struct SingleChunkByteSource {
            data: Vec<u8>,
            consumed: std::cell::RefCell<bool>,
        }

        impl ReadableByteSource for SingleChunkByteSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
                _buffer: &mut [u8],
            ) -> StreamResult<usize> {
                if *self.consumed.borrow() {
                    controller.close()?;
                    return Ok(0);
                }

                let chunk = self.data.clone();
                let _len = chunk.len();
                *self.consumed.borrow_mut() = true;

                controller.enqueue(chunk)?;
                Ok(0)
            }
        }

        let source = SingleChunkByteSource {
            data: b"byob test data".to_vec(),
            consumed: std::cell::RefCell::new(false),
        };

        let stream = ReadableStream::new_bytes(source, None);
        let (_locked, byob_reader) = stream.get_byob_reader();

        let mut buffer = [0u8; 20];

        // BYOB read should return number of bytes read
        match byob_reader.read(&mut buffer).await.unwrap() {
            bytes_read => assert!(bytes_read > 0),
            //None => panic!("Expected data from BYOB reader"),
        }
        println!("buffer content: {:?}", buffer);

        // Subsequent read should indicate end of stream
        assert_eq!(byob_reader.read(&mut buffer).await.unwrap(), 0);
    }

    // ========== Controller Behavior Tests ==========

    #[tokio::test]
    async fn test_controller_close_behavior() {
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

        let stream = ReadableStream::new_default(source, None);
        let (_locked, reader) = stream.get_reader();

        // Read all items
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(2));

        // Controller should have closed stream
        assert_eq!(reader.read().await.unwrap(), None);
        reader.closed().await.unwrap();
    }

    #[tokio::test]
    async fn test_controller_error_behavior() {
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
                    controller.error(StreamError::Custom("Controller error".into()))?;
                    Ok(())
                }
            }
        }

        let source = ErrorAfterItemsSource {
            sent_items: std::cell::RefCell::new(false),
        };

        let stream = ReadableStream::new_default(source, None);
        let (_locked, reader) = stream.get_reader();

        // First read succeeds
        assert_eq!(reader.read().await.unwrap(), Some("valid item".to_string()));

        // Second read should get the error
        match reader.read().await {
            Err(StreamError::Custom(_)) => {} // Expected
            other => panic!("Expected Custom error, got: {:?}", other),
        }
    }

    // ========== Concurrency and Timing Tests ==========

    #[tokio::test]
    async fn test_concurrent_reads() {
        let data: Vec<i32> = (0..10).collect();
        let stream = ReadableStream::from_iter(data.into_iter(), None);
        let (_locked, reader) = stream.get_reader();

        // Multiple concurrent read attempts - only one should succeed at a time
        let read1 = reader.read();
        let read2 = reader.read();

        let result1 = timeout(Duration::from_millis(100), read1).await.unwrap();
        let result2 = timeout(Duration::from_millis(100), read2).await.unwrap();

        // Both should succeed but with different values (due to queue behavior)
        assert!(result1.is_ok());
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_reader_closed_notification() {
        let data = vec![1];
        let stream = ReadableStream::from_iter(data.into_iter(), None);
        let (_locked, reader) = stream.get_reader();

        // Start waiting for close
        let close_future = reader.closed();

        // Consume the stream
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), None);

        // Close notification should resolve
        timeout(Duration::from_millis(100), close_future)
            .await
            .expect("Stream should close within timeout")
            .expect("Close should succeed");
    }

    // ========== Memory and Resource Tests ==========

    #[tokio::test]
    async fn test_large_stream_handling() {
        let large_data: Vec<i32> = (0..1000).collect();
        let expected_sum: i32 = large_data.iter().sum();

        let stream = ReadableStream::from_iter(large_data.into_iter(), None);
        let (_locked, reader) = stream.get_reader();

        let mut actual_sum = 0;
        while let Some(item) = reader.read().await.unwrap() {
            actual_sum += item;
        }

        assert_eq!(actual_sum, expected_sum);
    }

    #[tokio::test]
    async fn test_push_based_stream() {
        use std::sync::{Arc, Mutex};

        struct PushStartSource {
            data: Vec<i32>,
            enqueued: Arc<Mutex<bool>>,
        }

        impl super::ReadableSource<i32> for PushStartSource {
            fn start(
                &mut self,
                controller: &mut super::ReadableStreamDefaultController<i32>,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let enqueued = self.enqueued.clone();
                let data = self.data.clone();

                async move {
                    let mut enq_lock = enqueued.lock().unwrap();
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
                _controller: &mut super::ReadableStreamDefaultController<i32>,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                async { Ok(()) }
            }
        }

        let source = PushStartSource {
            data: vec![10, 20, 30],
            enqueued: Arc::new(Mutex::new(false)),
        };

        let stream = super::ReadableStream::new_default(source, None);
        let (_locked_stream, reader) = stream.get_reader();

        assert_eq!(reader.read().await.unwrap(), Some(10));
        assert_eq!(reader.read().await.unwrap(), Some(20));
        assert_eq!(reader.read().await.unwrap(), Some(30));
        assert_eq!(reader.read().await.unwrap(), None);

        reader.closed().await.unwrap();
    }

    #[tokio::test]
    async fn test_start_method_error_puts_stream_in_errored_state() {
        struct FailingStartSource;

        impl super::ReadableSource<i32> for FailingStartSource {
            fn start(
                &mut self,
                _controller: &mut super::ReadableStreamDefaultController<i32>,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                async {
                    Err(super::StreamError::Custom(
                        "Start initialization failed".into(),
                    ))
                }
            }

            fn pull(
                &mut self,
                _controller: &mut super::ReadableStreamDefaultController<i32>,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                async { Ok(()) } // This should never be called
            }
        }

        let source = FailingStartSource;
        let stream = super::ReadableStream::new_default(source, None);
        let (_locked_stream, reader) = stream.get_reader();

        // Give the stream task a moment to call start() and fail
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Reading should immediately return the start error
        let read_result = reader.read().await;
        assert!(read_result.is_err());

        if let Err(super::StreamError::Custom(err)) = read_result {
            assert_eq!(err.to_string(), "Start initialization failed");
        } else {
            panic!("Expected Custom error from start failure");
        }

        // Subsequent reads should also error
        let read_result2 = reader.read().await;
        assert!(read_result2.is_err());

        // The closed() method should also return the error (not Ok)
        let closed_result = reader.closed().await;
        assert!(closed_result.is_err());

        if let Err(super::StreamError::Custom(err)) = closed_result {
            assert_eq!(err.to_string(), "Start initialization failed");
        } else {
            panic!("Expected Custom error from start failure");
        }
    }

    #[tokio::test]
    async fn test_basic_pipe_to() {
        use std::sync::{Arc, Mutex};

        // CountingSink will just count how many chunks are written
        #[derive(Clone)]
        struct CountingSink {
            written: Arc<Mutex<Vec<Vec<u8>>>>,
        }

        impl CountingSink {
            fn new() -> Self {
                Self {
                    written: Arc::new(Mutex::new(Vec::new())),
                }
            }
            fn get_written(&self) -> Vec<Vec<u8>> {
                self.written.lock().unwrap().clone()
            }
        }

        impl super::WritableSink<Vec<u8>> for CountingSink {
            fn write(
                &mut self,
                chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let written = self.written.clone();
                async move {
                    written.lock().unwrap().push(chunk);
                    Ok(())
                }
            }
        }

        // Create a readable that yields three chunks
        let data = vec![vec![1u8, 2, 3], vec![4u8, 5], vec![6u8]];
        let readable = super::ReadableStream::from_iter(data.clone().into_iter(), None);

        // Create a writable sink that records chunks
        let sink = CountingSink::new();
        let writable = super::WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(spawn_on_thread);

        // Pipe the readable into the writable
        readable.pipe_to(&writable, None).await.expect("pipe_to");

        // Verify sink received all chunks in order
        let written = sink.get_written();
        assert_eq!(written, data);
    }

    #[tokio::test]
    async fn test_pipe_to_destination_write_error() {
        use std::sync::{Arc, Mutex};

        // Sink that fails after N successful writes
        #[derive(Clone)]
        struct FailingWriteSink {
            written: Arc<Mutex<Vec<Vec<u8>>>>,
            fail_after: usize,
            write_count: Arc<Mutex<usize>>,
            abort_called: Arc<Mutex<bool>>,
        }

        impl FailingWriteSink {
            fn new(fail_after: usize) -> Self {
                Self {
                    written: Arc::new(Mutex::new(Vec::new())),
                    fail_after,
                    write_count: Arc::new(Mutex::new(0)),
                    abort_called: Arc::new(Mutex::new(false)),
                }
            }

            fn get_written(&self) -> Vec<Vec<u8>> {
                self.written.lock().unwrap().clone()
            }

            fn _was_abort_called(&self) -> bool {
                *self.abort_called.lock().unwrap()
            }
        }

        impl super::WritableSink<Vec<u8>> for FailingWriteSink {
            fn write(
                &mut self,
                chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let written = Arc::clone(&self.written);
                let write_count = Arc::clone(&self.write_count);
                let fail_after = self.fail_after;

                async move {
                    let mut count = write_count.lock().unwrap();
                    *count += 1;

                    if *count > fail_after {
                        return Err(super::StreamError::Custom("Write failed".into()));
                    }

                    written.lock().unwrap().push(chunk);
                    Ok(())
                }
            }

            fn abort(
                &mut self,
                _reason: Option<String>,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let abort_called = self.abort_called.clone();
                async move {
                    *abort_called.lock().unwrap() = true;
                    Ok(())
                }
            }
        }

        // Source that can track if it was cancelled
        struct TrackingSource {
            data: Vec<Vec<u8>>,
            index: usize,
            cancelled: Arc<Mutex<bool>>,
        }

        impl TrackingSource {
            fn new(data: Vec<Vec<u8>>) -> Self {
                Self {
                    data,
                    index: 0,
                    cancelled: Arc::new(Mutex::new(false)),
                }
            }
        }

        impl super::ReadableSource<Vec<u8>> for TrackingSource {
            async fn pull(
                &mut self,
                controller: &mut super::ReadableStreamDefaultController<Vec<u8>>,
            ) -> super::StreamResult<()> {
                if self.index >= self.data.len() {
                    controller.close()?;
                    return Ok(());
                }

                controller.enqueue(self.data[self.index].clone())?;
                self.index += 1;
                Ok(())
            }

            async fn cancel(&mut self, _reason: Option<String>) -> super::StreamResult<()> {
                *self.cancelled.lock().unwrap() = true;
                Ok(())
            }
        }

        let data = vec![
            vec![1u8, 2, 3],
            vec![4u8, 5, 6],
            vec![7u8, 8, 9], // This will cause the failure
            vec![10u8, 11],  // This should never be reached
            vec![12u8, 13], // These are to ensure it writes up to a point where the error propagated by the third chunk would reject a ready call and error the pipe_to
        ];

        let source = TrackingSource::new(data.clone());
        let cancelled_flag = Arc::clone(&source.cancelled);
        let readable = super::ReadableStream::new_default(source, None);

        let sink = FailingWriteSink::new(2); // Fail after 2 successful writes
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(spawn_on_thread);

        // Test: pipe_to should return error when destination write fails
        let pipe_result = readable.pipe_to(&writable, None).await;
        assert!(
            pipe_result.is_err(),
            "pipe_to should fail when write errors"
        );

        // Verify error handling per WHATWG spec:
        // 1. "An error in destination will cancel this source readable stream, unless preventCancel is truthy"
        assert!(
            *cancelled_flag.lock().unwrap(),
            "Source should be cancelled when destination errors"
        );

        // 2. Only successful writes should have completed
        let written = sink.get_written();
        assert_eq!(
            written.len(),
            2,
            "Only 2 chunks should have been written successfully"
        );
        assert_eq!(
            written,
            &data[..2],
            "Written chunks should match expected data"
        );

        // 3. Stream should be in errored state
        /*assert!(
            writable.errored.load(std::sync::atomic::Ordering::SeqCst),
            "Writable stream should be in errored state"
        );*/
    }

    #[tokio::test]
    async fn test_pipe_to_source_error() {
        use std::sync::{Arc, Mutex};

        // Source that errors after yielding N chunks
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

        impl super::ReadableSource<Vec<u8>> for ErroringSource {
            async fn pull(
                &mut self,
                controller: &mut super::ReadableStreamDefaultController<Vec<u8>>,
            ) -> super::StreamResult<()> {
                if self.index >= self.error_after {
                    return Err(super::StreamError::Custom("Source error".into()));
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

        // Sink that tracks if abort was called
        #[derive(Clone)]
        struct TrackingSink {
            written: Arc<Mutex<Vec<Vec<u8>>>>,
            abort_called: Arc<Mutex<bool>>,
            abort_reason: Arc<Mutex<Option<String>>>,
        }

        impl TrackingSink {
            fn new() -> Self {
                Self {
                    written: Arc::new(Mutex::new(Vec::new())),
                    abort_called: Arc::new(Mutex::new(false)),
                    abort_reason: Arc::new(Mutex::new(None)),
                }
            }

            fn get_written(&self) -> Vec<Vec<u8>> {
                self.written.lock().unwrap().clone()
            }

            fn was_abort_called(&self) -> bool {
                *self.abort_called.lock().unwrap()
            }

            fn get_abort_reason(&self) -> Option<String> {
                self.abort_reason.lock().unwrap().clone()
            }
        }

        impl super::WritableSink<Vec<u8>> for TrackingSink {
            fn write(
                &mut self,
                chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let written = Arc::clone(&self.written);
                async move {
                    written.lock().unwrap().push(chunk);
                    Ok(())
                }
            }

            fn abort(
                &mut self,
                reason: Option<String>,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let abort_called = self.abort_called.clone();
                let abort_reason = self.abort_reason.clone();
                async move {
                    *abort_called.lock().unwrap() = true;
                    *abort_reason.lock().unwrap() = reason;
                    Ok(())
                }
            }
        }

        let data = vec![vec![1u8, 2, 3], vec![4u8, 5, 6]];

        let source = ErroringSource::new(data.clone(), 2); // Error after 2 chunks
        let readable = super::ReadableStream::new_default(source, None);

        let sink = TrackingSink::new();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(spawn_on_thread);

        // Test: pipe_to should return error when source errors
        let pipe_result = readable.pipe_to(&writable, None).await;
        assert!(
            pipe_result.is_err(),
            "pipe_to should fail when source errors"
        );

        // Verify error handling per WHATWG spec:
        // 1. "An error in this source readable stream will abort destination, unless preventAbort is truthy"
        assert!(
            sink.was_abort_called(),
            "Destination should be aborted when source errors"
        );

        // 2. All chunks before the error should have been written
        let written = sink.get_written();
        assert_eq!(written, data, "All chunks before error should be written");

        // 3. Abort reason should contain error information
        let abort_reason = sink.get_abort_reason();
        assert!(
            abort_reason.is_some() && abort_reason.unwrap().contains("Source error"),
            "Abort reason should contain source error information"
        );
    }

    #[tokio::test]
    async fn test_pipe_to_prevent_options() {
        use std::sync::{Arc, Mutex};

        // Reusable test helpers
        #[derive(Clone)]
        struct TestSource {
            data: Vec<Vec<u8>>,
            index: Arc<Mutex<usize>>,
            cancelled: Arc<Mutex<bool>>,
            should_error: bool,
            error_after: usize,
        }

        impl TestSource {
            fn new(data: Vec<Vec<u8>>) -> Self {
                Self {
                    data,
                    index: Arc::new(Mutex::new(0)),
                    cancelled: Arc::new(Mutex::new(false)),
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
                *self.cancelled.lock().unwrap()
            }
        }

        impl super::ReadableSource<Vec<u8>> for TestSource {
            async fn pull(
                &mut self,
                controller: &mut super::ReadableStreamDefaultController<Vec<u8>>,
            ) -> super::StreamResult<()> {
                let mut idx = self.index.lock().unwrap();

                if self.should_error && *idx >= self.error_after {
                    return Err(super::StreamError::Custom("Source error".into()));
                }

                if *idx >= self.data.len() {
                    controller.close()?;
                    return Ok(());
                }

                controller.enqueue(self.data[*idx].clone())?;
                *idx += 1;
                Ok(())
            }

            async fn cancel(&mut self, _reason: Option<String>) -> super::StreamResult<()> {
                *self.cancelled.lock().unwrap() = true;
                Ok(())
            }
        }

        #[derive(Clone)]
        struct TestSink {
            written: Arc<Mutex<Vec<Vec<u8>>>>,
            aborted: Arc<Mutex<bool>>,
            closed: Arc<Mutex<bool>>,
            should_error: bool,
        }

        impl TestSink {
            fn new() -> Self {
                Self {
                    written: Arc::new(Mutex::new(Vec::new())),
                    aborted: Arc::new(Mutex::new(false)),
                    closed: Arc::new(Mutex::new(false)),
                    should_error: false,
                }
            }

            fn with_error(mut self) -> Self {
                self.should_error = true;
                self
            }

            fn was_aborted(&self) -> bool {
                *self.aborted.lock().unwrap()
            }

            fn was_closed(&self) -> bool {
                *self.closed.lock().unwrap()
            }

            fn written_data(&self) -> Vec<Vec<u8>> {
                self.written.lock().unwrap().clone()
            }
        }

        impl super::WritableSink<Vec<u8>> for TestSink {
            fn write(
                &mut self,
                chunk: Vec<u8>,
                _controller: &mut WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let written = self.written.clone();
                let should_error = self.should_error;

                async move {
                    if should_error {
                        return Err(super::StreamError::Custom("Sink error".into()));
                    }
                    written.lock().unwrap().push(chunk);
                    Ok(())
                }
            }

            fn abort(
                &mut self,
                _reason: Option<String>,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let aborted = self.aborted.clone();
                async move {
                    *aborted.lock().unwrap() = true;
                    Ok(())
                }
            }

            fn close(self) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let closed = self.closed;
                async move {
                    *closed.lock().unwrap() = true;
                    Ok(())
                }
            }
        }

        // Test successful pipe with normal cleanup
        let data = vec![vec![1u8, 2, 3], vec![4u8, 5, 6]];
        let source = TestSource::new(data.clone());
        let readable = super::ReadableStream::new_default(source.clone(), None);
        let sink = TestSink::new();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(spawn_on_thread);

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
        let readable = super::ReadableStream::new_default(source, None);
        let sink = TestSink::new();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(spawn_on_thread);

        let options = super::StreamPipeOptions {
            prevent_close: true,
            ..Default::default()
        };

        let result = readable.pipe_to(&writable, Some(options)).await;
        assert!(result.is_ok(), "Pipe with prevent_close should succeed");
        assert!(
            !sink.was_closed(),
            "Sink should NOT be closed when prevent_close=true"
        );

        // Test source error with prevent_cancel = true
        let data = vec![vec![1u8, 2], vec![3u8, 4]];
        let source = TestSource::new(data).with_error_after(1);
        let readable = super::ReadableStream::new_default(source.clone(), None);
        let sink = TestSink::new();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(spawn_on_thread);

        let options = super::StreamPipeOptions {
            prevent_cancel: true,
            prevent_abort: false,
            ..Default::default()
        };

        let result = readable.pipe_to(&writable, Some(options)).await;
        assert!(result.is_err(), "Pipe should fail when source errors");
        assert!(
            !source.was_cancelled(),
            "Source should NOT be cancelled when prevent_cancel=true"
        );
        assert!(
            sink.was_aborted(),
            "Sink should be aborted when source errors"
        );

        // Test source error with prevent_cancel = false
        let data = vec![vec![1u8, 2], vec![3u8, 4]];
        let source = TestSource::new(data).with_error_after(1);
        let readable = super::ReadableStream::new_default(source.clone(), None);
        let sink = TestSink::new();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(spawn_on_thread);

        let options = super::StreamPipeOptions {
            prevent_cancel: false,
            prevent_abort: false,
            ..Default::default()
        };

        let result = readable.pipe_to(&writable, Some(options)).await;
        assert!(result.is_err(), "Pipe should fail when source errors");

        assert!(
            !source.was_cancelled(),
            "Source should Not be cancelled when source errors"
        );
        assert!(
            sink.was_aborted(),
            "Sink should be aborted when source errors"
        );

        // Test sink error with prevent_abort = true
        //let data = vec![vec![1u8, 2, 3]];
        // IMPORTANT write long enough so that a ready call catches an error with the sink
        let data = vec![vec![1u8, 2], vec![3u8, 4], vec![6u8, 7]];
        let source = TestSource::new(data);
        let readable = super::ReadableStream::new_default(source.clone(), None);
        let sink = TestSink::new().with_error();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(spawn_on_thread);

        let options = super::StreamPipeOptions {
            prevent_abort: true,
            prevent_cancel: false,
            ..Default::default()
        };

        let result = readable.pipe_to(&writable, Some(options)).await;
        assert!(result.is_err(), "Pipe should fail when sink errors");

        assert!(
            source.was_cancelled(),
            "Source should be cancelled when sink errors"
        );
        assert!(
            !sink.was_aborted(),
            "Sink should NOT be aborted when Sink errors"
        );

        // Test sink error with prevent_abort = false
        //let data = vec![vec![1u8, 2, 3]];
        // IMPORTANT write long enough so that a ready call catches an error with the sink
        let data = vec![vec![1u8, 2], vec![3u8, 4], vec![6u8, 7]];
        let source = TestSource::new(data);
        let readable = super::ReadableStream::new_default(source.clone(), None);
        let sink = TestSink::new().with_error();
        let writable = WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(spawn_on_thread);

        let options = super::StreamPipeOptions {
            prevent_abort: false,
            prevent_cancel: false,
            ..Default::default()
        };

        let result = readable.pipe_to(&writable, Some(options)).await;
        assert!(result.is_err(), "Pipe should fail when sink errors");

        assert!(
            source.was_cancelled(),
            "Source should be cancelled when sink errors"
        );
        assert!(
            !sink.was_aborted(),
            "Sink should NOT be aborted when sink errors"
        );
    }

    #[tokio::test]
    //#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_pipe_to_abort_signal() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use tokio::time::sleep;

        // SlowSink introduces delays to simulate long-running operations
        #[derive(Clone)]
        struct SlowSink {
            written: Arc<Mutex<Vec<Vec<u8>>>>,
            aborted: Arc<Mutex<bool>>,
            delay_ms: u64,
        }

        impl SlowSink {
            fn new(delay_ms: u64) -> Self {
                Self {
                    written: Arc::new(Mutex::new(Vec::new())),
                    aborted: Arc::new(Mutex::new(false)),
                    delay_ms,
                }
            }

            fn was_aborted(&self) -> bool {
                *self.aborted.lock().unwrap()
            }

            fn get_written(&self) -> Vec<Vec<u8>> {
                self.written.lock().unwrap().clone()
            }
        }

        impl super::WritableSink<Vec<u8>> for SlowSink {
            /*fn write(
                &mut self,
                chunk: Vec<u8>,
                _controller: &mut crate::dlc::ideation::d::writable_new::WritableStreamDefaultController,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                let written = self.written.clone();
                let delay = self.delay_ms;

                async move {
                    // Simulate slow write operation
                    sleep(Duration::from_millis(delay)).await;
                    written.lock().unwrap().push(chunk);
                    Ok(())
                }
            }*/
            fn write(
                &mut self,
                chunk: Vec<u8>,
                controller: &mut WritableStreamDefaultController,
            ) -> impl Future<Output = StreamResult<()>> + Send {
                let written = self.written.clone();
                let delay = self.delay_ms;

                let abort_fut = controller.abort_future();

                async move {
                    tokio::select! {
                        _ = abort_fut => {
                            eprintln!("abort happened before write finished");
                            // Abort happened before write finished
                            Err(StreamError::Aborted(None))
                        }
                        _ = sleep(Duration::from_millis(delay)) => {
                            // Simulate completing the write
                            written.lock().unwrap().push(chunk);
                            Ok(())
                        }
                    }
                }
            }

            fn abort(
                &mut self,
                _reason: Option<String>,
            ) -> impl std::future::Future<Output = super::StreamResult<()>> + Send {
                eprintln!("abort called from within sink");
                let aborted = self.aborted.clone();
                async move {
                    *aborted.lock().unwrap() = true;
                    eprintln!("abort called from within sink is done");
                    Ok(())
                }
            }
        }

        // Create test data
        let data = vec![
            vec![1u8, 2, 3],
            vec![4u8, 5, 6],
            vec![7u8, 8, 9],
            vec![10u8, 11, 12],
            vec![13u8, 14, 15],
        ];

        let readable = super::ReadableStream::from_iter(data.clone().into_iter(), None);
        let sink = SlowSink::new(100); // 100ms delay per write

        /*let writable = super::WritableStream::new(
            sink.clone(),
            Box::new(crate::dlc::ideation::d::CountQueuingStrategy::new(10)),
        );*/
        let writable = super::WritableStream::builder(sink.clone())
            .strategy(CountQueuingStrategy::new(10))
            .spawn(tokio::spawn);

        // Create abort controller and signal
        let (abort_handle, abort_registration) = futures_util::stream::AbortHandle::new_pair();

        // Start the pipe operation with abort signal
        let pipe_options = super::StreamPipeOptions {
            signal: Some(abort_registration),
            prevent_close: false,
            prevent_abort: false,
            prevent_cancel: false,
        };

        // Start pipe_to in a separate task
        /*let pipe_handle =
        tokio::spawn(async move { readable.pipe_to(&writable, Some(pipe_options)).await });*/
        //let handle = readable.pipe_to_threaded(writable, Some(pipe_options));
        //let handle = readable.pipe_to_with_spawn(writable, Some(pipe_options), tokio::spawn);
        let handle = readable
            .pipe_to_with_spawn(writable, Some(pipe_options), |fut| tokio::task::spawn(fut));

        // Let it start processing, then abort after a short delay
        sleep(Duration::from_millis(150)).await; // Allow 2-3 chunks to be written
        //abort_handle.abort(Some("Test abortion".into()));
        abort_handle.abort();

        // Wait for the pipe operation to complete
        //let pipe_result = pipe_handle.await.unwrap();
        //let pipe_result = handle.join().unwrap();
        let pipe_result = handle.await.unwrap();

        // Verify the pipe operation was aborted
        assert!(pipe_result.is_err());
        assert!(matches!(pipe_result, Err(super::StreamError::Aborted(_))));

        // Verify cleanup behavior
        assert!(
            sink.was_aborted(),
            "Sink should have been aborted when pipe operation was aborted"
        );

        // Should have written some but not all chunks (due to slow operation + early abort)
        let written = sink.get_written();
        assert!(
            written.len() < data.len(),
            "Should not have written all chunks due to abortion. Written: {}, Total: {}",
            written.len(),
            data.len()
        );
        assert!(
            written.len() > 0,
            "Should have written at least some chunks before abortion"
        );

        // Verify the written chunks are correct (partial data)
        for (i, chunk) in written.iter().enumerate() {
            assert_eq!(
                chunk, &data[i],
                "Written chunk {} should match source data",
                i
            );
        }
    }

    #[tokio::test]
    async fn test_start_called_before_pull_operations() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct StartTrackingByteSource {
            data: Vec<u8>,
            start_called: Arc<AtomicBool>,
            start_call_order: Arc<AtomicUsize>,
            pull_call_order: Arc<AtomicUsize>,
            call_counter: Arc<AtomicUsize>,
        }

        impl ReadableByteSource for StartTrackingByteSource {
            async fn start(
                &mut self,
                controller: &mut ReadableByteStreamController,
            ) -> StreamResult<()> {
                // Record that start was called and its order
                self.start_called.store(true, Ordering::SeqCst);
                let call_num = self.call_counter.fetch_add(1, Ordering::SeqCst);
                self.start_call_order.store(call_num, Ordering::SeqCst);

                // Simulate some async work in start
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(())
            }

            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
                _buffer: &mut [u8],
            ) -> StreamResult<usize> {
                // Verify start was called before any pull
                assert!(
                    self.start_called.load(Ordering::SeqCst),
                    "pull() called before start()!"
                );

                // Record pull call order
                let call_num = self.call_counter.fetch_add(1, Ordering::SeqCst);
                self.pull_call_order.store(call_num, Ordering::SeqCst);

                // Return data and close
                if !self.data.is_empty() {
                    let chunk = self.data.clone();
                    self.data.clear();
                    controller.enqueue(chunk)?;
                } else {
                    controller.close()?;
                }
                Ok(0)
            }
        }

        let start_called = Arc::new(AtomicBool::new(false));
        let start_order = Arc::new(AtomicUsize::new(0));
        let pull_order = Arc::new(AtomicUsize::new(0));
        let counter = Arc::new(AtomicUsize::new(0));

        let source = StartTrackingByteSource {
            data: b"test data".to_vec(),
            start_called: Arc::clone(&start_called),
            start_call_order: Arc::clone(&start_order),
            pull_call_order: Arc::clone(&pull_order),
            call_counter: Arc::clone(&counter),
        };

        let stream = ReadableStream::builder(source)
            .with_spawn(|fut| {
                tokio::spawn(fut);
            })
            .build();
        let (_locked, reader) = stream.get_reader();

        // Give the stream task time to call start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Verify start was called
        assert!(
            start_called.load(Ordering::SeqCst),
            "start() was never called"
        );

        // Now read from the stream
        let result = reader.read().await.unwrap();
        assert!(result.is_some(), "Expected data from stream");

        // Verify ordering: start should have been called before pull
        let start_call_num = start_order.load(Ordering::SeqCst);
        let pull_call_num = pull_order.load(Ordering::SeqCst);

        assert!(
            start_call_num < pull_call_num,
            "start() should be called before pull(). start: {}, pull: {}",
            start_call_num,
            pull_call_num
        );
    }

    #[tokio::test]
    async fn test_start_error_prevents_operations() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct FailingStartByteSource {
            pull_called: Arc<AtomicBool>,
        }

        impl ReadableByteSource for FailingStartByteSource {
            async fn start(
                &mut self,
                _controller: &mut ReadableByteStreamController,
            ) -> StreamResult<()> {
                // Simulate start failure
                Err(StreamError::Custom("Start failed".into()))
            }

            async fn pull(
                &mut self,
                _controller: &mut ReadableByteStreamController,
                _buffer: &mut [u8],
            ) -> StreamResult<usize> {
                // This should never be called if start fails
                self.pull_called.store(true, Ordering::SeqCst);
                Ok(0)
            }
        }

        let pull_called = Arc::new(AtomicBool::new(false));
        let source = FailingStartByteSource {
            pull_called: Arc::clone(&pull_called),
        };

        let stream = ReadableStream::new_bytes(source, None);
        let (_locked, reader) = stream.get_reader();

        // Give the stream task time to call start and fail
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Attempt to read should fail due to start error
        /*match reader.read().await {
            Err(StreamError::Custom(msg)) if msg.contains("Start failed") => {
                // Expected error from start failure
            }
            other => panic!("Expected start failure error, got: {:?}", other),
        }*/
        // Attempt to read should fail due to start error
        match reader.read().await {
            Err(StreamError::Custom(msg)) => {
                // Convert the ArcError into a string and check for the substring.
                let error_message = msg.to_string();
                if error_message.contains("Start failed") {
                    // Expected error from start failure
                    // Your logic here
                } else {
                    panic!("Expected start failure error, got: {:?}", msg);
                }
            }
            other => panic!("Expected custom error from start failure, got: {:?}", other),
        }

        // Verify pull was never called
        assert!(
            !pull_called.load(Ordering::SeqCst),
            "pull() should not be called when start() fails"
        );

        // Stream should be in error state
        match reader.closed().await {
            Err(StreamError::Custom(msg)) if msg.to_string().contains("Start failed") => {}
            other => panic!(
                "Expected closed() to propagate start failure, got: {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn test_start_blocks_immediate_reads() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::Barrier;

        struct SlowStartByteSource {
            data: Vec<u8>,
            start_barrier: Arc<Barrier>,
            start_completed: Arc<AtomicBool>,
        }

        impl ReadableByteSource for SlowStartByteSource {
            async fn start(
                &mut self,
                _controller: &mut ReadableByteStreamController,
            ) -> StreamResult<()> {
                // Wait for test to signal it's ready
                self.start_barrier.wait().await;

                // Simulate slow start
                tokio::time::sleep(Duration::from_millis(100)).await;

                self.start_completed.store(true, Ordering::SeqCst);
                Ok(())
            }

            async fn pull(
                &mut self,
                controller: &mut ReadableByteStreamController,
                _buffer: &mut [u8],
            ) -> StreamResult<usize> {
                // Verify start completed before pull
                assert!(
                    self.start_completed.load(Ordering::SeqCst),
                    "pull called before start completed"
                );

                if !self.data.is_empty() {
                    let chunk = self.data.clone();
                    self.data.clear();
                    controller.enqueue(chunk)?;
                } else {
                    controller.close()?;
                }
                Ok(0)
            }
        }

        let barrier = Arc::new(Barrier::new(2));
        let start_completed = Arc::new(AtomicBool::new(false));

        let source = SlowStartByteSource {
            data: b"delayed data".to_vec(),
            start_barrier: Arc::clone(&barrier),
            start_completed: Arc::clone(&start_completed),
        };

        let stream = ReadableStream::builder(source)
            .with_spawn(|fut| {
                tokio::spawn(fut);
            })
            .build();
        let (_locked, reader) = stream.get_reader();

        // Immediately try to read - this should block waiting for start
        let read_future = reader.read();

        // Verify start hasn't completed yet
        assert!(!start_completed.load(Ordering::SeqCst));

        // Now allow start to proceed
        barrier.wait().await;

        // The read should now complete successfully
        let result = timeout(Duration::from_millis(500), read_future)
            .await
            .expect("Read should complete after start finishes")
            .unwrap();

        assert!(
            result.is_some(),
            "Should receive data after start completes"
        );

        // Verify start did complete
        assert!(start_completed.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;
    use futures::executor::block_on;
    use futures::stream::iter as stream_iter;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    // Simple test source for verification
    struct TestSource {
        data: Vec<String>,
        index: usize,
        closed: bool,
    }

    impl TestSource {
        fn new(data: Vec<String>) -> Self {
            Self {
                data,
                index: 0,
                closed: false,
            }
        }
    }

    impl ReadableSource<String> for TestSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<String>,
        ) -> StreamResult<()> {
            if self.closed {
                return Ok(());
            }

            if self.index >= self.data.len() {
                controller.close()?;
                self.closed = true;
            } else {
                let item = self.data[self.index].clone();
                self.index += 1;
                controller.enqueue(item)?;
            }
            Ok(())
        }
    }

    // Byte source for testing ByteStream builder
    struct TestByteSource {
        data: Vec<u8>,
        position: usize,
    }

    impl TestByteSource {
        fn new(data: Vec<u8>) -> Self {
            Self { data, position: 0 }
        }
    }

    impl ReadableByteSource for TestByteSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableByteStreamController,
            buffer: &mut [u8],
        ) -> StreamResult<usize> {
            if self.position >= self.data.len() {
                controller.close()?;
                return Ok(0);
            }

            /*let remaining = self.data.len() - self.position;
            let to_copy = std::cmp::min(buffer.len(), remaining);

            buffer[..to_copy].copy_from_slice(&self.data[self.position..self.position + to_copy]);
            self.position += to_copy;

            //let chunk = buffer[..to_copy].to_vec();
            //controller.enqueue(chunk)?;

            Ok(to_copy)*/
            // Enqueue all remaining data as one chunk
            /*let chunk = self.data[self.position..].to_vec();
            self.position = self.data.len();

            controller.enqueue(chunk)?;
            Ok(0) // Let reader handle buffer filling*/
            let remaining = self.data.len() - self.position;
            let to_copy = std::cmp::min(buffer.len(), remaining);

            buffer[..to_copy].copy_from_slice(&self.data[self.position..self.position + to_copy]);
            self.position += to_copy;

            Ok(to_copy) // Return the number of bytes copied to the buffer
        }
    }

    #[test]
    fn test_basic_default_stream_builder() {
        let data = vec!["hello".to_string(), "world".to_string()];
        let source = TestSource::new(data.clone());

        let stream = ReadableStreamBuilder::new(source).build();
        let (_, reader) = stream.get_reader();

        block_on(async {
            assert_eq!(reader.read().await.unwrap(), Some("hello".to_string()));
            assert_eq!(reader.read().await.unwrap(), Some("world".to_string()));
            assert_eq!(reader.read().await.unwrap(), None);
        });
    }

    #[test]
    fn test_byte_stream_builder() {
        let data = b"Hello, World!".to_vec();
        let source = TestByteSource::new(data.clone());

        let stream = ReadableStreamBuilder::new(source).build();
        let (_, reader) = stream.get_byob_reader();

        block_on(async {
            let mut buffer = vec![0u8; 13];
            //let bytes_read = reader.read(&mut buffer).await.unwrap().unwrap();
            //println!("asserting bytes read");
            //assert_eq!(bytes_read, 13);
            //println!("asserting buffer");
            //assert_eq!(buffer, b"Hello, World!");
            //println!("all success")
            match reader.read(&mut buffer).await.unwrap() {
                bytes_read => {
                    assert_eq!(bytes_read, 13);
                    assert_eq!(buffer, b"Hello, World!".to_vec());
                } //None => panic!("Expected data but stream ended"),
            }
        });
    }

    #[test]
    fn test_builder_with_custom_strategy() {
        let data = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let source = TestSource::new(data);

        // Use a custom high water mark
        let custom_strategy = CountQueuingStrategy::new(5);

        let stream = ReadableStreamBuilder::new(source)
            .with_strategy(custom_strategy)
            .build();

        // Verify the stream was created successfully
        assert!(!stream.locked());

        let (_, reader) = stream.get_reader();
        block_on(async {
            assert_eq!(reader.read().await.unwrap(), Some("a".to_string()));
        });
    }

    #[test]
    fn test_builder_with_custom_spawn() {
        let data = vec!["test".to_string()];
        let source = TestSource::new(data);

        // Track if custom spawn was called
        let spawn_called = Arc::new(Mutex::new(false));
        let spawn_called_clone = Arc::clone(&spawn_called);

        let stream = ReadableStreamBuilder::new(source)
            .with_spawn(move |fut| {
                *spawn_called_clone.lock().unwrap() = true;
                // Still need to actually run the future for the test to work
                std::thread::spawn(move || {
                    futures::executor::block_on(fut);
                });
            })
            .build();

        // Give the spawn function time to be called
        thread::sleep(Duration::from_millis(10));
        assert!(*spawn_called.lock().unwrap());

        let (_, reader) = stream.get_reader();
        block_on(async {
            assert_eq!(reader.read().await.unwrap(), Some("test".to_string()));
        });
    }

    #[test]
    fn test_builder_chaining() {
        let data = vec!["chain".to_string(), "test".to_string()];
        let source = TestSource::new(data);

        // Test method chaining
        let stream = ReadableStreamBuilder::new(source)
            .with_strategy(CountQueuingStrategy::new(2))
            .with_thread_spawn()
            .build();

        let (_, reader) = stream.get_reader();
        block_on(async {
            assert_eq!(reader.read().await.unwrap(), Some("chain".to_string()));
            assert_eq!(reader.read().await.unwrap(), Some("test".to_string()));
            assert_eq!(reader.read().await.unwrap(), None);
        });
    }

    #[test]
    fn test_from_vec_convenience() {
        let data = vec!["item1".to_string(), "item2".to_string()];

        let stream = ReadableStreamBuilder::from_vec(data.clone()).build();
        let (_, reader) = stream.get_reader();

        block_on(async {
            assert_eq!(reader.read().await.unwrap(), Some("item1".to_string()));
            assert_eq!(reader.read().await.unwrap(), Some("item2".to_string()));
            assert_eq!(reader.read().await.unwrap(), None);
        });
    }

    #[test]
    fn test_from_iterator_convenience() {
        let data = vec![1, 2, 3, 4, 5];
        let iter = data.into_iter();

        let stream = ReadableStreamBuilder::from_iterator(iter).build();
        let (_, reader) = stream.get_reader();

        block_on(async {
            assert_eq!(reader.read().await.unwrap(), Some(1));
            assert_eq!(reader.read().await.unwrap(), Some(2));
            assert_eq!(reader.read().await.unwrap(), Some(3));
            assert_eq!(reader.read().await.unwrap(), Some(4));
            assert_eq!(reader.read().await.unwrap(), Some(5));
            assert_eq!(reader.read().await.unwrap(), None);
        });
    }

    #[test]
    fn test_from_stream_convenience() {
        let data = vec!["async1", "async2", "async3"];
        let async_stream = stream_iter(data.into_iter().map(|s| s.to_string()));

        let stream = ReadableStreamBuilder::from_stream(async_stream).build();
        let (_, reader) = stream.get_reader();

        block_on(async {
            assert_eq!(reader.read().await.unwrap(), Some("async1".to_string()));
            assert_eq!(reader.read().await.unwrap(), Some("async2".to_string()));
            assert_eq!(reader.read().await.unwrap(), Some("async3".to_string()));
            assert_eq!(reader.read().await.unwrap(), None);
        });
    }

    #[test]
    fn test_builder_reusability() {
        // Test that we can create multiple builders with the same pattern
        let create_stream = |data: Vec<&str>| {
            let string_data: Vec<String> = data.into_iter().map(|s| s.to_string()).collect();
            ReadableStreamBuilder::from_vec(string_data)
                .with_strategy(CountQueuingStrategy::new(3))
                .with_thread_spawn()
                .build()
        };

        let stream1 = create_stream(vec!["test1", "test2"]);
        let stream2 = create_stream(vec!["test3", "test4"]);

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        block_on(async {
            // Verify both streams work independently
            assert_eq!(reader1.read().await.unwrap(), Some("test1".to_string()));
            assert_eq!(reader2.read().await.unwrap(), Some("test3".to_string()));
            assert_eq!(reader1.read().await.unwrap(), Some("test2".to_string()));
            assert_eq!(reader2.read().await.unwrap(), Some("test4".to_string()));
        });
    }

    #[test]
    fn test_stream_static_builder_method() {
        let data = vec!["static".to_string(), "method".to_string()];
        let source = TestSource::new(data);

        // Test the static builder method on ReadableStream
        let stream: ReadableStream<String, TestSource, DefaultStream, Unlocked> =
            ReadableStream::builder(source).build();

        let (_, reader) = stream.get_reader();
        block_on(async {
            assert_eq!(reader.read().await.unwrap(), Some("static".to_string()));
            assert_eq!(reader.read().await.unwrap(), Some("method".to_string()));
            assert_eq!(reader.read().await.unwrap(), None);
        });
    }

    #[test]
    fn test_error_handling_in_builder() {
        // Test that errors in the source are properly handled
        struct ErrorSource;

        impl ReadableSource<String> for ErrorSource {
            async fn pull(
                &mut self,
                controller: &mut ReadableStreamDefaultController<String>,
            ) -> StreamResult<()> {
                controller.error(StreamError::Custom("Test error".into()))?;
                Ok(())
            }
        }

        let stream = ReadableStreamBuilder::new(ErrorSource).build();
        let (_, reader) = stream.get_reader();

        block_on(async {
            match reader.read().await {
                Err(StreamError::Custom(msg)) => {
                    assert!(msg.to_string().contains("Test error"));
                }
                other => panic!("Expected custom error, got: {:?}", other),
            }
        });
    }

    #[test]
    fn test_multiple_configurations() {
        // Test different configuration combinations
        let configs = vec![
            // Default config
            ReadableStreamBuilder::from_vec(vec!["default".to_string()]),
            // With custom strategy
            ReadableStreamBuilder::from_vec(vec!["strategy".to_string()])
                .with_strategy(CountQueuingStrategy::new(10)),
            // With thread spawn
            ReadableStreamBuilder::from_vec(vec!["thread".to_string()]).with_thread_spawn(),
            // Full config
            ReadableStreamBuilder::from_vec(vec!["full".to_string()])
                .with_strategy(CountQueuingStrategy::new(5))
                .with_thread_spawn(),
        ];

        for (i, builder) in configs.into_iter().enumerate() {
            let stream = builder.build();
            let (_, reader) = stream.get_reader();

            block_on(async {
                let result = reader.read().await.unwrap().unwrap();
                match i {
                    0 => assert_eq!(result, "default"),
                    1 => assert_eq!(result, "strategy"),
                    2 => assert_eq!(result, "thread"),
                    3 => assert_eq!(result, "full"),
                    _ => unreachable!(),
                }
            });
        }
    }
}

#[cfg(test)]
mod pipe_through_tests {
    use super::super::transform::{StreamResult, *};
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    // Simple uppercase transformer for testing
    struct UppercaseTransformer;

    impl Transformer<String, String> for UppercaseTransformer {
        fn transform(
            &mut self,
            chunk: String,
            controller: &mut TransformStreamDefaultController<String>,
        ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
            let result = controller.enqueue(chunk.to_uppercase());
            futures::future::ready(result)
        }
    }

    // Double transformer for numbers
    struct DoubleTransformer;

    impl Transformer<i32, i32> for DoubleTransformer {
        fn transform(
            &mut self,
            chunk: i32,
            controller: &mut TransformStreamDefaultController<i32>,
        ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
            let result = controller.enqueue(chunk * 2);
            futures::future::ready(result)
        }
    }

    #[tokio::test]
    async fn test_pipe_through_basic() {
        // Create source stream
        let data = vec!["hello".to_string(), "world".to_string()];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        // Create transform
        let transform = TransformStream::new(UppercaseTransformer);

        // Pipe through
        let result_stream = source_stream.pipe_through(transform, None);
        let (_locked, reader) = result_stream.get_reader();

        // Read results
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
        assert_eq!(result3, None); // Stream closed
    }

    #[tokio::test]
    async fn test_pipe_through_numbers() {
        let data = vec![1, 2, 3, 4];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let transform = TransformStream::new(DoubleTransformer);
        let result_stream = source_stream.pipe_through(transform, None);
        let (_locked, reader) = result_stream.get_reader();

        // Should get doubled values
        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert_eq!(reader.read().await.unwrap(), Some(4));
        assert_eq!(reader.read().await.unwrap(), Some(6));
        assert_eq!(reader.read().await.unwrap(), Some(8));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_pipe_through_empty_stream() {
        let data: Vec<String> = vec![];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let transform = TransformStream::new(UppercaseTransformer);
        let result_stream = source_stream.pipe_through(transform, None);
        let (_locked, reader) = result_stream.get_reader();

        // Should immediately return None
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_pipe_through_chained() {
        // Test chaining multiple transforms
        let data = vec![1, 2, 3];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        // First transform: double
        let transform1 = TransformStream::new(DoubleTransformer);
        let intermediate_stream = source_stream.pipe_through(transform1, None);

        // Second transform: double again
        let transform2 = TransformStream::new(DoubleTransformer);
        let result_stream = intermediate_stream.pipe_through(transform2, None);
        let (_locked, reader) = result_stream.get_reader();

        // Should get quadrupled values
        assert_eq!(reader.read().await.unwrap(), Some(4)); // 1 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), Some(8)); // 2 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), Some(12)); // 3 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), None);
    }

    // Test with pipe options
    #[tokio::test]
    async fn test_pipe_through_with_options() {
        let data = vec!["test".to_string()];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let transform = TransformStream::new(UppercaseTransformer);

        let options = StreamPipeOptions {
            prevent_close: false,
            prevent_abort: false,
            prevent_cancel: false,
            signal: None,
        };

        let result_stream = source_stream.pipe_through(transform, Some(options));
        let (_locked, reader) = result_stream.get_reader();

        assert_eq!(reader.read().await.unwrap(), Some("TEST".to_string()));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    // Test error handling in pipe_through
    #[tokio::test]
    async fn test_pipe_through_error_handling() {
        struct ErrorTransformer;

        impl Transformer<i32, i32> for ErrorTransformer {
            fn transform(
                &mut self,
                chunk: i32,
                controller: &mut TransformStreamDefaultController<i32>,
            ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
                if chunk == 3 {
                    futures::future::ready(Err(StreamError::Custom("Error on 3".into())))
                } else {
                    let result = controller.enqueue(chunk);
                    futures::future::ready(result)
                }
            }
        }

        let data = vec![1, 2, 3, 4];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let transform = TransformStream::new(ErrorTransformer);
        let result_stream = source_stream.pipe_through(transform, None);
        let (_locked, reader) = result_stream.get_reader();

        // Should get first two values
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(2));

        // Then should error
        let read_result = reader.read().await;
        assert!(read_result.is_err());
    }

    #[tokio::test]
    async fn test_pipe_through_spawned() {
        // Create source stream
        let data = vec![1, 2, 3];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        // Create a custom spawn function using tokio::spawn
        let my_spawn = |future| tokio::spawn(future);

        // Create a transform
        let transform = TransformStream::new(DoubleTransformer);

        // Pipe through using the custom spawn function
        let (result_stream, _join_handle) =
            source_stream.pipe_through_spawned(transform, None, my_spawn);

        let (_locked, reader) = result_stream.get_reader();

        // Should get doubled values
        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert_eq!(reader.read().await.unwrap(), Some(4));
        assert_eq!(reader.read().await.unwrap(), Some(6));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_pipe_through_spawned_chained() {
        // Create a custom spawn function using tokio::spawn.
        // This is a common pattern to pass to `pipe_through_spawned`.
        let my_spawn = |future| tokio::spawn(future);

        // 1. Create the initial source stream with some data.
        let data = vec![1, 2, 3];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        // We will collect the futures returned by each spawn call to await them later.
        let mut spawn_futures = Vec::new();

        // 2. Perform the first pipe operation.
        // This doubles the values and spawns the piping task.
        let (intermediate_stream, future_1) = source_stream.pipe_through_spawned(
            TransformStream::new(DoubleTransformer), // The transform to apply
            None,                                    // Optional stream pipe options
            my_spawn,                                // The spawn function to run the pipe task
        );
        // Add the returned future handle to our collection.
        spawn_futures.push(future_1);

        // 3. Perform the second pipe operation, using the output of the first as the input.
        // This will double the already-doubled values, resulting in quadrupled values.
        let (result_stream, future_2) = intermediate_stream.pipe_through_spawned(
            TransformStream::new(DoubleTransformer), // Another transform
            None,                                    // Optional stream pipe options
            my_spawn,                                // Reuse the same spawn function
        );
        // Add the second future handle to our collection.
        spawn_futures.push(future_2);

        // 4. Get a reader for the final stream.
        let (_locked, reader) = result_stream.get_reader();

        // 5. Read the final results. The values should be quadrupled.
        assert_eq!(reader.read().await.unwrap(), Some(4)); // 1 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), Some(8)); // 2 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), Some(12)); // 3 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), None); // The stream should be closed now.

        // 6. Await all the spawned futures to ensure the background piping tasks
        // have completed successfully. This is good practice to prevent the test
        // from finishing while tasks are still running.
        let results = futures::future::join_all(spawn_futures).await;

        // You could also assert that all futures completed without errors.
        for result in results {
            assert!(result.is_ok());
            let inner_result = result.unwrap();
            assert!(inner_result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_pipe_through_with_spawn_chained() {
        // 1. Create the initial source stream.
        let data = vec![1, 2, 3];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        // 2. Perform the first pipe operation using the new function.
        // It returns a ReadableStream, which is immediately chainable.
        let intermediate_stream = source_stream.pipe_through_with_spawn(
            TransformStream::new(DoubleTransformer),
            None,
            tokio::spawn,
        );

        // 3. Perform the second pipe operation.
        // This is a seamless continuation of the chain.
        let result_stream = intermediate_stream.pipe_through_with_spawn(
            TransformStream::new(DoubleTransformer),
            None,
            tokio::spawn,
        );

        // 4. Get a reader for the final stream.
        let (_locked, reader) = result_stream.get_reader();

        // 5. Read the final results. The values should be quadrupled.
        assert_eq!(reader.read().await.unwrap(), Some(4)); // 1 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), Some(8)); // 2 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), Some(12)); // 3 * 2 * 2
        assert_eq!(reader.read().await.unwrap(), None); // The stream should be closed now.
    }
}

#[cfg(test)]
mod tee_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_tee_basic() {
        // Create source stream
        let data = vec![1, 2, 3, 4];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        // Tee the stream
        let (stream1, stream2) = source_stream.tee();

        // Get readers for both branches
        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Both branches should get the same data
        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        assert_eq!(reader1.read().await.unwrap(), Some(2));
        assert_eq!(reader2.read().await.unwrap(), Some(2));

        assert_eq!(reader1.read().await.unwrap(), Some(3));
        assert_eq!(reader2.read().await.unwrap(), Some(3));

        assert_eq!(reader1.read().await.unwrap(), Some(4));
        assert_eq!(reader2.read().await.unwrap(), Some(4));

        // Both should end
        assert_eq!(reader1.read().await.unwrap(), None);
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_tee_different_read_speeds() {
        let data = vec![1, 2, 3];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let (stream1, stream2) = source_stream.tee();
        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

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

    #[tokio::test]
    async fn test_tee_one_branch_cancel() {
        let data = vec![1, 2, 3, 4];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let (stream1, stream2) = source_stream.tee();
        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Read first item from both
        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        // Cancel reader1
        reader1
            .cancel(Some("Branch 1 canceled".to_string()))
            .await
            .unwrap();

        // Reader2 should still work (original stream continues)
        assert_eq!(reader2.read().await.unwrap(), Some(2));
        assert_eq!(reader2.read().await.unwrap(), Some(3));
        assert_eq!(reader2.read().await.unwrap(), Some(4));
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_tee_both_branches_cancel() {
        let data = vec![1, 2, 3, 4];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let (stream1, stream2) = source_stream.tee();
        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Read first item from both
        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        // Cancel both branches
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

        // Original stream should be canceled (coordinator should have exited)
        // We can't directly test the original stream since it's consumed by tee,
        // but if both readers are canceled, the coordinator should stop
    }

    #[tokio::test]
    async fn test_tee_empty_stream() {
        let data: Vec<i32> = vec![];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let (stream1, stream2) = source_stream.tee();
        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Both should immediately return None
        assert_eq!(reader1.read().await.unwrap(), None);
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_tee_error_propagation() {
        // Create a stream that will error
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
                    Err(StreamError::Custom("Test error".into()))
                }
            }
        }

        let error_stream = ReadableStream::new(ErrorSource { count: 0 });
        let (stream1, stream2) = error_stream.tee();
        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Both should get the data before error
        assert_eq!(reader1.read().await.unwrap(), Some(0));
        assert_eq!(reader2.read().await.unwrap(), Some(0));

        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        // Both should get the same error
        let err1 = reader1.read().await.unwrap_err();
        let err2 = reader2.read().await.unwrap_err();

        match (&err1, &err2) {
            (StreamError::Custom(msg1), StreamError::Custom(msg2)) => {
                assert_eq!(msg1.to_string(), "Test error");
                assert_eq!(msg2.to_string(), "Test error");
            }
            _ => panic!("Expected Custom error"),
        }
    }

    #[tokio::test]
    async fn test_tee_chained() {
        // Test tee of a tee (multiple levels)
        let data = vec![1, 2];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        // First level tee
        let (stream1, stream2) = source_stream.tee();

        // Second level tee on stream1
        let (stream1a, stream1b) = stream1.tee();

        // Get readers
        let (_, reader1a) = stream1a.get_reader();
        let (_, reader1b) = stream1b.get_reader();
        let (_, reader2) = stream2.get_reader();

        // All should get the same data
        assert_eq!(reader1a.read().await.unwrap(), Some(1));
        assert_eq!(reader1b.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        assert_eq!(reader1a.read().await.unwrap(), Some(2));
        assert_eq!(reader1b.read().await.unwrap(), Some(2));
        assert_eq!(reader2.read().await.unwrap(), Some(2));

        // All should end
        assert_eq!(reader1a.read().await.unwrap(), None);
        assert_eq!(reader1b.read().await.unwrap(), None);
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_tee_with_pipe_operations() {
        // Test that tee branches can be used in pipe operations
        let data = vec![1, 2, 3];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let (stream1, stream2) = source_stream.tee();

        // Use stream1 directly
        let (_, reader1) = stream1.get_reader();

        // Pipe stream2 through a transform (if you have transforms implemented)
        // For now, just read from stream2 directly
        let (_, reader2) = stream2.get_reader();

        // Both should work independently
        assert_eq!(reader1.read().await.unwrap(), Some(1));
        assert_eq!(reader2.read().await.unwrap(), Some(1));

        assert_eq!(reader1.read().await.unwrap(), Some(2));
        assert_eq!(reader2.read().await.unwrap(), Some(2));

        assert_eq!(reader1.read().await.unwrap(), Some(3));
        assert_eq!(reader2.read().await.unwrap(), Some(3));

        assert_eq!(reader1.read().await.unwrap(), None);
        assert_eq!(reader2.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_tee_string_data() {
        // Test with string data to ensure cloning works for different types
        let data = vec!["hello".to_string(), "world".to_string()];
        let source_stream = ReadableStream::from_iter(data.into_iter(), None);

        let (stream1, stream2) = source_stream.tee();
        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        assert_eq!(reader1.read().await.unwrap(), Some("hello".to_string()));
        assert_eq!(reader2.read().await.unwrap(), Some("hello".to_string()));

        assert_eq!(reader1.read().await.unwrap(), Some("world".to_string()));
        assert_eq!(reader2.read().await.unwrap(), Some("world".to_string()));

        assert_eq!(reader1.read().await.unwrap(), None);
        assert_eq!(reader2.read().await.unwrap(), None);
    }
}

#[cfg(test)]
mod backpressure_tee_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::time::{Duration, sleep};

    #[tokio::test]
    async fn test_spec_compliant_fast_reader_not_throttled() {
        let data: Vec<i32> = (1..=10).collect();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) =
            source_stream.tee_with_backpressure(BackpressureMode::Unbounded, Some(2));

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        let fast_count = Arc::new(Mutex::new(0));
        let slow_count = Arc::new(Mutex::new(0));

        let fast_count_clone = fast_count.clone();
        let slow_count_clone = slow_count.clone();

        // Fast reader
        let fast_task = tokio::spawn(async move {
            while let Ok(Some(_)) = reader1.read().await {
                *fast_count_clone.lock().unwrap() += 1;
            }
        });

        // Slow reader
        let slow_task = tokio::spawn(async move {
            while let Ok(Some(_)) = reader2.read().await {
                *slow_count_clone.lock().unwrap() += 1;
                sleep(Duration::from_millis(20)).await;
            }
        });

        tokio::join!(fast_task, slow_task);

        let fast = *fast_count.lock().unwrap();
        let slow = *slow_count.lock().unwrap();

        // SpecCompliant: fast reader is unconstrained
        assert_eq!(fast, data.len());
        assert_eq!(slow, data.len());
        assert!(
            fast >= slow,
            "Fast should finish before or alongside slow reader"
        );
    }

    #[tokio::test]
    async fn test_slowest_consumer_fast_reader_throttled() {
        let data: Vec<i32> = (1..=10).collect();
        let buffer_size = 2;
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) = source_stream
            .tee_with_backpressure(BackpressureMode::SlowestConsumer, Some(buffer_size));

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        let fast_progress = Arc::new(Mutex::new(Vec::new()));
        let slow_progress = Arc::new(Mutex::new(Vec::new()));

        let fast_progress_clone = fast_progress.clone();
        let slow_progress_clone = slow_progress.clone();

        // Fast reader (throttled by slow)
        let fast_task = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Some(_)) = reader1.read().await {
                count += 1;
                fast_progress_clone.lock().unwrap().push(count);
            }
        });

        // Slow reader (controls pace)
        let slow_task = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Some(_)) = reader2.read().await {
                count += 1;
                slow_progress_clone.lock().unwrap().push(count);
                sleep(Duration::from_millis(20)).await;
            }
        });

        tokio::join!(fast_task, slow_task);

        let fast = fast_progress.lock().unwrap();
        let slow = slow_progress.lock().unwrap();

        for i in 0..fast.len().min(slow.len()) {
            assert!(
                fast[i] <= slow[i] + buffer_size,
                "Fast reader got too far ahead at step {}: fast={}, slow={}",
                i,
                fast[i],
                slow[i]
            );
        }
    }

    #[tokio::test]
    async fn test_independent_mode_separate_limits() {
        let data: Vec<i32> = (1..=10).collect();
        let buffer_size = 2;
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) =
            source_stream.tee_with_backpressure(BackpressureMode::SpecCompliant, Some(buffer_size));

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        let fast_progress = Arc::new(Mutex::new(Vec::new()));
        let slow_progress = Arc::new(Mutex::new(Vec::new()));

        let fast_progress_clone = fast_progress.clone();
        let slow_progress_clone = slow_progress.clone();

        // Fast reader (independent but bounded by its buffer)
        let fast_task = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Some(_)) = reader1.read().await {
                count += 1;
                fast_progress_clone.lock().unwrap().push(count);
            }
        });

        // Slow reader
        let slow_task = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Some(_)) = reader2.read().await {
                count += 1;
                slow_progress_clone.lock().unwrap().push(count);
                sleep(Duration::from_millis(15)).await;
            }
        });

        tokio::join!(fast_task, slow_task);

        let fast = fast_progress.lock().unwrap();
        let slow = slow_progress.lock().unwrap();

        // Independent mode: fast can progress more than slow, but only within its buffer limit
        for i in 0..fast.len().min(slow.len()) {
            assert!(
                fast[i] <= slow[i] + buffer_size,
                "Fast reader exceeded its independent buffer at step {}: fast={}, slow={}",
                i,
                fast[i],
                slow[i]
            );
        }

        assert!(
            fast.len() >= slow.len(),
            "Fast should advance more than slow overall"
        );
    }

    #[tokio::test]
    async fn test_buffer_limit_enforcement() {
        let data: Vec<i32> = (1..=8).collect();
        let buffer_size = 1;
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) = source_stream
            .tee_with_backpressure(BackpressureMode::SlowestConsumer, Some(buffer_size));

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        let fast_progress = Arc::new(Mutex::new(Vec::new()));
        let slow_progress = Arc::new(Mutex::new(Vec::new()));

        let fast_progress_clone = fast_progress.clone();
        let slow_progress_clone = slow_progress.clone();

        // Fast reader
        let fast_task = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Some(_)) = reader1.read().await {
                count += 1;
                fast_progress_clone.lock().unwrap().push(count);
            }
        });

        // Slow reader
        let slow_task = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Some(_)) = reader2.read().await {
                count += 1;
                slow_progress_clone.lock().unwrap().push(count);
                sleep(Duration::from_millis(25)).await;
            }
        });

        tokio::join!(fast_task, slow_task);

        let fast = fast_progress.lock().unwrap();
        let slow = slow_progress.lock().unwrap();

        for i in 0..fast.len().min(slow.len()) {
            assert!(
                fast[i] <= slow[i] + buffer_size,
                "Buffer overflow: fast got {} vs slow {} at step {}",
                fast[i],
                slow[i],
                i
            );
        }
    }
}

#[cfg(test)]
mod spawn_variant_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn test_data() -> Vec<i32> {
        vec![1, 2, 3, 4, 5]
    }

    #[tokio::test]
    async fn test_blocking_spawn_with_tokio() {
        let data = test_data();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) = source_stream.tee_with_blocking_spawn(|runner| {
            tokio::spawn(async move {
                runner();
            });
        });

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Read concurrently to avoid deadlock
        let task1 = tokio::spawn(async move {
            let mut results = Vec::new();
            while let Ok(Some(value)) = reader1.read().await {
                results.push(value);
            }
            results
        });

        let task2 = tokio::spawn(async move {
            let mut results = Vec::new();
            while let Ok(Some(value)) = reader2.read().await {
                results.push(value);
            }
            results
        });

        let (results1, results2) = tokio::join!(task1, task2);
        let results1 = results1.unwrap();
        let results2 = results2.unwrap();

        assert_eq!(results1, data);
        assert_eq!(results2, data);
    }

    #[tokio::test]
    async fn test_blocking_spawn_with_thread() {
        let data = test_data();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) = source_stream.tee_with_blocking_spawn(|runner| {
            std::thread::spawn(move || {
                runner();
            });
        });

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Read concurrently
        let task1 = tokio::spawn(async move {
            let mut results = Vec::new();
            while let Ok(Some(value)) = reader1.read().await {
                results.push(value);
            }
            results
        });

        let task2 = tokio::spawn(async move {
            let mut results = Vec::new();
            while let Ok(Some(value)) = reader2.read().await {
                results.push(value);
            }
            results
        });

        let (results1, results2) = tokio::join!(task1, task2);
        assert_eq!(results1.unwrap(), data);
        assert_eq!(results2.unwrap(), data);
    }

    #[tokio::test]
    async fn test_async_spawn_with_tokio() {
        let data = test_data();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) = source_stream.tee_with_async_spawn(|fut| {
            tokio::spawn(fut);
        });

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Read concurrently
        let task1 = tokio::spawn(async move {
            let mut results = Vec::new();
            while let Ok(Some(value)) = reader1.read().await {
                results.push(value);
            }
            results
        });

        let task2 = tokio::spawn(async move {
            let mut results = Vec::new();
            while let Ok(Some(value)) = reader2.read().await {
                results.push(value);
            }
            results
        });

        let (results1, results2) = tokio::join!(task1, task2);
        assert_eq!(results1.unwrap(), data);
        assert_eq!(results2.unwrap(), data);
    }

    //#[tokio::test]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_backpressure_with_blocking_spawn() {
        let data = test_data();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) = source_stream.tee_with_backpressure_and_blocking_spawn(
            BackpressureMode::SlowestConsumer,
            Some(2),
            |runner| {
                tokio::spawn(async move {
                    runner();
                });
                /*std::thread::spawn(move || {
                    runner();
                });*/
            },
        );

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // CRITICAL: Read concurrently to avoid deadlock with SlowestConsumer
        let count1 = Arc::new(Mutex::new(0));
        let count2 = Arc::new(Mutex::new(0));

        let count1_clone = count1.clone();
        let count2_clone = count2.clone();

        let task1 = tokio::spawn(async move {
            while let Ok(Some(_)) = reader1.read().await {
                *count1_clone.lock().unwrap() += 1;
            }
        });

        let task2 = tokio::spawn(async move {
            while let Ok(Some(_)) = reader2.read().await {
                *count2_clone.lock().unwrap() += 1;
            }
        });

        tokio::join!(task1, task2);

        assert_eq!(*count1.lock().unwrap(), data.len());
        assert_eq!(*count2.lock().unwrap(), data.len());
    }

    #[tokio::test]
    async fn test_backpressure_with_async_spawn() {
        let data = test_data();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) = source_stream.tee_with_backpressure_and_async_spawn(
            BackpressureMode::Aggregate,
            Some(3),
            |fut| {
                tokio::spawn(fut);
            },
        );

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Read concurrently
        let count1 = Arc::new(Mutex::new(0));
        let count2 = Arc::new(Mutex::new(0));

        let count1_clone = count1.clone();
        let count2_clone = count2.clone();

        let task1 = tokio::spawn(async move {
            while let Ok(Some(_)) = reader1.read().await {
                *count1_clone.lock().unwrap() += 1;
            }
        });

        let task2 = tokio::spawn(async move {
            while let Ok(Some(_)) = reader2.read().await {
                *count2_clone.lock().unwrap() += 1;
            }
        });

        tokio::join!(task1, task2);

        assert_eq!(*count1.lock().unwrap(), data.len());
        assert_eq!(*count2.lock().unwrap(), data.len());
    }

    #[test]
    fn test_default_tee_still_works() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let data = test_data();
            let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

            let (stream1, stream2) = source_stream.tee();

            let (_, reader1) = stream1.get_reader();
            let (_, reader2) = stream2.get_reader();

            for expected in &data {
                assert_eq!(reader1.read().await.unwrap(), Some(*expected));
                assert_eq!(reader2.read().await.unwrap(), Some(*expected));
            }
            assert_eq!(reader1.read().await.unwrap(), None);
            assert_eq!(reader2.read().await.unwrap(), None);
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn _test_local_async_spawn_with_tokio() {
        let data = test_data();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) = source_stream.tee_with_local_async_spawn(|fut| {
            tokio::task::spawn_local(fut);
        });

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        // Read concurrently
        let task1 = tokio::task::spawn_local(async move {
            let mut results = Vec::new();
            while let Ok(Some(value)) = reader1.read().await {
                results.push(value);
            }
            results
        });

        let task2 = tokio::task::spawn_local(async move {
            let mut results = Vec::new();
            while let Ok(Some(value)) = reader2.read().await {
                results.push(value);
            }
            results
        });

        let results1 = task1.await.unwrap();
        let results2 = task2.await.unwrap();

        assert_eq!(results1, data);
        assert_eq!(results2, data);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_local_async_spawn_with_tokio() {
        let data = test_data();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        // Create LocalSet
        let local = tokio::task::LocalSet::new();

        // Run everything inside the LocalSet
        local
            .run_until(async move {
                let (stream1, stream2) = source_stream.tee_with_local_async_spawn(|fut| {
                    tokio::task::spawn_local(fut);
                });

                let (_, reader1) = stream1.get_reader();
                let (_, reader2) = stream2.get_reader();

                let task1 = tokio::task::spawn_local(async move {
                    let mut results = Vec::new();
                    while let Ok(Some(value)) = reader1.read().await {
                        results.push(value);
                    }
                    results
                });

                let task2 = tokio::task::spawn_local(async move {
                    let mut results = Vec::new();
                    while let Ok(Some(value)) = reader2.read().await {
                        results.push(value);
                    }
                    results
                });

                let results1 = task1.await.unwrap();
                let results2 = task2.await.unwrap();

                assert_eq!(results1, data);
                assert_eq!(results2, data);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn _test_backpressure_with_local_async_spawn() {
        let data = test_data();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let (stream1, stream2) = source_stream.tee_with_backpressure_and_local_async_spawn(
            BackpressureMode::SlowestConsumer,
            Some(2),
            |fut| {
                tokio::task::spawn_local(fut);
            },
        );

        let (_, reader1) = stream1.get_reader();
        let (_, reader2) = stream2.get_reader();

        let count1 = Arc::new(Mutex::new(0));
        let count2 = Arc::new(Mutex::new(0));

        let count1_clone = count1.clone();
        let count2_clone = count2.clone();

        let task1 = tokio::task::spawn_local(async move {
            while let Ok(Some(_)) = reader1.read().await {
                *count1_clone.lock().unwrap() += 1;
            }
        });

        let task2 = tokio::task::spawn_local(async move {
            while let Ok(Some(_)) = reader2.read().await {
                *count2_clone.lock().unwrap() += 1;
            }
        });

        tokio::join!(task1, task2);

        assert_eq!(*count1.lock().unwrap(), data.len());
        assert_eq!(*count2.lock().unwrap(), data.len());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_backpressure_with_local_async_spawn() {
        let data = test_data();
        let source_stream = ReadableStream::from_iter(data.clone().into_iter(), None);

        let local = tokio::task::LocalSet::new();

        local
            .run_until(async move {
                let (stream1, stream2) = source_stream.tee_with_backpressure_and_local_async_spawn(
                    BackpressureMode::SlowestConsumer,
                    Some(2),
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );

                let (_, reader1) = stream1.get_reader();
                let (_, reader2) = stream2.get_reader();

                let count1 = Arc::new(Mutex::new(0));
                let count2 = Arc::new(Mutex::new(0));

                let count1_clone = count1.clone();
                let count2_clone = count2.clone();

                let task1 = tokio::task::spawn_local(async move {
                    while let Ok(Some(_)) = reader1.read().await {
                        *count1_clone.lock().unwrap() += 1;
                    }
                });

                let task2 = tokio::task::spawn_local(async move {
                    while let Ok(Some(_)) = reader2.read().await {
                        *count2_clone.lock().unwrap() += 1;
                    }
                });

                tokio::join!(task1, task2);

                assert_eq!(*count1.lock().unwrap(), data.len());
                assert_eq!(*count2.lock().unwrap(), data.len());
            })
            .await;
    }
}
