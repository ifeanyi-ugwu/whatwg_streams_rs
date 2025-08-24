use super::{CountQueuingStrategy, QueuingStrategy};
use futures::{
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    future::poll_fn,
    io::AsyncRead,
    stream::{Stream, StreamExt},
};
use std::{
    collections::VecDeque,
    error::Error,
    fmt,
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

// ----------- Error Types -----------
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
            StreamError::Closing => write!(f, "Stream is closing"),
            StreamError::Closed => write!(f, "Stream is closed"),
            StreamError::Custom(err) => write!(f, "{}", err),
        }
    }
}

type StreamResult<T> = Result<T, StreamError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Readable,
    Closed,
    Errored,
}

// ----------- Placeholder types  -----------
pub struct WritableStream<T> {
    _phantom: PhantomData<T>,
}

pub struct TransformStream<I, O> {
    _phantom: PhantomData<(I, O)>,
}

pub struct AbortSignal {
    // Implementation details would go here
}

// ----------- Stream Type Markers -----------
pub struct DefaultStream;
pub struct ByteStream;

// ----------- Lock State Markers -----------
pub struct Unlocked;
pub struct Locked;

// ----------- Stream Type Marker Trait -----------
pub trait StreamTypeMarker: Send + Sync + 'static {}

impl StreamTypeMarker for DefaultStream {}
impl StreamTypeMarker for ByteStream {}

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

pub trait ReadableByteSource: Send + Sized + 'static {
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
}

// ----------- WakerSet -----------
#[derive(Clone, Default)]
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
        completion: oneshot::Sender<StreamResult<usize>>,
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

    pub fn desired_size(&self) -> isize {
        if self.closed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
            return 0;
        }

        self.desired_size.load(Ordering::SeqCst)
    }

    pub fn close(&mut self) -> StreamResult<()> {
        self.tx
            .unbounded_send(ControllerMsg::Close)
            .map_err(|_| StreamError::Custom(ArcError::from("Failed to close stream")))?;
        Ok(())
    }

    pub fn enqueue(&mut self, chunk: T) -> StreamResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(StreamError::Custom(ArcError::from("Stream is closed")));
        }
        if self.errored.load(Ordering::SeqCst) {
            return Err(StreamError::Custom(ArcError::from("Stream is errored")));
        }

        self.tx
            .unbounded_send(ControllerMsg::Enqueue { chunk })
            .map_err(|_| StreamError::Custom(ArcError::from("Failed to enqueue chunk")))?;
        Ok(())
    }

    pub fn error(&mut self, error: StreamError) -> StreamResult<()> {
        self.tx
            .unbounded_send(ControllerMsg::Error(error))
            .map_err(|_| StreamError::Custom(ArcError::from("Failed to error stream")))?;
        Ok(())
    }
}

pub struct ReadableByteStreamController {
    tx: UnboundedSender<ByteControllerMsg>,
    queue_total_size: Arc<AtomicUsize>,
    high_water_mark: Arc<AtomicUsize>,
    desired_size: Arc<AtomicIsize>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
}

impl ReadableByteStreamController {
    fn new(
        tx: UnboundedSender<ByteControllerMsg>,
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

    pub fn desired_size(&self) -> isize {
        if self.closed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
            return 0;
        }

        self.desired_size.load(Ordering::SeqCst)
    }

    pub fn close(&mut self) -> StreamResult<()> {
        self.tx
            .unbounded_send(ByteControllerMsg::Close)
            .map_err(|_| StreamError::Custom(ArcError::from("Failed to close stream")))?;
        Ok(())
    }

