use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "codex-stdio-to-uds",
    about = "Relay standard input and output to a Unix domain socket."
)]
struct Args {
    /// Path to the Unix domain socket to connect to.
    #[arg(value_name = "SOCKET_PATH")]
    socket_path: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    codex_stdio_to_uds::run(&args.socket_path)
}
