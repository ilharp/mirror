use std::fs::create_dir_all;
use std::future::ready;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;
use std::{fs, io};

use anyhow::{Error, Result};
use env_logger::Builder;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use hyper_staticfile::Static;
use log::{error, info};
use once_cell::race::OnceBox;
use serde::Deserialize;
use tokio::spawn;
use tokio_cron_scheduler::{Job, JobScheduler};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize, Clone, Debug)]
struct Config {
    mirrors: Vec<Mirror>,
    admin_servers: Option<Vec<AdminServer>>,
}

#[derive(Deserialize, Clone, Debug)]
struct Mirror {
    name: String,
    source: String,
    sync: Option<String>,
    serve: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
struct AdminServer {
    listen: String,
    token: String,
}

#[tokio::main]
async fn main() {
    let mut log_builder = Builder::new();
    log_builder.filter_level(log::LevelFilter::Info);
    log_builder.init();
    info!("mirror v{}", VERSION);

    if let Err(e) = main_intl().await {
        error!("{e}");
        // println!("\nBacktrace:\n\n{}", e.backtrace());
        exit(1);
    }
}

static GLOBAL_CONFIG: OnceBox<Config> = OnceBox::new();

async fn main_intl() -> Result<()> {
    let current_path = std::env::current_dir()?;
    let mut config_path = PathBuf::from(&current_path);
    config_path.push("mirror.yml");
    let config_raw = fs::read_to_string(config_path)?;
    GLOBAL_CONFIG
        .set(Box::new(serde_yaml::from_str(&*config_raw)?))
        .unwrap();
    let config = GLOBAL_CONFIG.get().unwrap();

    if config.mirrors.is_empty() {
        return Err(Error::msg("No mirror found."));
    }

    let scheduler = JobScheduler::new().await?;

    for mirror in &config.mirrors {
        info!("Initializing {}", &mirror.name);

        let mut root_path = PathBuf::from(&current_path);
        root_path.push("data");
        root_path.push(&mirror.name);
        create_dir_all(&root_path)?;

        if let Some(cron) = &mirror.sync {
            info!("Initializing sync task for {}", &mirror.name);

            scheduler
                .add(Job::new_async(&*(*cron), move |_uuid, _lock| {
                    Box::pin(sync(&mirror))
                })?)
                .await?;
        }

        if let Some(listen) = &mirror.serve {
            info!("Initializing server {} for {}", &listen, &mirror.name);

            let hyper_static = Static::new(&root_path);

            Server::bind(&SocketAddr::from_str(&*listen)?).serve(make_service_fn(move |_conn| {
                let hyper_static = hyper_static.clone();

                ready(Ok::<_, hyper::Error>(service_fn(move |req| {
                    serve_handler(req, hyper_static.clone())
                })))
            }));
        }
    }

    if let Some(admin_servers) = &config.admin_servers {
        for server in admin_servers {
            info!("Initializing admin server {}", &server.listen);

            Server::bind(&SocketAddr::from_str(&*server.listen)?).serve(make_service_fn(|_conn| {
                ready(Ok::<_, hyper::Error>(service_fn(|req| admin_handler(req))))
            }));
        }
    }

    Ok(())
}

async fn serve_handler<B>(
    req: Request<B>,
    hyper_static: Static,
) -> Result<Response<Body>, io::Error> {
    hyper_static.serve(req).await
}

async fn admin_handler<B>(req: Request<B>) -> Result<Response<Body>, Error> {
    // TODO auth

    match try_sync_by_name("").await {
        None => Ok(Response::builder()
            .status(400)
            .body("[MIRROR] not found".into())?),
        Some(_) => Ok(Response::builder()
            .status(200)
            .body("[MIRROR] sync started".into())?),
    }
}

async fn sync(mirror: &Mirror) {
    // TODO
}

async fn try_sync_by_name(name: &str) -> Option<()> {
    let option_mirror = GLOBAL_CONFIG
        .get()
        .unwrap()
        .mirrors
        .iter()
        .find(|&x| x.name == name);
    if let None = option_mirror {
        return None;
    }

    spawn(sync(option_mirror.unwrap()));

    Some(())
}
