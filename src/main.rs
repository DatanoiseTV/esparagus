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

    let cli = esparagus::cli::Cli::parse();
    let code = esparagus::runner::run(cli);
    std::process::exit(code);
}
