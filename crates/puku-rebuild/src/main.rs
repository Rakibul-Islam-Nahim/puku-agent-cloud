//! `puku-rebuild` CLI: thin wrapper around the `puku_rebuild` library.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.6. Recovers the *environment*, not
//! data files.

use clap::Parser;
use uuid::Uuid;

use puku_rebuild::build_script;

#[derive(Debug, Parser)]
#[command(name = "puku-rebuild", about = "Emit env-rebuild shell script")]
struct Args {
    /// Session id whose packages to replay.
    session: Uuid,
    /// Output file; defaults to stdout.
    #[arg(short, long)]
    output: Option<std::path::PathBuf>,
}

fn main() {
    let args = Args::parse();
    eprintln!(
        "puku-rebuild: session {} -- pass --from-json <path> for a real script, \
         or use the `build_script` function from the library API",
        args.session
    );
    let script = build_script(args.session, Vec::new());
    match args.output {
        Some(path) => std::fs::write(&path, script).expect("write script"),
        None => print!("{}", script),
    }
}