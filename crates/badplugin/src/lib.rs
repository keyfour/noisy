//! A deliberately wrong plugin: it exports the C ABI shape but declares
//! ABI version `2` — the loader must reject it with a readable message
//! (implementation plan §Phase 3 acceptance).
#![allow(clippy::missing_safety_doc, unused_variables)]

use std::ffi::{c_char, CString};
use std::sync::OnceLock;

fn static_cstr(value: &'static str) -> *const c_char {
    static C_STRING: OnceLock<CString> = OnceLock::new();
    C_STRING.get_or_init(|| CString::new(value).unwrap()).as_ptr()
}

const DESCRIPTOR: &str = r#"{"name":"ghost","version":"0.1.0","signals":["a"],"methods":[{"name":"b"}],"depends":[]}"#;

#[no_mangle]
pub extern "C" fn noisy_abi_version() -> *const c_char {
    // The engine wants 1; this plugin lies.
    static_cstr("2")
}

#[no_mangle]
pub extern "C" fn noisy_module_descriptor() -> *const c_char {
    static_cstr(DESCRIPTOR)
}
