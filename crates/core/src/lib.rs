//! Toolchain-facing core of Cairn: the pipeline, shared by the desktop app and the command line.
//!
//! Deliberately free of any Tauri dependency: the GUI is a thin shell over this, and keeping the
//! split means the parser and toolchain probes stay testable in seconds instead of behind a
//! full webview build.

pub mod buildconfig;
pub mod cancel;
pub mod catalog;
pub mod contours;
pub mod elevation;
pub mod poly;
pub mod presets;
pub mod progress;
pub mod settings;
pub mod steps;
pub mod terrain;
pub mod tileserver;
pub mod toolchain;
pub mod valhalla;

pub use cancel::Cancel;
pub use progress::{parse_line, LogEvent};
