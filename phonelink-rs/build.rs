fn main() {
    // Lazy binding, so a GTK/libadwaita older than the build's lacks a symbol
    // at call time rather than at load time, and `core::compat` can explain
    // the problem instead of the loader printing "undefined symbol".
    println!("cargo:rustc-link-arg-bins=-Wl,-z,lazy");
}
