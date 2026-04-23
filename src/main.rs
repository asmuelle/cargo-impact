use anyhow::Result;
use cargo_impact::{log_miss, mcp, run, ImpactArgs};
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

    // Subcommand dispatch — done manually before clap parses against
    // `ImpactArgs`, which isn't a subcommand-aware parser.
    match args.get(1).and_then(|a| a.to_str()) {
        Some("mcp") => return mcp::serve(),
        Some("log-miss") => {
            // Rebuild argv with the `log-miss` token removed so
            // `LogMissArgs::parse_from` sees a plain flag list.
            let mut sub: Vec<std::ffi::OsString> = Vec::with_capacity(args.len().saturating_sub(1));
            sub.push(args[0].clone());
            sub.extend(args.into_iter().skip(2));
            let parsed = log_miss::LogMissArgs::parse_from(sub);
            return log_miss::run(&parsed);
        }
        _ => {}
    }

    let parsed = ImpactArgs::parse_from(args);
    let code = run(&parsed)?;
    std::process::exit(code);
}
