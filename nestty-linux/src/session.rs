// Session persistence lives in nestty-core so the macOS FFI surface can
// share the wire format and load/save semantics. This file re-exports the
// types and free functions so existing `crate::session::*` call sites in
// `window.rs` / `tabs.rs` keep compiling unchanged.
pub use nestty_core::session::*;
