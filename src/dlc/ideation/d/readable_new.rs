use super::QueuingStrategy;
use futures::{
    FutureExt, SinkExt,
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    future::{self, poll_fn},
    stream::StreamExt,
};
use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Poll, Waker},
};

// ----------- Error / Result Types -----------

#[derive(Clone)]
pub struct ArcError(Arc<dyn std::error::Error + Send + Sync>);

impl fmt::Display for ArcError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ArcError: {}", self.0)
    }
}
impl fmt::Debug for ArcError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ArcError: {:?}", self.0)
    }
}
impl std::error::Error for ArcError {}

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
    Custom(ArcError),
}

impl<E: std::error::Error + Send + Sync + 'static> From<E> for StreamError {
    fn from(e: E) -> Self {
        StreamError::Custom(ArcError(Arc::new(e)))
    }
}

pub type StreamResult<T> = Result<T, StreamError>;

// ----------- Stream States -----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Readable,
    Closed,
    Errored,
}

// ----------- Typestate -----------

pub struct Unlocked;
pub struct Locked;

// ----------- Readable Source (like your trait) -----------

pub trait ReadableSource<T: Send + 'static>: Sized {
    fn start(
        &mut self,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> impl Future<Output = StreamResult<()>> + Send {
        future::ready(Ok(()))
    }

    fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<T>,
    ) -> impl Future<Output = StreamResult<Option<T>>> + Send + 'static {
        future::ready(Ok(None)).boxed()
    }

    fn cancel(
        &mut self,
        reason: Option<String>,
    ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
        future::ready(Ok(())).boxed()
    }
}

// ----------- Controller -----------

pub struct ReadableStreamDefaultController<T> {
    // Internal connection to the stream, to enqueue/close/error
    tx: UnboundedSender<ControllerMsg<T>>,
}

impl<T> Clone for ReadableStreamDefaultController<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<T> ReadableStreamDefaultController<T> {
    fn new(tx: UnboundedSender<ControllerMsg<T>>) -> Self {
        Self { tx }
    }

    pub fn enqueue(&self, chunk: T) -> StreamResult<()> {
        self.tx
            .unbounded_send(ControllerMsg::Enqueue { chunk })
            .map_err(|_| StreamError::Custom(ArcError::from("Failed to enqueue chunk")))?;
        Ok(())
    }

    pub fn close(&self) -> StreamResult<()> {
        self.tx
            .unbounded_send(ControllerMsg::Close)
            .map_err(|_| StreamError::Custom(ArcError::from("Failed to close stream")))?;
        Ok(())
    }

    pub fn error(&self, err: StreamError) {
        let _ = self.tx.unbounded_send(ControllerMsg::Error(err));
    }
}

enum ControllerMsg<T> {
    Enqueue { chunk: T },
    Close,
    Error(StreamError),
}

// ----------- Inner State -----------

struct ReadableStreamInner<T, Source> {
    state: StreamState,
    queue: VecDeque<T>,
    queue_total_size: usize,
    strategy: Box<dyn QueuingStrategy<T> + Send + Sync>,
    source: Option<Source>,

    locked: bool,

    cancel_requested: bool,
    cancel_reason: Option<String>,
    cancel_completions: Vec<oneshot::Sender<StreamResult<()>>>,

    // Pending reads are queued here to resolve in order when chunks arrive
    pending_reads: VecDeque<oneshot::Sender<StreamResult<Option<T>>>>,

    ready_wakers: WakerSet,
    closed_wakers: WakerSet,
}

