//! Standalone CLI for importing `.nsys-rep` files. Useful for scripted
//! imports from SLURM epilog jobs. Also exercised by the REST upload path.

use anyhow::Result;
use clap::Parser;
use perfweave_api::{ch::{Ch, ChConfig}, imports::import_nsys};

#[derive(Parser, Debug)]
#[command(name = "perfweave-nsys-import")]
struct Args {
    /// Path to a .nsys-rep file
    path: std::path::PathBuf,

    /// node_id to attribute imported events to
    #[arg(long, default_value_t = 0)]
    node_id: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    perfweave_common::logging::init("nsys-import");
    let args = Args::parse();
    let ch = Ch::new(ChConfig::default());
    let r = import_nsys(&ch, &args.path, args.node_id).await?;
    println!(
        "imported {} events (ts_min={} ts_max={})",
        r.event_count, r.ts_min_ns, r.ts_max_ns
    );
    Ok(())
}
