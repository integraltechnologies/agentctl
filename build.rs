//! Links the installed procd v0.1.0 static library, and checks that the
//! header the FFI mirrors is the one that library ships with: procd's v0 ABI
//! has no version discriminator, so a caller must build against exactly the
//! header matching the library it links.
//!
//! procd is an installed dependency, found under `PROCD_PREFIX` (default
//! `/usr/local` on Unix; required on Windows) as `include/procd.h` and
//! `lib/libprocd.a` (`lib/procd.lib` on Windows). Nothing else is searched.

use std::env;
use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// SHA-256 of the v0.1.0 `procd.h` that `src/procd.rs` mirrors.
const HEADER_SHA256: &str = "5dd4d132649881717129e9b5b5925dece0786630509a3cb78659d6c40eefdab9";

fn main() {
    println!("cargo:rerun-if-env-changed=PROCD_PREFIX");
    println!("cargo:rerun-if-changed=build.rs");
    let windows = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let prefix = match env::var_os("PROCD_PREFIX") {
        Some(prefix) => PathBuf::from(prefix),
        None if windows => panic!("set PROCD_PREFIX to where procd v0.1.0 is installed"),
        None => PathBuf::from("/usr/local"),
    };
    let header = prefix.join("include").join("procd.h");
    let lib_dir = prefix.join("lib");
    let library = lib_dir.join(if windows { "procd.lib" } else { "libprocd.a" });
    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rerun-if-changed={}", library.display());

    let bytes = fs::read(&header).unwrap_or_else(|e| {
        panic!(
            "procd is not installed: cannot read {} ({e}); install procd v0.1.0 or set PROCD_PREFIX",
            header.display()
        )
    });
    let digest = Sha256::digest(&bytes);
    let found: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        found,
        HEADER_SHA256,
        "{} is not the procd v0.1.0 header agentctl's bindings mirror (sha256 {found})",
        header.display()
    );
    assert!(
        library.is_file(),
        "procd's static library is missing: {}",
        library.display()
    );
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=procd");
}
