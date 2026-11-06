use std::path::PathBuf;
use std::process::Command;

/// The app id (`core::settings::APPLICATION_ID`) as a resource path.
const ICON_PREFIX: &str = "/io/github/wardethan2000_eng/phonelink/icons";
const ICON_NAME: &str = "io.github.wardethan2000_eng.phonelink";

fn main() {
    // Lazy binding, so a GTK/libadwaita older than the build's lacks a symbol
    // at call time rather than at load time, and `core::compat` can explain
    // the problem instead of the loader printing "undefined symbol".
    println!("cargo:rustc-link-arg-bins=-Wl,-z,lazy");
    compile_icons();
}

/// Bundle the app icon (`data/icons`) into the binary as a GResource in the
/// icon-theme layout, so a lone binary still has its window icon.
fn compile_icons() {
    let icons = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/icons");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut files = String::new();
    for size in [48, 64, 128, 256] {
        let name = format!("phonelink-{size}.png");
        println!("cargo:rerun-if-changed={}", icons.join(&name).display());
        files += &format!("    <file alias=\"{size}x{size}/apps/{ICON_NAME}.png\">{name}</file>\n");
    }
    let xml = format!("<?xml version=\"1.0\"?>\n<gresources>\n  <gresource prefix=\"{ICON_PREFIX}\">\n{files}  </gresource>\n</gresources>\n");
    let xml_path = out.join("icons.gresource.xml");
    std::fs::write(&xml_path, xml).unwrap();
    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(&icons)
        .arg("--target")
        .arg(out.join("icons.gresource"))
        .arg(&xml_path)
        .status()
        .expect("glib-compile-resources (glib2-devel) is needed to build");
    assert!(status.success(), "glib-compile-resources failed");
}
