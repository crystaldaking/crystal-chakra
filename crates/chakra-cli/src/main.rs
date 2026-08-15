//! `chakra` — user-facing entry point of the Chakra code intelligence service.
//!
//! v0.1 bootstrap: argument parsing and `--help`/`--version` only.
//! Indexing and MCP serving arrive in later phases.

use clap::Parser;

/// Local code intelligence layer for AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "chakra", version)]
struct Cli {
    // Future subcommands (serve, index, status) land in later v0.1 phases.
}

fn main() {
    let _cli = Cli::parse();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
