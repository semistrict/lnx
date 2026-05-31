mod cli;
mod init;
mod initramfs;
mod krun;
mod paths;
mod runner;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    cli.run()
}
