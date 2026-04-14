//! Build script — runs bindgen over `wrapper.h` to produce constants and struct
//! layouts, then emits the linker directive to link against `liblustreapi`.
//!
//! No fallback: if bindgen fails the build fails with a clear error. robinhood-rs
//! only builds on hosts where the `lustre-client` package is installed, so this
//! simplification is safe.

use std::env;
use std::path::PathBuf;

fn main() {
    // Link against the system liblustreapi at runtime. This must match the
    // `links = "lustreapi"` directive in Cargo.toml.
    println!("cargo:rustc-link-lib=lustreapi");

    // Rebuild when wrapper.h changes. We can't easily track upstream Lustre
    // headers from the system prefix, so `cargo clean` is the recourse after a
    // lustre-client package upgrade.
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let bindings_path = out_path.join("bindings.rs");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // Layout tests often break on packed Lustre structs across bindgen
        // versions. Our hand-written extern "C" declarations and struct uses
        // carry their own alignment expectations via `#[repr(C, packed)]`.
        .layout_tests(false)
        // Types with flexible-array members can't implement Debug automatically.
        // liblustreapi has a handful — skip them so bindgen doesn't bail.
        .no_debug("hsm_user_request")
        .no_debug("hsm_user_state")
        .no_debug("hsm_action_item")
        .no_debug("hsm_copy")
        // Block all function AND extern-static generation. bindgen 0.70 does NOT
        // emit `unsafe` on `extern "C"` blocks, which Rust 2024 requires.
        // Functions are hand-written in `sys.rs`; the extern statics bindgen
        // emits for C lookup tables (comp_flags_table, hsm_flags_table, etc.)
        // are unused by us.
        .blocklist_function(".*")
        .blocklist_var(".*")
        .generate_inline_functions(false)
        .generate()
        .expect(
            "bindgen failed: ensure the `lustre-client` package is installed with headers \
             (expected at /usr/include/lustre/ and /usr/include/linux/lustre/)",
        );

    bindings
        .write_to_file(&bindings_path)
        .expect("failed to write bindings.rs to OUT_DIR");
}
