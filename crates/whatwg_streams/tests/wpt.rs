/// Integration test suite mirroring the WHATWG Streams Web Platform Tests (WPT).
///
/// # WPT reference
///
/// Browse the canonical test suite at:
/// <https://github.com/web-platform-tests/wpt/tree/master/streams>
///
/// The live runner (requires a browser) is at:
/// <https://wpt.live/streams/>
///
/// # Module layout
///
/// Each sub-module maps 1-to-1 with a WPT directory or file:
///
/// | Module              | WPT path                                          |
/// |---------------------|---------------------------------------------------|
/// | `readable/`         | `streams/readable-streams/`                       |
/// | `writable/`         | `streams/writable-streams/`                       |
/// | `byte_stream/`      | `streams/readable-streams/byte-source.any.js`     |
/// | `piping/`           | `streams/piping/`                                 |
/// | `transform/`        | `streams/transform-streams/`                      |
/// | `queuing_strategies/` | `streams/queuing-strategies.any.js`             |
///
/// Shared sink/source helpers live in [`helpers`].
pub mod helpers;

mod readable;
mod writable;
mod byte_stream;
mod piping;
mod transform;
mod queuing_strategies;
