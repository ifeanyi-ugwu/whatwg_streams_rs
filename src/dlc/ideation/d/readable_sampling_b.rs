use futures::stream::Stream;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

// ----------- Stream Type Markers -----------
pub struct DefaultStream;
pub struct ByteStream;

// ----------- Lock State Markers -----------
pub struct Unlocked;
pub struct Locked;

// ----------- Source Traits with Associated Types -----------
pub trait ReadableSource<T>: Send + Sized + 'static
where
    T: Send + 'static,
{
    type StreamType: StreamTypeMarker;
    type Controller: StreamController<T>;

    fn start(
        &mut self,
        controller: &mut Self::Controller,
    ) -> impl Future<Output = StreamResult<()>> + Send {
        async { StreamResult(Ok(())) }
    }

    fn pull(
        &mut self,
        controller: &mut Self::Controller,
    ) -> impl Future<Output = StreamResult<Option<T>>> + Send;

    fn cancel(&mut self, reason: Option<String>) -> impl Future<Output = StreamResult<()>> + Send {
        async { StreamResult(Ok(())) }
    }
}

pub trait ReadableByteSource: Send + Sized + 'static {
    //type StreamType: StreamTypeMarker = ByteStream;
    //type Controller: StreamController<Vec<u8>> = ReadableByteStreamController;
    type StreamType: StreamTypeMarker;
    type Controller: StreamController<Vec<u8>>;

    fn start(
        &mut self,
        controller: &mut Self::Controller,
    ) -> impl Future<Output = StreamResult<()>> + Send {
        async { StreamResult(Ok(())) }
    }

    fn pull(
        &mut self,
        controller: &mut Self::Controller,
        buffer: &mut [u8],
    ) -> impl Future<Output = StreamResult<usize>> + Send;

    fn cancel(&mut self, reason: Option<String>) -> impl Future<Output = StreamResult<()>> + Send {
        async { StreamResult(Ok(())) }
    }
}

// ----------- Stream Type Marker Trait -----------
pub trait StreamTypeMarker: Send + Sync + 'static {
    type Item: Send + 'static;
    type Controller: StreamController<Self::Item>;
    type Reader<Source, LockState>: StreamReader<Self::Item>
    where
        Source: Send + 'static,
        LockState: Send + 'static;
}

/*impl StreamTypeMarker for DefaultStream {
    type Item = (); // This will be overridden by the actual source
    type Controller = ReadableStreamDefaultController<Self::Item>;
    type Reader<Source, LockState> = ReadableStreamDefaultReader<Self::Item, Source, LockState>;
}*/

impl StreamTypeMarker for DefaultStream {
    type Item = ();
    type Controller = ReadableStreamDefaultController<Self::Item>;
    type Reader<Source, LockState>
        = ReadableStreamDefaultReader<Self::Item, Source, LockState>
    where
        Source: Send + 'static,
        LockState: Send + 'static;
}

/*impl StreamTypeMarker for ByteStream {
    type Item = Vec<u8>;
    type Controller = ReadableByteStreamController;
    type Reader<Source, LockState> = ReadableStreamBYOBReader<Source, LockState>;
}*/

impl StreamTypeMarker for ByteStream {
    type Item = Vec<u8>;
    type Controller = ReadableByteStreamController;
    type Reader<Source, LockState>
        = ReadableStreamBYOBReader<Source, LockState>
    where
        Source: Send + 'static,
        LockState: Send + 'static;
}

// ----------- Controller Trait -----------
pub trait StreamController<T>: Send + 'static {
    fn enqueue(&mut self, chunk: T) -> StreamResult<()>;
    fn close(&mut self) -> StreamResult<()>;
    fn error(&mut self, error: StreamError);
}

// ----------- Main ReadableStream with Typestate -----------
pub struct ReadableStream<T, Source, StreamType, LockState = Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
    LockState: Send + 'static,
{
    // Your existing fields
    command_tx: std::sync::mpsc::Sender<StreamCommand<T>>, // Simplified for example
    _phantom: PhantomData<(T, Source, StreamType, LockState)>,
}

// ----------- Constructor Implementation -----------
impl<T, Source> ReadableStream<T, Source, DefaultStream, Unlocked>
where
    T: Send + 'static,
    Source: ReadableSource<T, StreamType = DefaultStream> + Send + 'static,
{
    pub fn new_default(source: Source) -> Self {
        // Implementation for default streams
        Self {
            command_tx: todo!(), // Your actual implementation
            _phantom: PhantomData,
        }
    }
}

