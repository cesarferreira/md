use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "md",
    version,
    about = "Preview Markdown with kitty graphics"
)]
struct Cli {
    /// Markdown file to preview. Reads stdin when omitted.
    file: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.file {
        Some(path) => preview_file(&path),
        None => preview_stdin(),
    }
}

fn preview_file(path: &std::path::Path) -> Result<()> {
    todo!("render {} with the kitty graphics protocol", path.display())
}

fn preview_stdin() -> Result<()> {
    todo!("render stdin with the kitty graphics protocol")
}
