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
    println!("cargo:rerun-if-changed=src/markdown.rs");
    println!("cargo:rerun-if-changed=src/view_event.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    // Give the cdylib a stable install identity so a consumer that links it by
    // path records a build-tree-independent name and resolves the co-installed
    // copy via its own RPATH. The linker option is platform-specific:
    //
    // - ELF (Linux/BSD): a SONAME. Without it, a C/C++ consumer (e.g. adele-kde's
    //   `libadelecore.so` QML plugin) records the absolute build-tree path as its
    //   `DT_NEEDED`, so the installed plugin only resolves the core while this
    //   build tree exists. With a SONAME it records the bare name and its
    //   `$ORIGIN` RPATH resolves the co-installed copy.
    // - Mach-O (macOS/iOS): the analog is the dylib `install_name`. Apple's `ld`
    //   rejects `-soname` outright, so an `@rpath`-relative install_name is both
    //   what makes the link succeed and what lets a Swift/ObjC consumer resolve
    //   the core from an `@loader_path`/`@executable_path` RPATH at runtime.
    //
    // `rustc-cdylib-link-arg` applies to the cdylib link only (ignored for the rlib).
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "macos" | "ios" => println!(
            "cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libadele_client_core.dylib"
        ),
        _ => println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libadele_client_core.so"),
    }

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
        // A plain syntax error in this crate's own source. rustc parses the
        // same source right after this build script exits and reports it
        // with the file, line, and a caret — a strictly better diagnostic
        // than cbindgen's syn error. Warn and let the build carry on to that
        // rustc failure rather than panicking here and hiding it.
        Err(cbindgen::Error::ParseSyntaxError { .. }) => {
            println!(
                "cargo:warning=cbindgen could not parse this crate; see the rustc error below"
            );
        }
        // Every other cbindgen::Error variant (a `cargo metadata`/`Cargo.toml`
        // problem, a file cbindgen expected but could not open, cbindgen's own
        // `cargo rustc -Zunpretty=expanded` failing) is not something rustc
        // would ever report on its own. This repository has no CI, so a
        // silently stale header defeats the ABI version constant it is meant
        // to carry (#86); fail the build rather than only printing a warning.
        Err(e) => panic!("cbindgen header generation failed: {e}"),
    }
}