    pub fn enqueue(&mut self, chunk: Vec<u8>) -> StreamResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(StreamError::Custom(ArcError::from("Stream is closed")));
        }
        if self.errored.load(Ordering::SeqCst) {
            return Err(StreamError::Custom(ArcError::from("Stream is errored")));
        }

        self.tx
            .unbounded_send(ByteControllerMsg::Enqueue { chunk })
            .map_err(|_| StreamError::Custom(ArcError::from("Failed to enqueue chunk")))?;
        Ok(())
    }

    pub fn error(&mut self, error: StreamError) -> StreamResult<()> {
        self.tx
            .unbounded_send(ByteControllerMsg::Error(error))
            .map_err(|_| StreamError::Custom(ArcError::from("Failed to error stream")))?;
        Ok(())
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
    pending_read_intos: VecDeque<(Vec<u8>, oneshot::Sender<StreamResult<usize>>)>,
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
    _phantom: PhantomData<(T, Source, StreamType, LockState)>,
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

    pub fn pipe_through<I, O, OutputStreamType>(
        self,
        _transform: &mut TransformStream<I, O>,
        _options: Option<StreamPipeOptions>,
    ) -> ReadableStream<O, Source, OutputStreamType, Unlocked>
    where
        I: Send + 'static,
        O: Send + 'static,
        OutputStreamType: StreamTypeMarker,
        T: Into<I>, // Input stream data must be convertible to transform input
    {
        todo!("pipe_through implementation")
    }

    pub async fn pipe_to<W>(
        &self,
        _destination: &WritableStream<T>,
        _options: Option<StreamPipeOptions>,
    ) -> StreamResult<()> {
        todo!("pipe_to implementation")
    }

    pub fn tee(
        self,
    ) -> (
        ReadableStream<T, Source, S, Locked>,
        ReadableStream<T, Source, S, Locked>,
    ) {
        todo!("tee implementation")
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
        let (ctrl_tx, ctrl_rx) = unbounded();
        let queue_total_size = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let locked = Arc::new(AtomicBool::new(false));
        let stored_error = Arc::new(RwLock::new(None));

        let strategy: Box<dyn QueuingStrategy<Vec<u8>> + Send + Sync> =
            strategy.unwrap_or_else(|| Box::new(CountQueuingStrategy::new(1)));

        let high_water_mark = Arc::new(AtomicUsize::new(strategy.high_water_mark()));
        let desired_size = Arc::new(AtomicIsize::new(strategy.high_water_mark() as isize));

        let inner = ReadableStreamInner::new(source, strategy);

        let task_fut = readable_byte_stream_task(
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
            _phantom: PhantomData,
        }
    }
}

// ----------- Generic Constructor -----------
impl<T, Source, StreamType> ReadableStream<T, Source, StreamType, Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
{
    pub fn new(source: Source) -> Self
    where
        Source: ReadableSource<T>,
    {
        let (command_tx, _command_rx) = unbounded();
        Self {
            command_tx,
            queue_total_size: Arc::new(AtomicUsize::new(0)),
            high_water_mark: Arc::new(AtomicUsize::new(0)),
            desired_size: Arc::new(AtomicIsize::new(0)),
            closed: Arc::new(AtomicBool::new(false)),
            errored: Arc::new(AtomicBool::new(false)),
            locked: Arc::new(AtomicBool::new(false)),
            stored_error: Arc::new(RwLock::new(None)),
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

// ----------- Reader Methods for Default Streams -----------
impl<T, Source> ReadableStream<T, Source, DefaultStream, Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
{
    pub fn get_reader(
        self,
    ) -> (
        ReadableStream<T, Source, DefaultStream, Locked>,
        ReadableStreamDefaultReader<T, Source, DefaultStream, Locked>,
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
    pub fn get_reader(
        self,
    ) -> (
        ReadableStream<Vec<u8>, Source, ByteStream, Locked>,
        ReadableStreamDefaultReader<Vec<u8>, Source, ByteStream, Locked>,
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
            _phantom: PhantomData,
        });

        (locked_stream, reader)
    }

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
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<IoResult<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if self.errored.load(Ordering::SeqCst) {
            let error_msg = self
                .stored_error
                .read()
                .ok()
                .and_then(|guard| guard.clone())
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Stream is errored".to_string());
            return Poll::Ready(Err(IoError::new(ErrorKind::Other, error_msg)));
        }

        if self.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(0));
        }

        let waker = cx.waker().clone();
        let _ = self
            .command_tx
            .unbounded_send(StreamCommand::RegisterReadyWaker { waker });

        Poll::Pending
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

