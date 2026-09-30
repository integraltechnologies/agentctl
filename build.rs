//! Locates the installed procd that agentctl links statically: its public
//! header `procd.h` and its static library (`libprocd.a`; `procd.lib` for
//! MSVC). procd is an external dependency, consumed only as installed:
//! nothing here builds, fetches or patches it.
//!
//! By default both are found where the target's C toolchain finds them by
//! itself: the header through the C compiler's include search, and the
//! library through a trial link with the same compiler driver (for MSVC,
//! the `LIB` search path). `PROCD_INCLUDE_DIR` and `PROCD_LIB_DIR`, set
//! together, name the directories instead. Either may be scoped to one
//! target, as in `PROCD_LIB_DIR_x86_64_unknown_linux_gnu`. The compiler
//! also honors the usual `CC`, `CFLAGS` and `CFLAGS_<target>`.
//!
//! procd.h has no version discriminator, so which install is used is left
//! to the consumer; what this build compiled against and linked is printed
//! (`cargo build -vv`) and recorded for `procd::tests`.
//!
//! `src/procd_layout.c` is compiled against that same header. It checks the
//! function signatures `src/procd.rs` declares and reports the header's
//! layouts and constants to `procd::tests` to compare with the Rust mirror.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/procd_layout.c");
    // Search paths the toolchain itself honors.
    for name in ["CPATH", "C_INCLUDE_PATH", "LIBRARY_PATH", "INCLUDE", "LIB"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let target = env::var("TARGET").unwrap();
    let include_dir = configured("PROCD_INCLUDE_DIR", &target);
    let lib_dir = configured("PROCD_LIB_DIR", &target);
    assert_eq!(
        include_dir.is_some(),
        lib_dir.is_some(),
        "set PROCD_INCLUDE_DIR and PROCD_LIB_DIR together, to one procd install: \
         the header must be the one matching the library"
    );

    let mut build = cc::Build::new();
    build
        .file("src/procd_layout.c")
        .std("c11")
        .warnings_into_errors(true);
    if let Some(dir) = &include_dir {
        build.include(dir);
    }
    let compiler = build.get_compiler();
    let name = if compiler.is_like_msvc() {
        "procd.lib"
    } else {
        "libprocd.a"
    };

    let header = header(&build);
    if let Some(dir) = &include_dir {
        assert!(
            same_file(&header, &dir.join("procd.h")),
            "PROCD_INCLUDE_DIR is {}, but the compiler found procd.h at {}",
            dir.display(),
            header.display()
        );
    }
    let library = match &lib_dir {
        Some(dir) => dir.join(name),
        None if compiler.is_like_msvc() => lib_search(&compiler, name),
        None => trial_link(&compiler, name),
    };
    assert!(
        library.is_file(),
        "procd's static library is not at {}",
        library.display()
    );

    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rerun-if-changed={}", library.display());
    println!("procd header: {}", header.display());
    println!("procd library: {}", library.display());
    println!("cargo:rustc-env=AGENTCTL_PROCD_HEADER={}", header.display());
    println!(
        "cargo:rustc-env=AGENTCTL_PROCD_LIBRARY={}",
        library.display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        library.parent().unwrap().display()
    );
    println!("cargo:rustc-link-lib=static=procd");

    build.compile("procd_layout");
}

/// `name`, scoped to `target` or not; scoped wins.
fn configured(name: &str, target: &str) -> Option<PathBuf> {
    let scoped = format!("{name}_{}", target.replace(['-', '.'], "_"));
    println!("cargo:rerun-if-env-changed={scoped}");
    println!("cargo:rerun-if-env-changed={name}");
    env::var_os(&scoped)
        .or_else(|| env::var_os(name))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The procd.h the compiler includes in `src/procd_layout.c`, read from its
/// preprocessor output's line markers.
fn header(build: &cc::Build) -> PathBuf {
    let expanded = build.try_expand().unwrap_or_else(|e| {
        panic!(
            "cannot compile against procd.h ({e}). Install procd's header and static \
             library where this target's C toolchain finds them, or set \
             PROCD_INCLUDE_DIR and PROCD_LIB_DIR"
        )
    });
    String::from_utf8_lossy(&expanded)
        .lines()
        .filter(|line| line.starts_with('#'))
        .filter_map(|line| line.split('"').nth(1))
        // MSVC escapes the backslashes in `#line` paths.
        .map(|path| PathBuf::from(path.replace("\\\\", "\\")))
        .find(|path| path.file_name().is_some_and(|file| file == "procd.h"))
        .expect("the preprocessor reported no procd.h")
}

/// Where the target's linker finds `name` by itself: a trial link through
/// the compiler driver, traced (`-t`, which GNU ld, lld and ld64 all take).
fn trial_link(compiler: &cc::Tool, name: &str) -> PathBuf {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let probe = out.join("procd_probe.c");
    fs::write(
        &probe,
        "#include <procd.h>\nint main(void) { return procd_status_name(PROCD_OK) == 0; }\n",
    )
    .unwrap();
    let mut link = compiler.to_command();
    link.arg(&probe)
        .arg("-o")
        .arg(out.join("procd_probe"))
        .arg("-lprocd")
        .arg("-Wl,-t");
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os != "macos" && os != "windows") {
        link.arg("-pthread");
    }
    let output = link
        .output()
        .unwrap_or_else(|e| panic!("cannot run the C compiler to find procd: {e}"));
    let text = [output.stdout, output.stderr].concat();
    let text = String::from_utf8_lossy(&text);
    assert!(
        output.status.success(),
        "the target's linker does not find procd's static library ({name}). Install \
         procd's header and static library where this target's C toolchain finds \
         them, or set PROCD_INCLUDE_DIR and PROCD_LIB_DIR.\n{link:?}\n{text}"
    );
    // `<path>(<member>)`, or GNU ld's older `-lprocd (<path>)`.
    text.lines()
        .map(|line| line.trim().trim_start_matches("-lprocd ("))
        .filter_map(|line| line.find(name).map(|end| &line[..end + name.len()]))
        .map(PathBuf::from)
        .find(|path| path.file_name().is_some_and(|file| file == name) && path.is_file())
        .unwrap_or_else(|| panic!("the linker's trace names no {name}:\n{text}"))
}

/// Where MSVC's linker finds `name` by itself: the first directory of the
/// `LIB` search path holding it.
fn lib_search(compiler: &cc::Tool, name: &str) -> PathBuf {
    let lib = compiler
        .env()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("LIB"))
        .map(|(_, value)| value.clone())
        .or_else(|| env::var_os("LIB"))
        .unwrap_or_default();
    env::split_paths(&lib)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "procd's static library ({name}) is not on the LIB search path. Install \
                 procd's header and static library where this target's C toolchain \
                 finds them, or set PROCD_INCLUDE_DIR and PROCD_LIB_DIR"
            )
        })
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
