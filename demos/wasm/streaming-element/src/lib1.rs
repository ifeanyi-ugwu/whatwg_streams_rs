use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CustomElementRegistry, Document, Element, HtmlElement};
use whatwg_streams::local::{WritableSink, WritableStream, WritableStreamDefaultController};

// Our custom sink: appends chunks as <div> children
struct StreamingElementSink {
    parent: HtmlElement,
}

impl WritableSink<String> for StreamingElementSink {
    fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> impl std::future::Future<Output = whatwg_streams::local::StreamResult<()>> {
        async move {
            let document: Document = web_sys::window().unwrap().document().unwrap();
            let div = document.create_element("div").unwrap();
            div.set_inner_html(&chunk);
            self.parent.append_child(&div).unwrap();
            Ok(())
        }
    }
}

#[wasm_bindgen(start)]
pub fn register_streaming_element() {
    let document = web_sys::window().unwrap().document().unwrap();
    let registry: CustomElementRegistry = document.default_view().unwrap().custom_elements();

    // Define <streaming-element>
    let constructor = Closure::wrap(Box::new(move || {
        // Create the shadow DOM root
        let document = web_sys::window().unwrap().document().unwrap();
        let el: HtmlElement = document.create_element("div").unwrap().dyn_into().unwrap();

        // Create a writable stream attached to this element
        let sink = StreamingElementSink { parent: el.clone() };
        let writable = WritableStream::builder(sink).spawn(wasm_bindgen_futures::spawn_local);

        js_sys::Reflect::set(
            &el,
            &JsValue::from_str("writable"),
            &JsValue::from(writable),
        )
        .unwrap();

        el.into()
    }) as Box<dyn FnMut() -> JsValue>);

    registry
        .define("streaming-element", constructor.as_ref().unchecked_ref())
        .unwrap();
    constructor.forget();
}
