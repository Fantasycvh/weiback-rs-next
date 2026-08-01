#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release
use anyhow::Result;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

fn main() -> Result<()> {
    init_logger()?;

    info!("start running...");
    tauri_app::run()?;

    info!("done");
    Ok(())
}

fn init_logger() -> Result<()> {
    let runtime = weiback::config::runtime_dirs();
    runtime.ensure_created()?;
    let log_path = runtime.logs_dir.join("weiback-next.log");
    let log_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(log_path)?;

    let filter = EnvFilter::builder()
        .with_default_directive(if cfg!(debug_assertions) {
            tracing::Level::DEBUG.into()
        } else {
            tracing::Level::INFO.into()
        })
        .from_env_lossy()
        .add_directive("sqlx=warn".parse()?)
        .add_directive("h2=warn".parse()?)
        .add_directive("hyper_util=warn".parse()?)
        .add_directive("reqwest=warn".parse()?)
        .add_directive("weibosdk_rs=warn".parse()?);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::sync::Mutex::new(log_file)))
        .init();
    Ok(())
}
