fn main() {
    prefer_local_roc_04();
    // libroc doesn't ship a pkg-config file in Debian/Ubuntu, so we probe it
    // manually and fall back to a plain `-lroc` link directive.
    if pkg_config::probe_library("roc").is_err() {
        println!("cargo:rustc-link-lib=roc");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/lib.rs");
}

/// Put `/usr/local/lib` ahead of the distribution's library directories when
/// a source-built libroc 0.4 lives there.
///
/// On Ubuntu 24.04 and Debian bookworm, `apt install libroc-dev` provides
/// 0.3, and `packaging/install.sh` then builds 0.4 into /usr/local because
/// 0.3 is the wrong ABI. Both are now present, and `-lroc` resolves against
/// the multiarch directory first — so the build silently linked the 0.3 the
/// installer had just gone to the trouble of replacing. The result was a
/// binary that started fine and failed every route with a complaint about a
/// config field that does not exist in this version.
///
/// Only acts when the local header actually says 0.4, so a machine with a
/// correct distro package is left alone.
fn prefer_local_roc_04() {
    let header = std::path::Path::new("/usr/local/include/roc/version.h");
    let Ok(text) = std::fs::read_to_string(header) else { return };
    let field = |name: &str| -> Option<u32> {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("#define {name}")))
            .and_then(|rest| rest.trim().parse().ok())
    };
    if field("ROC_VERSION_MAJOR") == Some(0) && field("ROC_VERSION_MINOR") == Some(4) {
        println!("cargo:rustc-link-search=native=/usr/local/lib");
        println!("cargo:rerun-if-changed=/usr/local/include/roc/version.h");
    }
}
