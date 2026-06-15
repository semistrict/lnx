mod checkpoints;
mod cli;
mod descriptor;
mod ingress;
mod init;
mod initramfs;
mod krun;
mod oci;
mod paths;
mod runner;
mod server;
mod sparse_copy;
#[cfg(target_os = "macos")]
mod vmnet;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    cli.run()
}
