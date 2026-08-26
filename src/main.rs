mod config;
#[allow(dead_code)]
mod macos;

use std::process::ExitCode;

use argh::FromArgs;

use crate::config::Config;

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

    tracing::info!("no providers are registered yet");
    ExitCode::SUCCESS
}
