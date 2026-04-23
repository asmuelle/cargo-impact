use anyhow::Result;
use cargo_impact::{mcp, run, ImpactArgs};
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

    // Dispatch the `mcp` subcommand before clap parses against ImpactArgs,
    // which doesn't know about subcommands. `cargo impact mcp` → MCP stdio
    // server; anything else → normal analysis path.
    if args.get(1).is_some_and(|a| a == "mcp") {
        return mcp::serve();
    }

    let parsed = ImpactArgs::parse_from(args);
    let code = run(&parsed)?;
    std::process::exit(code);
}