// Helper for backpressure and updating state
impl<T, Source> ReadableStreamInner<T, Source>
where
    T: Send + 'static,
    Source: ReadableSource<T> + Send + 'static,
{
    fn update_backpressure(&mut self, backpressure: &Arc<AtomicBool>) {
        let total = self.queue_total_size;
        let prev = backpressure.load(Ordering::SeqCst);
        let cond = total >= self.strategy.high_water_mark();
        backpressure.store(cond, Ordering::SeqCst);
        if prev && !cond {
            self.ready_wakers.wake_all();
        }
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

pub enum ReadableStreamCommand<T> {
    Read {
        completion: oneshot::Sender<StreamResult<Option<T>>>,
    },
    Cancel {
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
}

// ----------- ReadableStream Struct -----------

pub struct ReadableStream<T, Source, S = Unlocked> {
    command_tx: UnboundedSender<ReadableStreamCommand<T>>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    _source: PhantomData<Source>,
    _state: PhantomData<S>,
}

impl<T, Source, S> Clone for ReadableStream<T, Source, S> {
    fn clone(&self) -> Self {
        Self {
            command_tx: self.command_tx.clone(),
            backpressure: Arc::clone(&self.backpressure),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            _source: PhantomData,
            _state: PhantomData,
        }
    }
}

impl<T, Source> ReadableStream<T, Source, Unlocked>
where
    T: Send + 'static,
    Source: ReadableSource<T> + Send + 'static,
{
    pub fn new(source: Source, strategy: Box<dyn QueuingStrategy<T> + Send + Sync>) -> Self {
        let (command_tx, command_rx) = unbounded::<ReadableStreamCommand<T>>();
        //let (ctrl_tx, ctrl_rx) = unbounded::<ControllerMsg<T>>();

        let inner = ReadableStreamInner {
            state: StreamState::Readable,
            queue: VecDeque::new(),
            queue_total_size: 0,
            strategy,
            source: Some(source),
            locked: false,
            cancel_requested: false,
            cancel_reason: None,
            cancel_completions: Vec::new(),
            pending_reads: VecDeque::new(),
            ready_wakers: WakerSet::new(),
            closed_wakers: WakerSet::new(),
        };

        let backpressure = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));

        let b2 = backpressure.clone();
        let c2 = closed.clone();
        let e2 = errored.clone();

        // Spawn async task handling the stream events and commands:
        std::thread::spawn(move || {
            futures::executor::block_on(readable_stream_task(
                command_rx, /*ctrl_rx,*/ inner, b2, c2, e2,
            ));
        });

        Self {
            command_tx,
            backpressure,
            closed,
            errored,
            _source: PhantomData,
            _state: PhantomData,
        }
    }

    pub async fn get_reader(
        self,
    ) -> Result<
        (
            ReadableStream<T, Source, Locked>,
            ReadableStreamDefaultReader<T, Source>,
        ),
        StreamError,
    > {
        let (tx, rx) = oneshot::channel();

        self.command_tx
            .clone()
            .send(ReadableStreamCommand::LockStream { completion: tx })
            .await
            .map_err(|_| StreamError::Custom(ArcError::from("Stream task dropped")))?;

        rx.await
            .map_err(|_| StreamError::Custom(ArcError::from("Lock response lost")))??;

        let locked = ReadableStream {
            command_tx: self.command_tx.clone(),
            backpressure: Arc::clone(&self.backpressure),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            _source: PhantomData,
            _state: PhantomData::<Locked>,
        };

        Ok((locked.clone(), ReadableStreamDefaultReader::new(locked)))
    }
}

impl<T, Source, S> ReadableStream<T, Source, S> {
    pub async fn cancel(&self, reason: Option<String>) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .clone()
            .send(ReadableStreamCommand::Cancel {
                reason,
                completion: tx,
            })
            .await
            .map_err(|_| StreamError::Custom(ArcError::from("Stream task dropped")))?;
        //rx.await.map_err(...).flatten()
        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom(ArcError::from("Cancel canceled"))))
    }
}

