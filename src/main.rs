mod checkpoints;
mod cli;
mod descriptor;
mod fsutil;
mod gvproxy_embedded;
mod host_share;
mod ingress;
mod init;
mod initramfs;
mod krun;
mod oci;
mod paths;
mod release_assets;
mod runner;
mod server;
mod sparse_copy;
mod status;
mod store;

use clap::Parser;

/// The exit status of a failure in lnx itself. A guest command's own status
/// passes through unchanged, so this keeps "lnx failed" apart from "the
/// command failed" (docker run uses 125 the same way).
const EXIT_LNX_ERROR: i32 = 125;

fn main() {
    let cli = cli::Cli::parse();
    if let Err(error) = cli.run() {
        eprintln!("Error: {error:?}");
        std::process::exit(EXIT_LNX_ERROR);
    }
}
