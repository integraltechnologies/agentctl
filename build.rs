//! Builds procd, the lifecycle authority agentctl links statically, from
//! the upstream source pinned as the `third_party/procd` git submodule,
//! with procd's own CMake, into this build's `OUT_DIR`. The submodule is
//! the procd this checkout resolves to, as `Cargo.lock` is for crates:
//! nothing is fetched here and nothing outside `OUT_DIR` is written.
//!
//! It also compiles `src/procd_layout.c` against that same header, which
//! checks the function signatures `src/procd.rs` declares and reports the
//! header's layouts and constants to `procd::tests` to compare with the
//! Rust mirror.

use std::env;
use std::path::PathBuf;

fn main() {
    let source =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("third_party/procd");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/procd_layout.c");
    for input in ["CMakeLists.txt", "include", "src"] {
        println!("cargo:rerun-if-changed={}", source.join(input).display());
    }
    assert!(
        source.join("CMakeLists.txt").is_file(),
        "procd's source is missing from {}: check out agentctl's submodules \
         (`git submodule update --init`, or clone with `--recurse-submodules`)",
        source.display()
    );

    // Release whatever Cargo's profile: procd is a dependency, and on MSVC
    // only its Release configuration uses the CRT Rust links (/MD).
    let build = cmake::Config::new(&source)
        .profile("Release")
        .build_target("procd")
        .build()
        .join("build");
    let msvc = env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc");
    let library = if msvc { "procd.lib" } else { "libprocd.a" };
    // Multi-configuration generators (Visual Studio) build into a directory
    // per configuration.
    let lib_dir = [build.join("Release"), build.clone()]
        .into_iter()
        .find(|dir| dir.join(library).is_file())
        .unwrap_or_else(|| panic!("procd's build produced no {library} in {}", build.display()));
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=procd");

    cc::Build::new()
        .file("src/procd_layout.c")
        .include(source.join("include"))
        .std("c11")
        .warnings_into_errors(true)
        .compile("procd_layout");
}
