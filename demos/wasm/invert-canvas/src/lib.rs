use js_sys::Uint8ClampedArray;
use wasm_bindgen::prelude::*;
use wasm_bindgen::Clamped;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};
use whatwg_streams::local::{ReadableStream, TransformStream, Transformer};

// Transformer: invert pixel colors
struct InvertColors;

impl Transformer<Vec<u8>, Vec<u8>> for InvertColors {
    async fn transform(
        &mut self,
        mut chunk: Vec<u8>,
        controller: &mut whatwg_streams::local::TransformStreamDefaultController<Vec<u8>>,
    ) -> Result<(), whatwg_streams::local::error::StreamError> {
        for px in chunk.chunks_mut(4) {
            px[0] = 255 - px[0]; // R
            px[1] = 255 - px[1]; // G
            px[2] = 255 - px[2]; // B
        }
        controller.enqueue(chunk);
        Ok(())
    }
}

#[wasm_bindgen]
pub async fn run_invert_demo(canvas_id: &str) {
    // Get canvas + 2d context
    let canvas: HtmlCanvasElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(canvas_id)
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();

    let ctx: CanvasRenderingContext2d = canvas
        .get_context("2d")
        .unwrap()
        .unwrap()
        .dyn_into::<CanvasRenderingContext2d>()
        .unwrap();

    // Grab pixel data
    let image_data = ctx
        .get_image_data(0.0, 0.0, canvas.width().into(), canvas.height().into())
        .unwrap();
    let pixels: Vec<u8> = Uint8ClampedArray::new(&image_data.data()).to_vec();

    // Source stream (pixel buffer)
    let source = ReadableStream::from_vec(vec![pixels]).spawn(wasm_bindgen_futures::spawn_local);

    // Transform stream (invert colors)
    let transform = TransformStream::builder(InvertColors).spawn(wasm_bindgen_futures::spawn_local);

    // Pipe through transform
    let output = source
        .pipe_through(transform, None)
        .spawn(wasm_bindgen_futures::spawn_local);

    let (_, mut reader) = output.get_reader().unwrap();

    // Render inverted image
    while let Ok(Some(chunk)) = reader.read().await {
        let clamped = Clamped(&chunk[..]);
        let new_image_data =
            ImageData::new_with_u8_clamped_array_and_sh(clamped, canvas.width(), canvas.height())
                .unwrap();
        ctx.put_image_data(&new_image_data, 0.0, 0.0).unwrap();
    }
}
