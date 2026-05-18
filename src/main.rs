use clap::Parser;

fn main() {
    // Initialise tracing for the protocol layer's TX/RX diagnostics. Filter
    // is driven by RUST_LOG; default off unless `--trace` raises the level.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    // Busybox-style multi-call: if we were invoked as `esptool` (or
    // `esptool.py` / `esptool.exe`, typically via a symlink), translate
    // the argv to esparagus's shape so scripts written against upstream
    // esptool — especially `idf.py flash` — keep working unmodified.
    let raw: Vec<String> = std::env::args().collect();
    let argv0 = raw.first().cloned().unwrap_or_default();
    let args = if esparagus::esptool_compat::is_esptool_invocation(&argv0) {
        match esparagus::esptool_compat::translate_argv(raw) {
            Ok(v) => v,
            Err(msg) => {
                eprintln!("error: {}", msg);
                std::process::exit(2);
            }
        }
    } else {
        raw
    };

    let cli = esparagus::cli::Cli::parse_from(args);
    let code = esparagus::runner::run(cli);
    std::process::exit(code);
}
