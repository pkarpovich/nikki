mod config;
mod extract;
mod macos;
mod providers;
mod runtime;
mod service;
mod window;

use std::env::var_os;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use argh::FromArgs;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

use crate::config::{Config, Paths};
use crate::macos::ax::accessibility_is_trusted;
use crate::macos::events::EventThread;
use crate::providers::browser_history::{
    BrowserHistoryProvider, directory_for, discard_stale_snapshot, user_data_dir,
};
use crate::providers::windows::{MacSources, WindowProvider};
use crate::providers::{Backoff, Ctx, supervise};
use crate::runtime::ship::endpoint;
use crate::runtime::{EMISSION_QUEUE, Pipeline, absorb, private_dir};

const PROVIDERS: &str = "windows, browser_history";

/// nikki captures what happens on this Mac and ships it to the nikki service.
#[derive(FromArgs)]
struct Args {
    /// load and validate the configuration, then exit
    #[argh(switch)]
    check_config: bool,
    #[argh(subcommand)]
    command: Option<Service>,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Service {
    Install(Install),
    Uninstall(Uninstall),
}

/// copy this binary to a stable path and run it as a launchd agent, so macOS permissions survive an upgrade
#[derive(FromArgs)]
#[argh(subcommand, name = "install")]
struct Install {}

/// unload the launchd agent and remove what install wrote, leaving captured data alone
#[derive(FromArgs)]
#[argh(subcommand, name = "uninstall")]
struct Uninstall {}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_target(false).init();

    let Args {
        check_config,
        command,
    } = argh::from_env();

    if let Some(command) = command {
        return run_service(command);
    }

    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => {
            tracing::error!(%error, "configuration is not usable");
            return ExitCode::FAILURE;
        }
    };

    if !check_config {
        if let Err(source) = private_dir(&paths.state_dir) {
            tracing::error!(
                path = %paths.state_dir.display(),
                %source,
                "the state directory could not be made private"
            );
            return ExitCode::FAILURE;
        }
        discard_stale_snapshot(&paths.state_dir);
    }

    let config = match config::load_from(&paths) {
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

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            tracing::error!(%reason, "nikki stopped");
            ExitCode::FAILURE
        }
    }
}

fn run_service(command: Service) -> ExitCode {
    match command {
        Service::Install(Install {}) => match service::install() {
            Ok(installed) => {
                let service::Installed {
                    program,
                    agent,
                    housing,
                } = &installed;
                tracing::info!(
                    program = %program.display(),
                    agent = %agent.display(),
                    "nikki is installed and running"
                );
                match housing {
                    service::Housing::Bundle => tracing::info!(
                        path = %program.display(),
                        "grant Accessibility to this application"
                    ),
                    service::Housing::Loose => tracing::warn!(
                        path = %program.display(),
                        "this binary is not inside an application bundle, so macOS will ask for                          Accessibility again whenever its path changes - install the cask instead"
                    ),
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                tracing::error!(%error, "nikki could not be installed");
                ExitCode::FAILURE
            }
        },
        Service::Uninstall(Uninstall {}) => match service::uninstall() {
            Ok(layout) => {
                let service::Layout { agent, .. } = &layout;
                tracing::info!(agent = %agent.display(), "nikki is uninstalled");
                ExitCode::SUCCESS
            }
            Err(error) => {
                tracing::error!(%error, "nikki could not be uninstalled");
                ExitCode::FAILURE
            }
        },
    }
}

async fn run(config: Config) -> Result<(), String> {
    let Some(home) = var_os("HOME").map(PathBuf::from) else {
        return Err("HOME is not set, so the browser profile cannot be resolved".to_string());
    };

    let user_data = user_data_dir(&home);
    let directory = directory_for(&user_data, &config.browser.profile)?;

    let endpoint = match endpoint(&config.service_url) {
        Ok(endpoint) => endpoint,
        Err(error) => return Err(error.to_string()),
    };

    let mut pipeline = match Pipeline::open(&config) {
        Ok(pipeline) => pipeline,
        Err(error) => return Err(error.to_string()),
    };
    let records = pipeline.records();
    let ship_now = pipeline.shipper().ship_now();

    let (events, inbox) = mpsc::unbounded_channel();
    let event_thread = match EventThread::spawn(events) {
        Ok(event_thread) => event_thread,
        Err(source) => return Err(format!("the event thread could not start: {source}")),
    };

    tracing::info!(
        device = %config.device,
        service = %endpoint,
        accessibility = accessibility_is_trusted(),
        profile = %config.browser.profile,
        directory = %directory,
        providers = PROVIDERS,
        "nikki is running"
    );

    let ctx = Ctx {
        config: Arc::new(config),
    };
    let (emissions, drafts) = mpsc::channel(EMISSION_QUEUE);
    let (shutdown, listener) = watch::channel(false);

    let absorbing = tokio::spawn(absorb(records.clone(), ship_now, drafts, listener.clone()));
    let windows = tokio::spawn(supervise(
        WindowProvider::new(MacSources::new(event_thread.rescan_handle()), inbox),
        ctx.clone(),
        emissions.clone(),
        Backoff::default(),
    ));
    let history = tokio::spawn(supervise(
        BrowserHistoryProvider::new(user_data, records),
        ctx,
        emissions.clone(),
        Backoff::default(),
    ));
    drop(emissions);

    tokio::select! {
        _ = terminate() => tracing::info!("a termination signal arrived"),
        _ = pipeline.shipper().run(listener) => {
            tracing::error!("the shipper stopped before the daemon did");
        }
    }

    windows.abort();
    history.abort();
    let _ = windows.await;
    let _ = history.await;
    event_thread.stop();

    let _ = shutdown.send(true);
    if absorbing.await.is_err() {
        tracing::error!("the record writer did not finish cleanly");
    }

    match pipeline.shipper().ship_once().await {
        Ok(progress) => tracing::info!(?progress, "the final batch was flushed"),
        Err(error) => tracing::error!(%error, "the final batch could not be flushed"),
    }

    match pipeline.close().await {
        Ok(()) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

async fn terminate() {
    let (Ok(mut terminate), Ok(mut interrupt)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::error!("the termination signals cannot be observed");
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
}
