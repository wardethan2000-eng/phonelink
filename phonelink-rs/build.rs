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
    builtin_google_client();
}

/// PhoneRelay's own Google OAuth client, so users only click Connect. Read
/// from `data/google_oauth_client.json` (the Desktop client JSON from Google
/// Cloud, gitignored: it stays out of the public repo) and baked in as
/// `PHONELINK_BUILTIN_GOOGLE_CLIENT_ID` / `_SECRET`. Google treats a desktop
/// client's secret as not confidential, but keeping it out of git avoids
/// secret scanners. Without the file the app falls back to the user's own
/// `~/.config/phonelink/google_oauth.json`.
fn builtin_google_client() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/google_oauth_client.json");
    println!("cargo:rerun-if-changed={}", path.display());
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let field = |key: &str| -> Option<String> {
        // Tiny scan for "key": "value" (no JSON crate in build-dependencies).
        let at = text.find(&format!("\"{key}\""))?;
        let rest = &text[at + key.len() + 2..];
        let start = rest.find('"')? + 1;
        let end = rest[start..].find('"')? + start;
        Some(rest[start..end].to_string())
    };
    if let (Some(id), Some(secret)) = (field("client_id"), field("client_secret")) {
        println!("cargo:rustc-env=PHONELINK_BUILTIN_GOOGLE_CLIENT_ID={id}");
        println!("cargo:rustc-env=PHONELINK_BUILTIN_GOOGLE_CLIENT_SECRET={secret}");
    } else {
        println!("cargo:warning=data/google_oauth_client.json has no client_id/client_secret; no built-in Google client");
    }
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
