mod config;
#[allow(dead_code)]
mod extract;
#[allow(dead_code)]
mod macos;
#[allow(dead_code)]
mod providers;
#[allow(dead_code)]
mod runtime;
#[allow(dead_code)]
mod window;

use std::process::ExitCode;

use argh::FromArgs;

use crate::config::Config;
use crate::runtime::Pipeline;
use crate::runtime::ship::endpoint;

/// nikki captures what happens on this Mac and ships it to the nikki service.
#[derive(FromArgs)]
struct Args {
    /// load and validate the configuration, then exit
    #[argh(switch)]
    check_config: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_target(false).init();

    let Args { check_config } = argh::from_env();

    let config = match config::load() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "configuration is not usable");
            return ExitCode::FAILURE;
        }
    };

    let Config {
        service_url,
        device,
        tick_interval,
        history_poll_interval,
        browser,
        state_dir,
        ..
    } = &config;
    tracing::info!(
        %device,
        %service_url,
        tick_interval,
        history_poll_interval,
        profile = %browser.profile,
        state_dir = %state_dir.display(),
        "configuration loaded"
    );

    if check_config {
        return ExitCode::SUCCESS;
    }

    let pipeline = match Pipeline::open(&config) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            tracing::error!(%error, "the runtime could not start");
            return ExitCode::FAILURE;
        }
    };
    match endpoint(&config.service_url) {
        Ok(endpoint) => tracing::info!(%endpoint, "records will be shipped here"),
        Err(error) => tracing::error!(%error, "the records endpoint is not usable"),
    }

    tracing::info!("no providers are registered yet");
    if let Err(error) = pipeline.close().await {
        tracing::error!(%error, "the runtime did not shut down cleanly");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