impl<Source> ReadableStream<Vec<u8>, Source, ByteStream, Unlocked>
where
    Source: ReadableByteSource + Send + 'static,
{
    pub fn new_bytes(source: Source) -> Self {
        // Implementation for byte streams
        Self {
            command_tx: todo!(), // Your actual implementation
            _phantom: PhantomData,
        }
    }
}

// ----------- Generic Constructor (The Rusty Way) -----------
impl<T, Source, StreamType> ReadableStream<T, Source, StreamType, Unlocked>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker,
{
    /// Generic constructor that works for any source that implements the right trait
    pub fn new(source: Source) -> Self
    where
        Source: ReadableSource<T, StreamType = StreamType>,
    {
        Self {
            command_tx: todo!(), // Your actual implementation
            _phantom: PhantomData,
        }
    }
}

// ----------- Reader Methods -----------
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
        StreamType::Reader<Source, Locked>,
    ) {
        let locked_stream = ReadableStream {
            command_tx: self.command_tx,
            _phantom: PhantomData,
        };

        // The reader type is determined by the StreamType
        let reader = todo!(); // Create the appropriate reader type

        (locked_stream, reader)
    }
}

// ----------- Stream Trait Implementation -----------
impl<T, Source, StreamType, LockState> Stream for ReadableStream<T, Source, StreamType, LockState>
where
    T: Send + 'static,
    Source: Send + 'static,
    StreamType: StreamTypeMarker<Item = T>,
    LockState: Send + 'static,
{
    type Item = StreamResult<T>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // Your polling implementation
        todo!()
    }
}

// ----------- Example Source Implementations -----------

// Default stream source
pub struct IteratorSource<I> {
    iter: I,
}

impl<I, T> ReadableSource<T> for IteratorSource<I>
where
    I: Iterator<Item = T> + Send + 'static,
    T: Send + 'static,
{
    type StreamType = DefaultStream;
    type Controller = ReadableStreamDefaultController<T>;

    async fn pull(&mut self, _controller: &mut Self::Controller) -> StreamResult<Option<T>> {
        StreamResult(Ok(self.iter.next()))
    }
}

// Byte stream source
pub struct FileSource {
    // File handle or similar
}

impl ReadableByteSource for FileSource {
    type StreamType = ByteStream;
    type Controller = ReadableByteStreamController;

    async fn pull(
        &mut self,
        _controller: &mut Self::Controller,
        buffer: &mut [u8],
    ) -> StreamResult<usize> {
        // Read bytes into buffer
        StreamResult(Ok(0)) // Placeholder
    }
}

// ----------- Usage Examples -----------
pub fn usage_examples() {
    // This automatically creates a default stream
    let default_stream = ReadableStream::new(IteratorSource {
        iter: vec![1, 2, 3].into_iter(),
    });

    // This automatically creates a byte stream
    let byte_stream = ReadableStream::new_bytes(FileSource {});

    // The type system ensures you get the right reader
    let (_locked_stream, _reader) = default_stream.get_reader();
    // reader is automatically ReadableStreamDefaultReader

    let (_locked_byte_stream, _byte_reader) = byte_stream.get_reader();
    // byte_reader is automatically ReadableStreamBYOBReader
}

// ----------- Placeholder types for completeness -----------
pub struct StreamResult<T>(Result<T, StreamError>);
pub struct StreamError;
pub struct ReadableStreamDefaultController<T>(PhantomData<T>);
pub struct ReadableByteStreamController;
pub struct ReadableStreamDefaultReader<T, Source, LockState>(PhantomData<(T, Source, LockState)>);
pub struct ReadableStreamBYOBReader<Source, LockState>(PhantomData<(Source, LockState)>);
pub enum StreamCommand<T> {
    Placeholder(PhantomData<T>),
}

pub trait StreamReader<T> {
    // Common reader methods
}

impl<T, Source, LockState> StreamReader<T> for ReadableStreamDefaultReader<T, Source, LockState> {}
impl<Source, LockState> StreamReader<Vec<u8>> for ReadableStreamBYOBReader<Source, LockState> {}

impl<T: Send + 'static> StreamController<T> for ReadableStreamDefaultController<T> {
    fn enqueue(&mut self, _chunk: T) -> StreamResult<()> {
        todo!()
    }
    fn close(&mut self) -> StreamResult<()> {
        todo!()
    }
    fn error(&mut self, _error: StreamError) {
        todo!()
    }
}

impl StreamController<Vec<u8>> for ReadableByteStreamController {
    fn enqueue(&mut self, _chunk: Vec<u8>) -> StreamResult<()> {
        todo!()
    }
    fn close(&mut self) -> StreamResult<()> {
        todo!()
    }
    fn error(&mut self, _error: StreamError) {
        todo!()
    }
}
