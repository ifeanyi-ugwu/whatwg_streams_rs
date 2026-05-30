/// Integration test suite mirroring the WHATWG Streams Web Platform Tests (WPT).
///
/// Each sub-module maps 1-to-1 with a WPT directory:
///   readable/  → streams/readable-streams/
///   writable/  → streams/writable-streams/
///   byte_stream/ → streams/readable-streams/byte-source.any.js
///   piping/    → streams/piping/
///   transform/ → streams/transform-streams/
///
/// Shared sink/source helpers live in `helpers` and are imported by each module.
pub mod helpers;

mod readable;
mod writable;
mod byte_stream;
mod piping;
mod transform;