impl<T, Source> ReadableStream<T, Source, Locked>
where
    T: Send + 'static,
    Source: ReadableSource<T> + Send + 'static,
{
    /*pub async fn release_lock(self) -> ReadableStream<T, Source, Unlocked> {
        let (tx, rx) = oneshot::channel();

        self.command_tx
            .clone()
            .send(ReadableStreamCommand::UnlockStream { completion: tx })
            .await
            .expect("Stream task dropped");

        rx.await
            .expect("Unlock response lost")
            .expect("Unlock failed");

        ReadableStream {
            command_tx: self.command_tx.clone(),
            backpressure: Arc::clone(&self.backpressure),
            closed: Arc::clone(&self.closed),
            errored: Arc::clone(&self.errored),
            _source: PhantomData,
            _state: PhantomData,
        }
    }*/
}

// ----------- Reader -----------

pub struct ReadableStreamDefaultReader<T, Source> {
    stream: ReadableStream<T, Source, Locked>,
}

impl<T, Source> ReadableStreamDefaultReader<T, Source>
where
    T: Send + 'static,
    Source: ReadableSource<T> + Send + 'static,
{
    fn new(stream: ReadableStream<T, Source, Locked>) -> Self {
        Self { stream }
    }

    /// Read one chunk, awaits if none buffered
    pub async fn read(&self) -> StreamResult<Option<T>> {
        let (tx, rx) = oneshot::channel();

        self.stream
            .command_tx
            .clone()
            .send(ReadableStreamCommand::Read { completion: tx })
            .await
            .map_err(|_| StreamError::Custom(ArcError::from("Stream task dropped")))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom(ArcError::from("Read canceled"))))
    }

    /// Cancel the stream
    pub async fn cancel(&self, reason: Option<String>) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.stream
            .command_tx
            .clone()
            .send(ReadableStreamCommand::Cancel {
                reason,
                completion: tx,
            })
            .await
            .map_err(|_| StreamError::Custom(ArcError::from("Stream task dropped")))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom(ArcError::from("Cancel canceled"))))
    }

    /// Returns a future resolving when stream is closed/errored
    pub async fn closed(&self) -> StreamResult<()> {
        // Just wait for closed flag or errored flag to be true
        poll_fn(|cx| {
            if self.stream.errored.load(Ordering::SeqCst) {
                return Poll::Ready(Err(StreamError::Custom(ArcError::from("Stream errored"))));
            }
            if self.stream.closed.load(Ordering::SeqCst) {
                return Poll::Ready(Ok(()));
            }
            // Register waker for closed notification
            let waker = cx.waker().clone();
            self.stream
                .command_tx
                .unbounded_send(ReadableStreamCommand::RegisterClosedWaker { waker })
                .ok();
            Poll::Pending
        })
        .await
    }

    pub async fn release_lock(self) -> ReadableStream<T, Source, Unlocked> {
        let (tx, rx) = oneshot::channel();

        // Send UnlockStream command to stream task
        self.stream
            .command_tx
            .clone()
            .send(ReadableStreamCommand::UnlockStream { completion: tx })
            .await
            .expect("Stream task dropped");

        // Await confirmation
        rx.await
            .expect("Unlock response lost")
            .expect("Unlock failed");

        // Return a new unlocked stream instance
        ReadableStream {
            command_tx: self.stream.command_tx.clone(),
            backpressure: Arc::clone(&self.stream.backpressure),
            closed: Arc::clone(&self.stream.closed),
            errored: Arc::clone(&self.stream.errored),
            _source: PhantomData,
            _state: PhantomData,
        }
    }
}

enum InflightFuture<T> {
    Pull(Pin<Box<dyn Future<Output = StreamResult<Option<T>>> + Send>>),
    //Cancel(Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>),
    /*Cancel {
        future: Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>,
        source: Source,
    },*/
    Cancel(Pin<Box<dyn Future<Output = StreamResult<()>> + Send>>),
}

// ----------- The async stream task -----------

enum TaskState<T, Source> {
    Idle,
    Pulling {
        fut: Pin<Box<dyn Future<Output = StreamResult<Option<T>>> + Send + 'static>>,
        source: Source,
    },
    Canceling {
        fut: Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'static>>,
        source: Source,
    },
    Closed,
    Errored,
}