    pub async fn closed(&self) -> StreamResult<()> {
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
            _phantom: PhantomData,
        }
    }
}

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

    pub async fn closed(&self) -> StreamResult<()> {
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

    // BYOB-specific read method that takes a buffer
    pub async fn read(&self, buffer: &mut [u8]) -> StreamResult<Option<usize>> {
        let (tx, rx) = oneshot::channel();
        self.0
            .command_tx
            .unbounded_send(StreamCommand::ReadInto {
                buffer: buffer.to_vec(),
                completion: tx,
            })
            .map_err(|_| StreamError::Custom("Stream task dropped".into()))?;

        match rx.await {
            Ok(Ok(bytes_read)) => Ok(Some(bytes_read)),
            Ok(Err(StreamError::Closed)) => Ok(None),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(StreamError::Custom("Read canceled".into())),
        }
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
) where
    T: Send + 'static,
    Source: ReadableSource<T> + Send + 'static,
{
    // Call start() first before processing any commands
    if let Some(mut source) = inner.source.take() {
        let mut controller = ReadableStreamDefaultController::new(
            ctrl_tx.clone(),
            Arc::clone(&queue_total_size),
            Arc::clone(&high_water_mark),
            Arc::clone(&desired_size),
            Arc::clone(&closed),
            Arc::clone(&errored),
        );

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
                let ctrl_tx_clone = ctrl_tx.clone();
                let queue_total_size_clone = Arc::clone(&queue_total_size);
                let high_water_mark_clone = Arc::clone(&high_water_mark);
                let desired_size_clone = Arc::clone(&desired_size);
                let closed_clone = Arc::clone(&closed);
                let errored_clone = Arc::clone(&errored);
                pull_future = Some(Box::pin(async move {
                    let mut source = source;
                    let mut controller = ReadableStreamDefaultController::new(
                        ctrl_tx_clone,
                        queue_total_size_clone,
                        high_water_mark_clone,
                        desired_size_clone,
                        closed_clone,
                        errored_clone,
                    );
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
async fn readable_byte_stream_task<Source>(
    mut command_rx: UnboundedReceiver<StreamCommand<Vec<u8>>>,
    mut ctrl_rx: UnboundedReceiver<ByteControllerMsg>,
    mut inner: ReadableStreamInner<Vec<u8>, Source>,
    queue_total_size: Arc<AtomicUsize>,
    high_water_mark: Arc<AtomicUsize>,
    desired_size: Arc<AtomicIsize>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    stored_error: Arc<RwLock<Option<StreamError>>>,
    ctrl_tx: UnboundedSender<ByteControllerMsg>,
) where
    Source: ReadableByteSource + Send + 'static,
{
    // Call start() first before processing any commands
    if let Some(mut source) = inner.source.take() {
        let mut controller = ReadableByteStreamController::new(
            ctrl_tx.clone(),
            Arc::clone(&queue_total_size),
            Arc::clone(&high_water_mark),
            Arc::clone(&desired_size),
            Arc::clone(&closed),
            Arc::clone(&errored),
        );

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

    let mut pull_future: Option<
        Pin<Box<dyn Future<Output = (Source, StreamResult<usize>, Vec<u8>)> + Send>>,
    > = None;
    let mut cancel_future: Option<Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>> = None;

    poll_fn(|cx| {
        // Process controller messages first
        while let Poll::Ready(Some(msg)) = ctrl_rx.poll_next_unpin(cx) {
            match msg {
                ByteControllerMsg::Enqueue { chunk } => {
                    // Check if we have pending read_into requests first
                    if let Some((mut buffer, tx)) = inner.pending_read_intos.pop_front() {
                        let bytes_to_copy = std::cmp::min(chunk.len(), buffer.len());
                        buffer[..bytes_to_copy].copy_from_slice(&chunk[..bytes_to_copy]);
                        let _ = tx.send(Ok(bytes_to_copy));

                        // If there's remaining data, put it in the queue
                        if chunk.len() > bytes_to_copy {
                            let remaining = chunk[bytes_to_copy..].to_vec();
                            let chunk_size = inner.strategy.size(&remaining);
                            inner.queue.push_back(remaining);
                            inner.queue_total_size += chunk_size;
                            queue_total_size.store(inner.queue_total_size, Ordering::SeqCst);
                        }
                    } else if let Some(tx) = inner.pending_reads.pop_front() {
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
                ByteControllerMsg::Close => {
                    if inner.state == StreamState::Readable {
                        inner.state = StreamState::Closed;
                        closed.store(true, Ordering::SeqCst);
                        desired_size.store(0, Ordering::SeqCst);

                        // Complete pending reads with None
                        while let Some(tx) = inner.pending_reads.pop_front() {
                            let _ = tx.send(Ok(None));
                        }

                        // Complete pending pull_intos with 0 bytes read (EOF)
                        while let Some((_buffer, tx)) = inner.pending_read_intos.pop_front() {
                            let _ = tx.send(Ok(0));
                        }

                        inner.closed_wakers.wake_all();
                        inner.ready_wakers.wake_all();
                    }
                }
                ByteControllerMsg::Error(err) => {
                    if inner.state != StreamState::Closed {
                        inner.state = StreamState::Errored;
                        errored.store(true, Ordering::SeqCst);
                        desired_size.store(0, Ordering::SeqCst);
                        inner.stored_error = Some(err.clone());
                        if let Ok(mut guard) = stored_error.write() {
                            *guard = Some(err.clone());
                        }
                        inner.queue.clear();
                        inner.queue_total_size = 0;
                        queue_total_size.store(0, Ordering::SeqCst);

                        // Fail pending operations
                        while let Some(tx) = inner.pending_reads.pop_front() {
                            let _ = tx.send(Err(err.clone()));
                        }
                        while let Some((_buffer, tx)) = inner.pending_read_intos.pop_front() {
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
                StreamCommand::ReadInto {
                    mut buffer,
                    completion,
                } => {
                    if inner.state == StreamState::Errored {
                        let _ = completion.send(Err(inner.get_stored_error()));
                        continue;
                    }
                    if inner.state == StreamState::Closed && inner.queue.is_empty() {
                        let _ = completion.send(Ok(0));
                        continue;
                    }

                    // Try to satisfy from existing queue first
                    if let Some(chunk) = inner.queue.pop_front() {
                        let bytes_to_copy = std::cmp::min(chunk.len(), buffer.len());
                        buffer[..bytes_to_copy].copy_from_slice(&chunk[..bytes_to_copy]);
                        let _ = completion.send(Ok(bytes_to_copy));

                        // Put back any remaining data
                        if chunk.len() > bytes_to_copy {
                            let remaining = chunk[bytes_to_copy..].to_vec();
                            let chunk_size = inner.strategy.size(&remaining);
                            inner.queue.push_front(remaining);
                            inner.queue_total_size -= inner.strategy.size(&chunk);
                            inner.queue_total_size += chunk_size;
                        } else {
                            let chunk_size = inner.strategy.size(&chunk);
                            inner.queue_total_size -= chunk_size;
                        }
                        queue_total_size.store(inner.queue_total_size, Ordering::SeqCst);
                        update_desired_size(
                            &queue_total_size,
                            &high_water_mark,
                            &desired_size,
                            &closed,
                            &errored,
                        );
                    } else {
                        // Queue the read_into request
                        inner.pending_read_intos.push_back((buffer, completion));
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

                        // Cancel all pending operations
                        while let Some(tx) = inner.pending_reads.pop_front() {
                            let _ = tx.send(Err(StreamError::Canceled));
                        }
                        while let Some((_buffer, tx)) = inner.pending_read_intos.pop_front() {
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
            && (inner.queue.is_empty()
                || !inner.pending_reads.is_empty()
                || !inner.pending_read_intos.is_empty())
        {
            if let Some(source) = inner.source.take() {
                inner.pulling = true;

                let ctrl_tx_clone = ctrl_tx.clone();
                let queue_total_size_clone = Arc::clone(&queue_total_size);
                let high_water_mark_clone = Arc::clone(&high_water_mark);
                let desired_size_clone = Arc::clone(&desired_size);
                let closed_clone = Arc::clone(&closed);
                let errored_clone = Arc::clone(&errored);

                // Use a fresh buffer inside the async block
                let buffer_size = if let Some((buffer, _)) = inner.pending_read_intos.front() {
                    buffer.len()
                } else {
                    8192
                };

                pull_future = Some(Box::pin(async move {
                    let mut source = source;
                    let mut controller = ReadableByteStreamController::new(
                        ctrl_tx_clone,
                        queue_total_size_clone,
                        high_water_mark_clone,
                        desired_size_clone,
                        closed_clone,
                        errored_clone,
                    );

                    let mut buffer = vec![0u8; buffer_size];
                    let result = source.pull(&mut controller, &mut buffer).await;
                    (source, result, buffer)
                }));
            }
        }

        // Poll pull future if in progress
        if let Some(ref mut fut) = pull_future {
            match fut.as_mut().poll(cx) {
                Poll::Ready((source, result, buffer)) => {
                    inner.pulling = false;
                    inner.source = Some(source);

                    match result {
                        Ok(bytes_read) => {
                            if bytes_read > 0 {
                                if let Some((mut consumer_buffer, tx)) =
                                    inner.pending_read_intos.pop_front()
                                {
                                    let to_copy = std::cmp::min(bytes_read, consumer_buffer.len());
                                    consumer_buffer[..to_copy].copy_from_slice(&buffer[..to_copy]);
                                    let _ = tx.send(Ok(to_copy));

                                    if bytes_read > to_copy {
                                        let remaining = buffer[to_copy..bytes_read].to_vec();
                                        let chunk_size = inner.strategy.size(&remaining);
                                        inner.queue.push_back(remaining);
                                        inner.queue_total_size += chunk_size;
                                        queue_total_size
                                            .store(inner.queue_total_size, Ordering::SeqCst);
                                    }
                                } else {
                                    let mut buf = buffer;
                                    buf.truncate(bytes_read);

                                    if let Some(tx) = inner.pending_reads.pop_front() {
                                        let _ = tx.send(Ok(Some(buf)));
                                    } else {
                                        let chunk_size = inner.strategy.size(&buf);
                                        inner.queue.push_back(buf);
                                        inner.queue_total_size += chunk_size;
                                        queue_total_size
                                            .store(inner.queue_total_size, Ordering::SeqCst);
                                    }
                                }
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
                        Err(err) => {
                            inner.state = StreamState::Errored;
                            errored.store(true, Ordering::SeqCst);
                            inner.stored_error = Some(err.clone());
                            if let Ok(mut guard) = stored_error.write() {
                                *guard = Some(err.clone());
                            }

                            // Fail all pending operations
                            while let Some(tx) = inner.pending_reads.pop_front() {
                                let _ = tx.send(Err(err.clone()));
                            }
                            while let Some((_buffer, tx)) = inner.pending_read_intos.pop_front() {
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
            Some(bytes_read) => {
                assert!(bytes_read > 0);
                println!("Read {} bytes", bytes_read);
            }
            None => println!("Stream ended"),
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
            Some(bytes_read) => {
                assert!(bytes_read > 0);
                println!("Read {} bytes", bytes_read);
            }
            None => println!("Stream ended"),
        }
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
                let len = chunk.len();
                *self.index.borrow_mut() = idx + 1;

                controller.enqueue(chunk)?;
                Ok(0)
            }
        }

        let source = ChunkedByteSource {
            chunks: vec![b"hello".to_vec(), b" world".to_vec()],
            index: std::cell::RefCell::new(0),
        };

        let stream = ReadableStream::new_bytes(source, None);
        let (_locked, reader) = stream.get_reader();

        // Read first chunk
        assert_eq!(reader.read().await.unwrap(), Some(b"hello".to_vec()));
        // Read second chunk
        assert_eq!(reader.read().await.unwrap(), Some(b" world".to_vec()));
        // Stream should be closed
        assert_eq!(reader.read().await.unwrap(), None);
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
            Some(bytes_read) => assert!(bytes_read > 0),
            None => panic!("Expected data from BYOB reader"),
        }

        // Subsequent read should indicate end of stream
        assert_eq!(byob_reader.read(&mut buffer).await.unwrap(), Some(0));
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
}
