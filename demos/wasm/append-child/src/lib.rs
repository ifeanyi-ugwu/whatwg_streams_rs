use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlElement;
use whatwg_streams::{StreamResult, WritableSink, WritableStream, WritableStreamDefaultController};

/// Sink that appends each written chunk to a parent element as a `<div>`.
struct AppendChildSink {
    parent: HtmlElement,
}

impl WritableSink<String> for AppendChildSink {
    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let document = web_sys::window().unwrap().document().unwrap();
        let div = document.create_element("div").unwrap();
        div.set_text_content(Some(&chunk));
        self.parent.append_child(&div).unwrap();
        Ok(())
    }
}

#[wasm_bindgen]
pub async fn run_append_demo(element_id: &str) {
    let parent: HtmlElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(element_id)
        .unwrap()
        .dyn_into()
        .unwrap();

    let writable = WritableStream::builder(AppendChildSink { parent }).spawn(wasm_bindgen_futures::spawn_local);
    let (_lock, writer) = writable.get_writer().unwrap();

    for chunk in [
        "Hello from Rust + WASM!",
        "Each of these is a stream chunk.",
        "Appended as <div> children.",
    ] {
        writer.write(chunk.to_string()).await.unwrap();
    }
    writer.close().await.unwrap();
}
