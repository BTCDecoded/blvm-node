//! Convert Bitcoin Core bitcoin.conf to blvm-node config.toml
//!
//! Standalone binary. Also available as `blvm config convert-core`.

use blvm_node::config::{generate_toml_config, parse_bitcoin_conf};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();

    if !args.input.exists() {
        eprintln!("Error: Input file '{}' not found", args.input.display());
        eprintln!();
        eprintln!("Usage: convert-bitcoin-core-config <bitcoin.conf> [output.toml]");
        eprintln!("Converts Bitcoin Core bitcoin.conf to blvm-node config.toml");
        std::process::exit(1);
    }

    if args.verbose {
        eprintln!("Reading Bitcoin Core config from: {}", args.input.display());
    }

    let bitcoin_config = parse_bitcoin_conf(&args.input)?;

    if args.verbose {
        eprintln!("Generating blvm-node config...");
    }

    let toml_config = generate_toml_config(&bitcoin_config, &args.input);

    std::fs::write(&args.output, toml_config)?;

    println!("✓ Configuration converted successfully!");
    println!("  Input:  {}", args.input.display());
    println!("  Output: {}", args.output.display());
    println!();
    println!("⚠️  IMPORTANT:");
    println!("  - Data directories are NOT converted");
    println!("  - Review the generated config and adjust as needed");
    println!("  - Some options may need manual configuration");

    Ok(())
}

struct Args {
    input: PathBuf,
    output: PathBuf,
    verbose: bool,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let input = args.next().expect("Input file required").into();
    let output = args
        .next()
        .unwrap_or_else(|| "config.toml".to_string())
        .into();
    let verbose = args.any(|a| a == "-v" || a == "--verbose");
    Args {
        input,
        output,
        verbose,
    }
}
