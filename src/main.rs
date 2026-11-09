mod change;
mod document;
mod images;
mod nav;
mod page;
mod pager;
mod protocol;
mod render;
mod terminal;

use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::render::ThemeChoice;

#[derive(Parser, Debug)]
#[command(name = "md", version, about = "Preview Markdown with kitty graphics")]
struct Cli {
    /// Markdown file to preview. Reads stdin when it is a pipe.
    file: Option<PathBuf>,

    /// Color theme. Auto follows the terminal background.
    #[arg(long, value_enum, default_value = "auto")]
    theme: ThemeChoice,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let (path, source) = match cli.file {
        Some(path) => {
            let source =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            (Some(path), source)
        }
        None => {
            if io::stdin().is_terminal() {
                bail!("pass a markdown file, or pipe markdown on stdin");
            }
            let mut source = String::new();
            io::stdin()
                .read_to_string(&mut source)
                .context("read markdown from stdin")?;
            (None, source)
        }
    };
    pager::run(path, source, cli.theme)
}
