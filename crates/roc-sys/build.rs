use std::path::{Path, PathBuf};

fn main() {
    let roc = RocInstall::find();
    roc.emit_link_directives();
    roc.generate_bindings();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=wrapper.h");
}

/// Which libroc this build is compiling against.
///
/// There is exactly one of these, and that is the point. The crate has two
/// entirely separate reasons to care where libroc lives — bindgen needs an
/// include path to parse the headers, and the linker needs a search path
/// ahead of the multiarch directories — and if those two ever disagreed the
/// result would be the worst possible failure mode: bindings describing one
/// version's structs, linked against another version's code. It would
/// compile, and then read fields at the wrong offsets at runtime. Deciding
/// once, here, is what makes that impossible rather than merely unlikely.
struct RocInstall {
    /// Passed to clang as `-I`. `<include_dir>/roc/version.h` exists.
    include_dir: PathBuf,
    /// Set only for a source-built copy that needs to win over a distro one.
    prefer_lib_dir: Option<PathBuf>,
    version: (u32, u32),
}

impl RocInstall {
    /// Prefer a source-built 0.4 in `/usr/local` over whatever the distro
    /// packages.
    ///
    /// On Ubuntu 24.04 and Debian bookworm/trixie, `apt install libroc-dev`
    /// provides 0.3, and `packaging/install.sh` then builds 0.4 into
    /// /usr/local because 0.3 is the wrong ABI. Both end up present, and
    /// `-lroc` resolves against the multiarch directory first — so the build
    /// silently linked the very 0.3 the installer had just gone to the
    /// trouble of replacing. The result was a binary that started fine and
    /// failed every route with a complaint about a config field that does not
    /// exist in this version.
    ///
    /// Only acts when the local header actually says 0.4, so a machine whose
    /// distro package is already correct is left alone.
    fn find() -> Self {
        let local = Path::new("/usr/local/include/roc/version.h");
        if let Some(version @ (0, 4)) = header_version(local) {
            return Self {
                include_dir: PathBuf::from("/usr/local/include"),
                prefer_lib_dir: Some(PathBuf::from("/usr/local/lib")),
                version,
            };
        }
        // Fall through to the system copy. No version gate here: if it turns
        // out to be 0.3, the build will fail on the fields this crate's
        // callers reference, and `check_runtime_version` catches the case
        // where headers and the loaded library disagree.
        let system = Path::new("/usr/include/roc/version.h");
        Self {
            include_dir: PathBuf::from("/usr/include"),
            prefer_lib_dir: None,
            version: header_version(system).unwrap_or((0, 0)),
        }
    }

    fn emit_link_directives(&self) {
        if let Some(dir) = &self.prefer_lib_dir {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
        // libroc ships no pkg-config file on Debian/Ubuntu, so probe and fall
        // back to a plain `-lroc`.
        if pkg_config::probe_library("roc").is_err() {
            println!("cargo:rustc-link-lib=roc");
        }
        println!(
            "cargo:rerun-if-changed={}/roc/version.h",
            self.include_dir.display()
        );
    }

    fn generate_bindings(&self) {
        println!(
            "cargo:warning=roc-sys: binding against libroc {}.{} headers in {}",
            self.version.0,
            self.version.1,
            self.include_dir.display()
        );

        let bindings = bindgen::Builder::default()
            .header("wrapper.h")
            .clang_arg(format!("-I{}", self.include_dir.display()))
            // Keep the output to roc's own surface. Without this the bindings
            // would also carry every libc type the headers pull in, which is
            // noise at best and a portability trap at worst.
            .allowlist_item("roc_.*")
            .allowlist_item("ROC_.*")
            // Rust enums by default, matching what the hand-written bindings
            // declared and what every call site already spells.
            //
            // A rustified enum is only sound where every value it will ever
            // hold is one of the declared variants — holding anything else is
            // an invalid discriminant, which is instant undefined behaviour,
            // not merely a surprising number. That is true of the enums this
            // crate only ever *writes* into roc's config structs, and the two
            // exceptions below are the ones where it is not.
            .default_enum_style(bindgen::EnumVariation::Rust {
                non_exhaustive: false,
            })
            // Exception 1: values that travel from roc back to us.
            // `roc_log_message.level` is written by libroc and read by our log
            // handler, and `roc_endpoint_get_protocol` writes a
            // `roc_protocol`. A libroc that grows a new log level or protocol
            // would otherwise hand us an invalid discriminant. As plain
            // constants there is no invalid bit pattern to construct. Call
            // sites are unaffected — a `const` is still a legal match pattern.
            .constified_enum_module("roc_log_level")
            .constified_enum_module("roc_protocol")
            // Exception 2: a field the C API itself puts out-of-enum values
            // in. config.h declares `roc_sender_config.packet_encoding` as
            // `roc_packet_encoding`, but documents — and this engine relies on
            // — assigning it an identifier returned by
            // `roc_context_register_encoding()`, which is by construction not
            // one of the two built-in variants. The hand-written binding
            // called this field a plain `c_int` for exactly that reason, and
            // it was right to: taking bindgen's literal translation here would
            // have been a *regression* into undefined behaviour on every
            // sender we open. Same width either way, so nothing about the
            // layout changes.
            .constified_enum_module("roc_packet_encoding")
            .derive_default(true)
            .derive_debug(true)
            // Struct layout is the entire reason this crate exists, so make
            // bindgen assert its own understanding of it at test time.
            .layout_tests(true)
            .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
            .generate()
            .expect("bindgen failed to parse the roc headers");

        let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
        bindings
            .write_to_file(out.join("bindings.rs"))
            .expect("could not write bindings.rs");
    }
}

/// `(major, minor)` from a roc `version.h`, or `None` if it isn't readable.
fn header_version(header: &Path) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(header).ok()?;
    let field = |name: &str| -> Option<u32> {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("#define {name}")))
            .and_then(|rest| rest.trim().parse().ok())
    };
    Some((field("ROC_VERSION_MAJOR")?, field("ROC_VERSION_MINOR")?))
}
