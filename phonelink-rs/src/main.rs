mod dump;

use std::process::ExitCode;

const USAGE: &str = "usage: phonelink-rs                 the app
       phonelink-rs --version | -V
       phonelink-rs --dump [--dump-signatures]
  --dump             read-only check against the running KDE Connect daemon:
                     devices, battery, conversation and message counts
  --dump-signatures  also the daemon's D-Bus surface and the type signature
                     of each reply and signal (implies --dump)";

fn main() -> ExitCode {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("PhoneRelay {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let signatures = args.iter().any(|a| a == "--dump-signatures");
    let dump = signatures || args.iter().any(|a| a == "--dump");
    if args.is_empty() {
        return match phonelink_rs::ui::run() {
            glib::ExitCode::SUCCESS => ExitCode::SUCCESS,
            _ => ExitCode::FAILURE,
        };
    }
    if !dump || args.iter().any(|a| !a.starts_with("--dump")) {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let code = glib::MainContext::default().block_on(dump::run(signatures));
    ExitCode::from(code as u8)
}
