use anyhow::Result;
use cargo_impact::{run, ImpactArgs};
use clap::Parser;

fn main() -> Result<()> {
    // When invoked as a cargo subcommand, cargo execs us with
    // argv = ["cargo-impact", "impact", ...rest]. Strip the duplicate
    // "impact" token so clap sees a clean arg list. When invoked directly
    // as `./cargo-impact ...`, there is no token to strip.
    let args: Vec<std::ffi::OsString> = std::env::args_os()
        .enumerate()
        .filter_map(|(i, a)| {
            if i == 1 && a == "impact" {
                None
            } else {
                Some(a)
            }
        })
        .collect();
    let parsed = ImpactArgs::parse_from(args);
    let code = run(&parsed)?;
    std::process::exit(code);
}
