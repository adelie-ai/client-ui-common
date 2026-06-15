//! Generate the C header (`include/adele_client_core.h`) from the crate's
//! `extern "C"` surface via cbindgen, so the C++ consumer (adele-kde's CMake)
//! can `#include` a committed, stable path.
//!
//! cbindgen ≥ 0.29 is required: it is the first release that understands the
//! edition-2024 `#[unsafe(no_mangle)]` attribute this crate uses (0.27 silently
//! emitted an empty header).

use std::path::PathBuf;

fn main() {
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = crate_dir.join("include").join("adele_client_core.h");

    // Re-run when the ABI surface or the cbindgen config changes.
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=src/engine.rs");
    println!("cargo:rerun-if-changed=src/view_event.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    // Give the cdylib a SONAME. Without it, a C/C++ consumer that links the
    // produced `libadele_client_core.so` by path (e.g. adele-kde's
    // `libadelecore.so` QML plugin) records the absolute build-tree path as its
    // `DT_NEEDED`, so the installed plugin only resolves the core while this
    // build tree exists. With a SONAME, the consumer records the bare name and
    // its `$ORIGIN` RPATH resolves the co-installed copy — a self-contained,
    // build-tree-independent install. `rustc-cdylib-link-arg` applies to the
    // cdylib link only (ignored for the rlib).
    println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libadele_client_core.so");

    let config = cbindgen::Config::from_file(crate_dir.join("cbindgen.toml")).unwrap_or_default();

    match cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
    {
        Ok(bindings) => {
            if let Some(parent) = out.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            bindings.write_to_file(&out);
        }
        // Don't fail the cdylib build if header generation hiccups — surface it
        // as a warning so `cargo build` still produces the `.so`, and CI's
        // header-presence check (which runs cbindgen directly) is the gate.
        Err(e) => println!("cargo:warning=cbindgen header generation failed: {e}"),
    }
}
