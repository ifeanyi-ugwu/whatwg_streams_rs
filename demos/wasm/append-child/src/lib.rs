use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlElement;
use whatwg_streams::local::{WritableSink, WritableStream, WritableStreamDefaultController};

// Our custom sink: takes strings and appends them as <div> children
struct AppendChildSink {
    parent: HtmlElement,
}

#[wasm_bindgen]
pub async fn run_append_demo(element_id: &str) {
    // Locate parent element in DOM
    let parent: HtmlElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(element_id)
        .unwrap()
        .dyn_into::<HtmlElement>()
        .unwrap();

    // Build WritableStream
    let sink = AppendChildSink { parent };
    let writable = WritableStream::builder(sink).spawn(wasm_bindgen_futures::spawn_local);

    // Get a writer
    let (_, writer) = writable.get_writer().unwrap();

    // Write some chunks
    let chunks = vec![
        "Hello from Rust + WASM!".to_string(),
        "Each of these is a stream chunk.".to_string(),
        "Appended as <div> children.".to_string(),
    ];

    for chunk in chunks {
        writer.write(chunk).await.unwrap();
    }

    writer.close().await.unwrap();
}

impl WritableSink<String> for AppendChildSink {
    fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> impl std::future::Future<Output = whatwg_streams::local::StreamResult<()>> {
        async move {
            let document = web_sys::window().unwrap().document().unwrap();
            let div = document.create_element("div").unwrap();
            div.set_inner_html(&chunk);
            self.parent.append_child(&div).unwrap();
            Ok(())
        }
    }
}
