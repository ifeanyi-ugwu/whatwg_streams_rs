use gloo_timers::future::TimeoutFuture;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Document, HtmlElement};
use whatwg_streams::local::{WritableSink, WritableStream, WritableStreamDefaultController};
use whatwg_streams::CountQueuingStrategy;

// Sink that appends chunks as <div> children
struct BackpressureSink {
    parent: HtmlElement,
}

impl WritableSink<String> for BackpressureSink {
    fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> impl std::future::Future<Output = whatwg_streams::local::StreamResult<()>> {
        let parent = self.parent.clone();
        async move {
            // Append the chunk
            let document: Document = web_sys::window().unwrap().document().unwrap();
            let div = document.create_element("div").unwrap();
            div.set_inner_html(&chunk);
            parent.append_child(&div).unwrap();

            // Backpressure: wait 100ms before allowing next write
            TimeoutFuture::new(100).await;

            Ok(())
        }
    }
}

#[wasm_bindgen]
pub fn run_backpressure_demo(element_id: &str) {
    let document = web_sys::window().unwrap().document().unwrap();
    let el: HtmlElement = document
        .get_element_by_id(element_id)
        .unwrap()
        .dyn_into()
        .unwrap();

    let sink = BackpressureSink { parent: el };
    let writable = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(spawn_local);

    // Example chunks
    let chunks = vec![
        "Chunk 1: hello!".to_string(),
        "Chunk 2: throttled streaming.".to_string(),
        "Chunk 3: backpressure demo.".to_string(),
        "Chunk 4: Rust + WASM FTW!".to_string(),
        "Chunk 5: finishing up.".to_string(),
    ];

    // Stream chunks asynchronously
    spawn_local(async move {
        let (_, writer) = writable.get_writer().unwrap();
        for chunk in chunks {
            writer.write(chunk).await.unwrap();
        }
        writer.close().await.unwrap();
    });
}