async fn readable_stream_task<T, Source>(
    mut command_rx: UnboundedReceiver<ReadableStreamCommand<T>>,
    mut inner: ReadableStreamInner<T, Source>,
    backpressure: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
) where
    T: Send + 'static,
    Source: ReadableSource<T> + Send + 'static,
{
    use TaskState::*;

    let mut state: TaskState<T, Source> = TaskState::Idle;

    // For controller message handling, if you have a receiver, e.g.:
    // let mut ctrl_rx = inner.ctrl_rx.clone();
    let (ctrl_tx, mut ctrl_rx) = unbounded::<ControllerMsg<T>>();

    poll_fn(move |cx| {
        loop {
            // 1. Handle controller messages — example, adjust to your controller receiver
            // (Assuming inner has a controller receiver, else skip)
            while let Ok(Some(msg)) = ctrl_rx.try_next() {
                match msg {
                    ControllerMsg::Enqueue { chunk } => {
                        if let Some(tx) = inner.pending_reads.pop_front() {
                            let _ = tx.send(Ok(Some(chunk)));
                        } else {
                            let sz = inner.strategy.size(&chunk);
                            inner.queue.push_back(chunk);
                            inner.queue_total_size += sz;
                            inner.update_backpressure(&backpressure);
                        }
                        inner.ready_wakers.wake_all();
                    }
                    ControllerMsg::Close => {
                        if inner.state != StreamState::Errored {
                            inner.state = StreamState::Closed;
                            while let Some(tx) = inner.pending_reads.pop_front() {
                                let _ = tx.send(Ok(None));
                            }
                            inner.closed_wakers.wake_all();
                            closed.store(true, Ordering::SeqCst);
                        }
                    }
                    ControllerMsg::Error(err) => {
                        if inner.state != StreamState::Closed && inner.state != StreamState::Errored
                        {
                            inner.state = StreamState::Errored;
                            inner.queue.clear();
                            inner.queue_total_size = 0;
                            while let Some(tx) = inner.pending_reads.pop_front() {
                                let _ = tx.send(Err(err.clone()));
                            }
                            inner.ready_wakers.wake_all();
                            inner.closed_wakers.wake_all();
                            errored.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }

            // 2. Drain commands
            loop {
                match command_rx.poll_next_unpin(cx) {
                    Poll::Ready(Some(cmd)) => match cmd {
                        ReadableStreamCommand::Read { completion } => {
                            // Handle Read command
                            if inner.state == StreamState::Errored {
                                let _ = completion.send(Err(StreamError::Custom(ArcError::from(
                                    "Stream is errored",
                                ))));
                                continue;
                            }
                            if inner.state == StreamState::Closed && inner.queue.is_empty() {
                                let _ = completion.send(Ok(None));
                                continue;
                            }
                            if let Some(chunk) = inner.queue.pop_front() {
                                let sz = inner.strategy.size(&chunk);
                                inner.queue_total_size -= sz;
                                inner.update_backpressure(&backpressure);
                                let _ = completion.send(Ok(Some(chunk)));
                            } else {
                                // No chunk available, enqueue completion
                                inner.pending_reads.push_back(completion);
                            }
                        }
                        ReadableStreamCommand::Cancel { reason, completion } => {
                            if inner.state == StreamState::Closed
                                || inner.state == StreamState::Errored
                            {
                                let _ = completion.send(Ok(()));
                                continue;
                            }

                            if inner.cancel_requested {
                                inner.cancel_completions.push(completion);
                            } else {
                                inner.cancel_requested = true;
                                inner.cancel_reason = reason;
                                inner.cancel_completions.push(completion);

                                // Immediately clear queue as per spec
                                inner.queue.clear();
                                inner.queue_total_size = 0;
                                inner.ready_wakers.wake_all();
                            }
                        }
                        ReadableStreamCommand::GetDesiredSize { completion } => {
                            let hwm = inner.strategy.high_water_mark();
                            let total = inner.queue_total_size;
                            let size = if inner.state == StreamState::Readable {
                                if total >= hwm {
                                    Some(0)
                                } else {
                                    Some(hwm - total)
                                }
                            } else {
                                None
                            };
                            let _ = completion.send(size);
                        }
                        ReadableStreamCommand::RegisterReadyWaker { waker } => {
                            inner.ready_wakers.register(&waker);
                        }
                        ReadableStreamCommand::RegisterClosedWaker { waker } => {
                            inner.closed_wakers.register(&waker);
                        }
                        ReadableStreamCommand::LockStream { completion } => {
                            if inner.locked {
                                let _ = completion.send(Err(StreamError::Custom(ArcError::from(
                                    "Stream already locked",
                                ))));
                            } else {
                                inner.locked = true;
                                let _ = completion.send(Ok(()));
                            }
                        }
                        ReadableStreamCommand::UnlockStream { completion } => {
                            if !inner.locked {
                                let _ = completion.send(Err(StreamError::Custom(ArcError::from(
                                    "Stream not locked",
                                ))));
                            } else {
                                inner.locked = false;
                                let _ = completion.send(Ok(()));
                            }
                        }
                    },
                    Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Pending => break,
                }
            }

            // 3. Drive state machine transitions

            let current_state = std::mem::replace(&mut state, TaskState::Idle);

            match current_state {
                TaskState::Idle => {
                    if inner.cancel_requested {
                        // Start cancellation future if source present
                        if let Some(mut source) = inner.source.take() {
                            let cancel_fut = source.cancel(inner.cancel_reason.take());
                            TaskState::Canceling {
                                fut: Box::pin(cancel_fut),
                                source,
                            }
                        } else {
                            // No source, resolve cancel and close immediately
                            for c in inner.cancel_completions.drain(..) {
                                let _ = c.send(Ok(()));
                            }
                            inner.state = StreamState::Closed;
                            closed.store(true, Ordering::SeqCst);
                            inner.cancel_requested = false;

                            TaskState::Closed
                        }
                    } else if inner.state == StreamState::Readable
                        && !backpressure.load(Ordering::SeqCst)
                        && inner.queue_total_size < inner.strategy.high_water_mark()
                    {
                        // Start a new pull if source available
                        if let Some(mut source) = inner.source.take() {
                            let mut ctrl = ReadableStreamDefaultController::new(ctrl_tx.clone());
                            let pull_fut = source.pull(&mut ctrl);
                            TaskState::Pulling {
                                fut: Box::pin(pull_fut),
                                source,
                            }
                        } else {
                            Idle
                        }
                    } else if inner.state == StreamState::Closed
                        || inner.state == StreamState::Errored
                    {
                        // Terminal states
                        TaskState::Closed
                    } else {
                        Idle
                    }
                }
                TaskState::Pulling { mut fut, source } => match fut.as_mut().poll(cx) {
                    Poll::Ready(res) => {
                        inner.source = Some(source);
                        match res {
                            Ok(Some(chunk)) => {
                                let sz = inner.strategy.size(&chunk);
                                inner.queue.push_back(chunk);
                                inner.queue_total_size += sz;
                                inner.update_backpressure(&backpressure);
                                inner.ready_wakers.wake_all();

                                TaskState::Idle
                            }
                            Ok(None) => {
                                inner.state = StreamState::Closed;
                                while let Some(tx) = inner.pending_reads.pop_front() {
                                    let _ = tx.send(Ok(None));
                                }
                                inner.closed_wakers.wake_all();
                                closed.store(true, Ordering::SeqCst);

                                TaskState::Closed
                            }
                            Err(e) => {
                                inner.state = StreamState::Errored;
                                inner.queue.clear();
                                inner.queue_total_size = 0;
                                while let Some(tx) = inner.pending_reads.pop_front() {
                                    let _ = tx.send(Err(e.clone()));
                                }
                                inner.ready_wakers.wake_all();
                                inner.closed_wakers.wake_all();
                                errored.store(true, Ordering::SeqCst);

                                TaskState::Errored
                            }
                        }
                    }
                    Poll::Pending => TaskState::Pulling { fut, source },
                },
                TaskState::Canceling { mut fut, source } => match fut.as_mut().poll(cx) {
                    Poll::Ready(res) => {
                        inner.source = None; // Source unusable after cancel
                        match res {
                            Ok(_) => {
                                inner.state = StreamState::Closed;
                                closed.store(true, Ordering::SeqCst);

                                for c in inner.cancel_completions.drain(..) {
                                    let _ = c.send(Ok(()));
                                }
                                while let Some(tx) = inner.pending_reads.pop_front() {
                                    let _ = tx.send(Ok(None));
                                }
                                inner.closed_wakers.wake_all();

                                TaskState::Closed
                            }
                            Err(e) => {
                                inner.state = StreamState::Errored;
                                errored.store(true, Ordering::SeqCst);

                                for c in inner.cancel_completions.drain(..) {
                                    let _ = c.send(Err(e.clone()));
                                }
                                while let Some(tx) = inner.pending_reads.pop_front() {
                                    let _ = tx.send(Err(e.clone()));
                                }
                                inner.closed_wakers.wake_all();
                                inner.ready_wakers.wake_all();

                                TaskState::Errored
                            }
                        }
                    }
                    Poll::Pending => TaskState::Canceling { fut, source },
                },
                TaskState::Closed => return Poll::Ready(()),
                TaskState::Errored => return Poll::Ready(()),
            };

            return Poll::Pending;
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use crate::dlc::ideation::d::CountQueuingStrategy;

    use super::*;

    struct MockSource {
        chunks: Vec<&'static str>,
        canceled: bool,
    }

    impl ReadableSource<String> for MockSource {
        fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<String>,
        ) -> impl Future<Output = StreamResult<Option<String>>> + Send + 'static {
            // Immediate ready future; owns data, no ref borrows after call
            if self.chunks.is_empty() {
                future::ready(Ok(None))
            } else {
                let chunk = self.chunks.remove(0).to_string();
                future::ready(Ok(Some(chunk)))
            }
        }

        fn cancel(
            &mut self,
            _reason: Option<String>,
        ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
            self.canceled = true;
            future::ready(Ok(())).boxed()
        }
    }
    #[tokio::test]
    async fn test_read_stream() {
        let source = MockSource {
            chunks: vec!["a", "b"],
            canceled: false,
        };
        let stream = ReadableStream::new(source, Box::new(CountQueuingStrategy::new(1)));

        let (locked, reader) = stream.get_reader().await.unwrap();

        assert_eq!(reader.read().await.unwrap(), Some("a".to_string()));
        assert_eq!(reader.read().await.unwrap(), Some("b".to_string()));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_cancel_stream() {
        let source = MockSource {
            chunks: vec!["a", "b"],
            canceled: false,
        };
        let stream = ReadableStream::new(source, Box::new(CountQueuingStrategy::new(1)));

        let (locked, reader) = stream.get_reader().await.unwrap();

        let cancel_res = reader.cancel(Some("testing".to_string())).await;
        assert!(cancel_res.is_ok());
        // Source cancel sets canceled flag
        // Here you’d verify source.canceled flag if accessible
    }

    #[tokio::test]
    async fn test_backpressure() {
        // This is conceptual — you need a strategy that controls backpressure size
        // and a source that fills slowly.
        // You can test that stream.backpressure reflects expected behavior.

        let source = MockSource {
            chunks: vec!["chunk".into(); 10],
            canceled: false,
        };
        let stream = ReadableStream::new(source, Box::new(CountQueuingStrategy::new(5)));

        // Initially, desired size reflects high water mark

        //let desired1 = stream.desired_size().await;
        //assert!(desired1.is_some());

        let (locked, reader) = stream.get_reader().await.unwrap();

        // Read chunks and verify backpressure updates from stream signals accordingly
        // ...
    }
}
