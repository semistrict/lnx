mod checkpoints;
mod cli;
mod descriptor;
mod gvproxy_embedded;
mod host_share;
mod ingress;
mod init;
mod initramfs;
mod krun;
mod oci;
mod packages;
mod paths;
mod runner;
mod server;
mod sparse_copy;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    cli.run()
}
