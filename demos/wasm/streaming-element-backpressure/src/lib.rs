use gloo_timers::future::TimeoutFuture;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlElement;
use whatwg_streams::{
    CountQueuingStrategy, StreamResult, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

/// Sink that appends each chunk, then waits 100ms — slow enough to exert backpressure
/// against a queue with a high-water mark of 1.
struct BackpressureSink {
    parent: HtmlElement,
}

impl WritableSink<String> for BackpressureSink {
    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let document = web_sys::window().unwrap().document().unwrap();
        let div = document.create_element("div").unwrap();
        div.set_text_content(Some(&chunk));
        self.parent.append_child(&div).unwrap();
        TimeoutFuture::new(100).await;
        Ok(())
    }
}

#[wasm_bindgen]
pub fn run_backpressure_demo(element_id: &str) {
    let el: HtmlElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(element_id)
        .unwrap()
        .dyn_into()
        .unwrap();

    let writable = WritableStream::builder(BackpressureSink { parent: el })
        .strategy(CountQueuingStrategy::new(1))
        .spawn(spawn_local);

    spawn_local(async move {
        let (_lock, writer) = writable.get_writer().unwrap();
        for chunk in [
            "Chunk 1: hello!",
            "Chunk 2: throttled streaming.",
            "Chunk 3: backpressure demo.",
            "Chunk 4: Rust + WASM FTW!",
            "Chunk 5: finishing up.",
        ] {
            writer.write(chunk.to_string()).await.unwrap();
        }
        writer.close().await.unwrap();
    });
}
