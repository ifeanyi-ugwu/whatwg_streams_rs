use super::{
    CountQueuingStrategy, QueuingStrategy, Unlocked,
    errors::StreamError,
    readable_sampling_b::{
        DefaultStream, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    },
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};
use futures::{
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    future::{self, Future},
    stream::StreamExt,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub type StreamResult<T> = Result<T, StreamError>;

/// Commands sent from the writable side to the transform task
#[derive(Debug)]
enum TransformCommand<I> {
    Write {
        chunk: I,
        completion: oneshot::Sender<StreamResult<()>>,
    },
    Close {
        completion: oneshot::Sender<StreamResult<()>>,
    },
    Abort {
        reason: Option<String>,
        completion: oneshot::Sender<StreamResult<()>>,
    },
}

/// TransformStream connecting readable and writable sides
pub struct TransformStream<I, O: Send + 'static> {
    readable: ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>,
    writable: WritableStream<I, TransformWritableSink<I>, Unlocked>,
}

impl<I: Send + 'static, O: Send + 'static> TransformStream<I, O> {
    /// Create a new TransformStream with default strategies
    pub fn new<T>(transformer: T) -> Self
    where
        T: Transformer<I, O> + Send + 'static,
    {
        let default_writable = CountQueuingStrategy::new(1);
        let default_readable = CountQueuingStrategy::new(1);
        Self::new_with_strategies(transformer, default_writable, default_readable)
    }

    /// Create a new TransformStream with custom strategies
    pub fn new_with_strategies<T, WS, RS>(
        transformer: T,
        writable_strategy: WS,
        readable_strategy: RS,
    ) -> Self
    where
        T: Transformer<I, O> + Send + 'static,
        WS: QueuingStrategy<I> + Send + Sync + 'static,
        RS: QueuingStrategy<O> + Send + Sync + 'static,
    {
        let (transform_tx, transform_rx) = unbounded::<TransformCommand<I>>();
        let (controller_tx, controller_rx) = unbounded::<O>();

        // Simple shared state - just for coordination, no queuing
        let errored = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));

        // Create the readable source that receives transformed data
        let readable_source =
            TransformReadableSource::new(controller_rx, Arc::clone(&errored), Arc::clone(&closed));

        // Create the writable sink that sends data to be transformed
        let writable_sink = TransformWritableSink::new(transform_tx);

        // Create the streams - let them handle their own queuing
        let readable = ReadableStream::new_with_strategy(readable_source, readable_strategy);
        let writable = WritableStream::new(writable_sink, Box::new(writable_strategy));

        // Spawn the transform task
        let task_errored = Arc::clone(&errored);
        let task_closed = Arc::clone(&closed);

        let controller = TransformStreamDefaultController::new(
            controller_tx.clone(),
            Arc::clone(&errored),
            Arc::clone(&closed),
            readable.controller.clone(),
            writable.controller.clone(),
        );

        std::thread::spawn(move || {
            futures::executor::block_on(transform_task(
                transformer,
                transform_rx,
                controller_tx,
                task_errored,
                task_closed,
                controller,
            ));
        });

        TransformStream { readable, writable }
    }

    /// Get the readable side
    pub fn readable(
        self,
    ) -> ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked> {
        self.readable
    }

    /// Get the writable side  
    pub fn writable(self) -> WritableStream<I, TransformWritableSink<I>, Unlocked> {
        self.writable
    }

    /// Split into both sides
    pub fn split(
        self,
    ) -> (
        ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>,
        WritableStream<I, TransformWritableSink<I>, Unlocked>,
    ) {
        (self.readable, self.writable)
    }
}

/// Controller for transform operations
pub struct TransformStreamDefaultController<O> {
    controller_tx: UnboundedSender<O>,
    errored: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    readable_controller: Arc<Mutex<ReadableStreamDefaultController<O>>>,
    writable_controller: Arc<Mutex<WritableStreamDefaultController>>,
}

