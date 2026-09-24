//! The Rust core behind the OpenAGC app. This is the only crate that knows
//! about UniFFI; everything Swift can see is exported from here.

uniffi::setup_scaffolding!();
