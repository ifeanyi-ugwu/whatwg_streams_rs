use std::error::Error;
use whatwg_streams::streams::{
    ByteLengthQueuingStrategy, CountQueuingStrategy, PipeOptions, ReadableSource, ReadableStream,
    ReadableStreamDefaultController, ReadableStreamPipeExt, ReadableStreamPipeThroughExt,
    StreamError, StreamFuture, StreamResult, TransformStream, TransformStreamDefaultController,
    Transformer, WritableSink, WritableStream, WritableStreamDefaultController,
};

use std::cell::RefCell;
// Example 1: A simple readable stream that produces incremental numbers
struct CounterSource {
    //current: u32,
    //max: u32,
    state: RefCell<CounterState>,
}

#[derive(Clone)]
struct CounterState {
    current: u32,
    max: u32,
}

impl ReadableSource<u32> for CounterSource {
    fn start(&mut self, controller: &mut ReadableStreamDefaultController<u32>) -> StreamResult<()> {
        // No initialization needed
        Ok(())
    }

    /*fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamFuture<'_, ()> {
        Box::pin(async move {
            if self.current <= self.max {
                controller.enqueue(self.current)?;
                self.current += 1;
                Ok(())
            } else {
                controller.close()?;
                Ok(())
            }
        })
    }*/
    fn pull<'a>(
        &mut self,
        controller: &'a mut ReadableStreamDefaultController<u32>,
    ) -> StreamFuture<'a, ()> {
        // 1. Copy the necessary data *before* creating the future.
        let state_clone = self.state.borrow().clone(); // Clone the CounterState

        // To modify the state, we'll need to do it within the future
        let state_cell = self.state.clone(); // Clone the RefCell

        // 2. Move the copied data into the async block.
        Box::pin(async move {
            let mut state = state_cell.borrow_mut(); // Borrow mutably inside the future
            if state.current <= state_clone.max {
                // Use the cloned state's max
                controller.enqueue(state_clone.current)?; // Use the cloned state's current
                state.current += 1; // Modify the state within the RefCell
                Ok(())
            } else {
                controller.close()?;
                Ok(())
            }
        })
    }

    fn cancel(&mut self, _reason: Option<String>) -> StreamFuture<'_, ()> {
        // Nothing to clean up
        Box::pin(async { Ok(()) })
    }
}

// Example 2: A writable stream that prints values
struct ConsoleWriterSink;

impl WritableSink<String> for ConsoleWriterSink {
    fn start(&mut self, _controller: &mut WritableStreamDefaultController) -> StreamResult<()> {
        // No initialization needed
        Ok(())
    }

    fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamFuture<'_, ()> {
        Box::pin(async move {
            println!("Received: {}", chunk);
            Ok(())
        })
    }

    fn close(&mut self) -> StreamFuture<'_, ()> {
        Box::pin(async {
            println!("Stream closed");
            Ok(())
        })
    }

    fn abort(&mut self, reason: Option<String>) -> StreamFuture<'_, ()> {
        Box::pin(async move {
            println!("Stream aborted: {:?}", reason);
            Ok(())
        })
    }
}

// Example 3: A transform stream that converts numbers to strings
struct NumberToStringTransformer;

impl Transformer<u32, String> for NumberToStringTransformer {
    fn start(
        &mut self,
        _controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        // No initialization needed
        Ok(())
    }

    /*fn transform(
        &mut self,
        chunk: u32,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamFuture<'_, ()> {
        Box::pin(async move {
            let string_value = chunk.to_string();
            controller.enqueue(string_value)?;
            Ok(())
        })
    }*/

    fn transform<'a>(
        &'a mut self,
        chunk: u32,
        controller: &'a mut TransformStreamDefaultController<String>,
    ) -> StreamFuture<'a, ()> {
        let chunk_copy = chunk;

        Box::pin(async move {
            let string_value = chunk_copy.to_string();
            controller.enqueue(string_value)?;
            Ok(())
        })
    }

    fn flush(
        &mut self,
        _controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamFuture<'_, ()> {
        // Nothing to flush
        Box::pin(async { Ok(()) })
    }
}

// Example usage
async fn stream_example() -> Result<(), Box<dyn Error>> {
    // Create a readable stream of numbers
    /*let mut readable = ReadableStream::new(CounterSource {
        current: 1,
        max: 10,
    });*/
    let mut readable = ReadableStream::new(CounterSource {
        state: RefCell::new(CounterState {
            current: 1,
            max: 10,
        }),
    });

    // Create a writable stream that prints strings
    let mut writable = WritableStream::new(ConsoleWriterSink);

    // Create a transform stream that converts numbers to strings
    let mut transform = TransformStream::new(NumberToStringTransformer);

    // Pipe the readable stream to the transform stream's writable side
    /*readable
        .pipe_to(transform.writable(), PipeOptions::default())
        .await?;

    // Pipe the transform stream's readable side to the writable stream
    transform
        .readable()
        .pipe_to(&mut writable, PipeOptions::default())
        .await?;*/

    // Pipe the readable stream through the transform stream
    let mut transformed_readable = readable.pipe_through(transform, PipeOptions::default());

    // Pipe the transformed readable stream to the writable stream
    transformed_readable
        .pipe_to(&mut writable, PipeOptions::default())
        .await?;

    Ok(())
}

// Manual usage example without piping
async fn manual_stream_example() -> Result<(), Box<dyn Error>> {
    // Create a readable stream of numbers
    // let mut readable = ReadableStream::new(CounterSource { current: 1, max: 5 });
    let mut readable = ReadableStream::new(CounterSource {
        state: RefCell::new(CounterState { current: 1, max: 5 }),
    });

    // Get a reader for the readable stream
    let mut reader = readable.get_reader();

    // Create a writable stream
    let mut writable = WritableStream::new(ConsoleWriterSink);

    // Get a writer for the writable stream
    let mut writer = writable.get_writer();

    // Read from the readable stream and write to the writable stream
    while let Some(number) = reader.read().await? {
        let string_value = number.to_string();
        writer.write(string_value).await?;
    }

    // Close the writable stream
    writer.close().await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("Running stream example with automatic piping:");
    stream_example().await?;

    println!("\nRunning stream example with manual reading/writing:");
    manual_stream_example().await?;

    Ok(())
}
