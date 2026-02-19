use clap::Parser;
use log::{error, info};

use dikt_app_lib::ibus_engine::{cleanup, create_context, init, run_main_loop};

#[derive(Debug, clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(long)]
    ibus: bool,

    #[clap(flatten)]
    verbose: clap_verbosity_flag::Verbosity,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    env_logger::Builder::new()
        .filter_level(args.verbose.log_level_filter())
        .init();

    info!("Starting Dikt IBus Engine");

    let context = create_context();

    if let Err(code) = init(&context, args.ibus) {
        error!("Failed to initialize IBus engine: error code {}", code);
        std::process::exit(code);
    }

    info!("Entering IBus main loop");
    run_main_loop();

    cleanup();
    Ok(())
}
