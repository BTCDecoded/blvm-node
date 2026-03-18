//! Convert Bitcoin Core config to blvm-node format.
//!
//! Used by `blvm config convert-core`.

use anyhow::{Context, Result};
use std::path::Path;

use crate::config::{generate_toml_config, parse_bitcoin_conf};

/// Run config convert-core: convert bitcoin.conf to config.toml.
pub fn run_config_convert_core(
    input: &Path,
    output: &Path,
    verbose: bool,
) -> Result<()> {
    if !input.exists() {
        anyhow::bail!("Input file '{}' not found", input.display());
    }

    if verbose {
        eprintln!("Reading Bitcoin Core config from: {}", input.display());
    }

    let bitcoin_config = parse_bitcoin_conf(input)
        .with_context(|| format!("Failed to parse {}", input.display()))?;

    if verbose {
        eprintln!("Generating blvm-node config...");
    }

    let toml_config = generate_toml_config(&bitcoin_config, input);
    std::fs::write(output, toml_config)
        .with_context(|| format!("Failed to write {}", output.display()))?;

    println!("✓ Configuration converted successfully!");
    println!("  Input:  {}", input.display());
    println!("  Output: {}", output.display());
    println!();
    println!("⚠️  IMPORTANT:");
    println!("  - Data directories are NOT converted");
    println!("  - Review the generated config and adjust as needed");
    println!("  - Some options may need manual configuration");

    Ok(())
}