impl<O: Send + 'static> TransformStreamDefaultController<O> {
    fn new(
        controller_tx: UnboundedSender<O>,
        errored: Arc<AtomicBool>,
        closed: Arc<AtomicBool>,
        readable_controller: Arc<ReadableStreamDefaultController<O>>,
        writable_controller: Arc<WritableStreamDefaultController>,
    ) -> Self {
        Self {
            controller_tx,
            errored,
            closed,
            //readable_controller: Mutex::new(readable_controller),
            //writable_controller: Mutex::new(writable_controller),
            readable_controller: Arc::new(Mutex::new((*readable_controller).clone())),
            writable_controller: Arc::new(Mutex::new((*writable_controller).clone())),
        }
    }

    /// Enqueue to readable side
    pub fn enqueue(&mut self, chunk: O) -> StreamResult<()> {
        if self.errored.load(Ordering::SeqCst) {
            return Err(StreamError::Custom("Transform stream is errored".into()));
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(StreamError::Custom("Transform stream is closed".into()));
        }

        self.readable_controller.lock().unwrap().enqueue(chunk)
    }

    /// Errors both the readable and writable side of the transform stream
    pub fn error(&mut self, error: StreamError) -> StreamResult<()> {
        self.errored.store(true, Ordering::SeqCst);
        self.readable_controller
            .lock()
            .unwrap()
            .error(error.clone())?;
        self.writable_controller.lock().unwrap().error(error);
        Ok(())
    }

    /// Closes the readable side and errors the writable side of the stream
    pub fn terminate(&mut self) -> StreamResult<()> {
        self.errored.store(true, Ordering::SeqCst);
        self.readable_controller.lock().unwrap().close()?;
        self.writable_controller
            .lock()
            .unwrap()
            .error(StreamError::Custom("Terminated".into()));
        Ok(())
    }

    /// Get desired size to fill the readable side of the stream's internal queue
    pub fn desired_size(&self) -> Option<isize> {
        if self.errored.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) {
            return None;
        }

        self.readable_controller.lock().unwrap().desired_size()
    }
}

/// Transformer trait
pub trait Transformer<I: Send + 'static, O: Send + 'static>: Send + 'static {
    /// Called once when the transform stream is created
    fn start(
        &mut self,
        controller: &mut TransformStreamDefaultController<O>,
    ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
        let _ = controller;
        future::ready(Ok(()))
    }

    /// Called for each chunk written to the writable side
    fn transform(
        &mut self,
        chunk: I,
        controller: &mut TransformStreamDefaultController<O>,
    ) -> impl Future<Output = StreamResult<()>> + Send + 'static;

    /// Called when the writable side is closed
    fn flush(
        &mut self,
        controller: &mut TransformStreamDefaultController<O>,
    ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
        let _ = controller;
        future::ready(Ok(()))
    }
}

/// Readable source for the transform stream
pub struct TransformReadableSource<O> {
    controller_rx: UnboundedReceiver<O>,
    errored: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl<O> TransformReadableSource<O> {
    fn new(
        controller_rx: UnboundedReceiver<O>,
        errored: Arc<AtomicBool>,
        closed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            controller_rx,
            errored,
            closed,
        }
    }
}

impl<O: Send + 'static> ReadableSource<O> for TransformReadableSource<O> {
    async fn pull(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<O>,
    ) -> StreamResult<()> {
        Ok(())
    }
}

/// Writable sink
pub struct TransformWritableSink<I> {
    transform_tx: UnboundedSender<TransformCommand<I>>,
}

impl<I> TransformWritableSink<I> {
    fn new(transform_tx: UnboundedSender<TransformCommand<I>>) -> Self {
        Self { transform_tx }
    }
}

