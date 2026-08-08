fn main() {
    // libroc doesn't ship a pkg-config file in Debian/Ubuntu, so we probe it
    // manually and fall back to a plain `-lroc` link directive.
    if pkg_config::probe_library("roc").is_err() {
        println!("cargo:rustc-link-lib=roc");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/lib.rs");
}
