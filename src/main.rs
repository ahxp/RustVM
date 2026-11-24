mod catalog;
mod cli;
mod config;
mod disk;
mod download;
mod gui;
mod vm;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let parsed = cli::Cli::parse();
    cli::run(parsed)
}