impl<I: Send + 'static> WritableSink<I> for TransformWritableSink<I> {
    async fn write(
        &mut self,
        chunk: I,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.transform_tx
            .unbounded_send(TransformCommand::Write {
                chunk,
                completion: tx,
            })
            .map_err(|_| StreamError::Custom("Transform stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Write operation canceled".into())))
    }

    async fn close(self) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.transform_tx
            .unbounded_send(TransformCommand::Close { completion: tx })
            .map_err(|_| StreamError::Custom("Transform stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Close operation canceled".into())))
    }

    async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.transform_tx
            .unbounded_send(TransformCommand::Abort {
                reason,
                completion: tx,
            })
            .map_err(|_| StreamError::Custom("Transform stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Abort operation canceled".into())))
    }
}

/// Simplified transform task
async fn transform_task<I, O, T>(
    mut transformer: T,
    mut transform_rx: UnboundedReceiver<TransformCommand<I>>,
    controller_tx: UnboundedSender<O>,
    errored: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    mut controller: TransformStreamDefaultController<O>,
) where
    I: Send + 'static,
    O: Send + 'static,
    T: Transformer<I, O>,
{
    // Call start
    if let Err(error) = transformer.start(&mut controller).await {
        errored.store(true, Ordering::SeqCst);
        let _ = controller.error(error);
        return;
    }

    // Process commands
    while let Some(cmd) = transform_rx.next().await {
        match cmd {
            /*TransformCommand::Write { chunk, completion } => {
                let result = transformer.transform(chunk, &mut controller).await;
                let _ = completion.send(result);
            }*/
            TransformCommand::Write { chunk, completion } => {
                let result = transformer.transform(chunk, &mut controller).await;
                match result {
                    Ok(()) => {
                        let _ = completion.send(Ok(()));
                    }
                    Err(error) => {
                        errored.store(true, Ordering::SeqCst);
                        let _ = controller.error(error.clone());
                        let _ = completion.send(Err(error));
                        break; // stop processing further writes
                    }
                }
            }

            TransformCommand::Close { completion } => {
                let flush_result = transformer.flush(&mut controller).await;
                if let Err(error) = flush_result {
                    errored.store(true, Ordering::SeqCst);
                    let _ = controller.error(error.clone());
                    let _ = completion.send(Err(error));
                } else {
                    closed.store(true, Ordering::SeqCst);
                    let _ = controller.terminate();
                    let _ = completion.send(Ok(()));
                }
                break;
            }

            TransformCommand::Abort {
                reason: _,
                completion,
            } => {
                errored.store(true, Ordering::SeqCst);
                let error = StreamError::Custom("Transform stream aborted".into());

                // Propagate error to both readable and writable sides
                let _ = controller.error(error.clone());

                let _ = completion.send(Err(error));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use std::time::Duration;
    use tokio::time::timeout;

    // Test transformer that converts strings to uppercase
    struct UppercaseTransformer;

    impl Transformer<String, String> for UppercaseTransformer {
        fn transform(
            &mut self,
            chunk: String,
            controller: &mut TransformStreamDefaultController<String>,
        ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
            let result = controller.enqueue(chunk.to_uppercase());
            future::ready(result)
        }
    }

    // Test transformer that multiplies numbers by 2
    struct DoubleTransformer;

    impl Transformer<i32, i32> for DoubleTransformer {
        fn transform(
            &mut self,
            chunk: i32,
            controller: &mut TransformStreamDefaultController<i32>,
        ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
            let result = controller.enqueue(chunk * 2);
            future::ready(result)
        }
    }

    // Test transformer that filters out even numbers and passes odd ones
    struct OddFilterTransformer;

    impl Transformer<i32, i32> for OddFilterTransformer {
        fn transform(
            &mut self,
            chunk: i32,
            controller: &mut TransformStreamDefaultController<i32>,
        ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
            let result = if chunk % 2 != 0 {
                controller.enqueue(chunk)
            } else {
                Ok(()) // Filter out even numbers
            };
            future::ready(result)
        }
    }

    // Test transformer that errors on specific input
    struct ErrorOnThreeTransformer;

    impl Transformer<i32, i32> for ErrorOnThreeTransformer {
        fn transform(
            &mut self,
            chunk: i32,
            controller: &mut TransformStreamDefaultController<i32>,
        ) -> impl Future<Output = StreamResult<()>> + Send + 'static {
            if chunk == 3 {
                future::ready(Err(StreamError::Custom("Cannot process 3".into())))
            } else {
                let result = controller.enqueue(chunk);
                future::ready(result)
            }
        }
    }

    #[tokio::test]
    async fn test_basic_transform() {
        let transformer = UppercaseTransformer;
        let transform_stream = TransformStream::new(transformer);
        let (readable, writable) = transform_stream.split();
        let (_stream, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write some data
        writer.write("hello".to_string()).await.unwrap();
        writer.write("world".to_string()).await.unwrap();
        writer.close().await.unwrap();

        // Read the transformed data
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
    async fn test_numeric_transform() {
        let transformer = DoubleTransformer;
        let transform_stream = TransformStream::new(transformer);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write numbers
        writer.write(5).await.unwrap();
        writer.write(10).await.unwrap();
        writer.write(-3).await.unwrap();
        writer.close().await.unwrap();

        // Read doubled numbers
        assert_eq!(reader.read().await.unwrap(), Some(10));
        assert_eq!(reader.read().await.unwrap(), Some(20));
        assert_eq!(reader.read().await.unwrap(), Some(-6));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_filtering_transform() {
        let transformer = OddFilterTransformer;
        let transform_stream = TransformStream::new(transformer);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write mix of odd and even numbers
        writer.write(1).await.unwrap(); // odd - should pass
        writer.write(2).await.unwrap(); // even - should be filtered
        writer.write(3).await.unwrap(); // odd - should pass  
        writer.write(4).await.unwrap(); // even - should be filtered
        writer.write(5).await.unwrap(); // odd - should pass
        writer.close().await.unwrap();

        // Should only get odd numbers
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(3));
        assert_eq!(reader.read().await.unwrap(), Some(5));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    //TODO: fix, test hangs
    #[tokio::test]
    async fn test_transform_error_handling() {
        let transformer = ErrorOnThreeTransformer;
        let transform_stream = TransformStream::new(transformer);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write data that will cause an error
        writer.write(1).await.unwrap();
        writer.write(2).await.unwrap();

        // This should work fine
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(2));

        // This write should fail
        let write_result = writer.write(3).await;
        assert!(write_result.is_err());

        // Reading should now error too since transform errored
        let read_result = reader.read().await;
        assert!(read_result.is_err());
    }

    #[tokio::test]
    async fn test_empty_stream() {
        let transformer = UppercaseTransformer;
        let transform_stream = TransformStream::new(transformer);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Close immediately without writing
        writer.close().await.unwrap();

        // Should get None immediately
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_multiple_writes_before_read() {
        let transformer = DoubleTransformer;
        let transform_stream = TransformStream::new(transformer);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write multiple items before reading any
        writer.write(1).await.unwrap();
        writer.write(2).await.unwrap();
        writer.write(3).await.unwrap();
        writer.close().await.unwrap();

        // All should be available to read
        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert_eq!(reader.read().await.unwrap(), Some(4));
        assert_eq!(reader.read().await.unwrap(), Some(6));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    //TODO: fix, test hangs
    #[tokio::test]
    async fn test_abort_stream() {
        let transformer = UppercaseTransformer;
        let transform_stream = TransformStream::new(transformer);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        writer.write("hello".to_string()).await.unwrap();

        // Read one item
        assert_eq!(reader.read().await.unwrap(), Some("HELLO".to_string()));

        // Abort the writer
        let abort_result = writer.abort(Some("Test abort".to_string())).await;
        assert!(abort_result.is_err()); // Abort should return error

        // Subsequent reads should fail
        let read_result = reader.read().await;
        assert!(read_result.is_err());
    }
}
