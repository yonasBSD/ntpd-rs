#![forbid(unsafe_code)]

use clap::Parser;
use ntp_daemon::config::{CmdArgs, Config};
use std::error::Error;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = CmdArgs::parse();
    let has_log_override = args.log_filter.is_some();
    let has_format_override = args.log_format.is_some();
    let log_filter = args.log_filter.unwrap_or_else(|| EnvFilter::new("info"));

    // Setup some basic tracing now so we are able
    // to log errors when loading the full configuration.
    let finish_tracing_init =
        ntp_daemon::tracing::init(log_filter, args.log_format.unwrap_or_default());

    let mut config = Config::from_args(args.config, args.peers).await?;

    // Sentry has a guard we need to keep alive, so store it.
    // The compiler will optimize this away when not using sentry.
    let tracing_state = finish_tracing_init(&mut config, has_log_override, has_format_override)?;

    // Warn/error if the config is unreasonable. We do this after finishing
    // tracing setup to ensure logging is fully configured.
    config.check();

    let (main_loop_handle, channels) = ntp_daemon::spawn(config.system, &config.peers).await?;

    ntp_daemon::observer::spawn(&config.observe, channels.peers, channels.system).await;

    ntp_daemon::config::dynamic::spawn(
        config.configure,
        channels.config,
        tracing_state.reload_handle,
    )
    .await;

    Ok(main_loop_handle.await??)
}
