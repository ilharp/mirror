use std::future::ready;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::str::FromStr;

use anyhow::{Error, Result};
use env_logger::Builder;
use futures_util::StreamExt;
use hyper::header;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use hyper_staticfile::Static;
use log::{error, info};
use once_cell::race::OnceBox;
use reqwest::get;
use serde::Deserialize;
use tokio::fs::{create_dir_all, read_to_string, remove_file, OpenOptions};
use tokio::io;
use tokio::io::copy;
use tokio::signal::ctrl_c;
use tokio::spawn;
use tokio_cron_scheduler::{Job, JobScheduler};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize, Clone, Debug)]
struct Config {
    mirrors: Vec<Mirror>,
    admin_server: Option<AdminServer>,
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
        println!("\nBacktrace:\n\n{}", e.backtrace());
        exit(1);
    }
}

static GLOBAL_CONFIG: OnceBox<Config> = OnceBox::new();
static CURRENT_PATH: OnceBox<PathBuf> = OnceBox::new();
static DATA_PATH: OnceBox<PathBuf> = OnceBox::new();
static TEMP_PATH: OnceBox<PathBuf> = OnceBox::new();

async fn main_intl() -> Result<()> {
    CURRENT_PATH
        .set(Box::new(std::env::current_dir()?))
        .unwrap();
    let current_path = CURRENT_PATH.get().unwrap();
    DATA_PATH
        .set(Box::new(Path::new(&current_path).join("data")))
        .unwrap();
    let data_path = DATA_PATH.get().unwrap();
    TEMP_PATH
        .set(Box::new(Path::new(&current_path).join("tmp")))
        .unwrap();

    let config_path = Path::new(&current_path).join("mirror.yml");
    let try_config_raw = read_to_string(config_path).await;
    let config_raw: OnceBox<String> = OnceBox::new();
    if let Err(_) = try_config_raw {
        let config_path = Path::new(&current_path).join("config/mirror.yml");
        config_raw
            .set(Box::new(read_to_string(config_path).await?))
            .unwrap();
    } else {
        config_raw.set(Box::new(try_config_raw.unwrap())).unwrap();
    }
    GLOBAL_CONFIG
        .set(Box::new(serde_yaml::from_str(config_raw.get().unwrap())?))
        .unwrap();
    let config = GLOBAL_CONFIG.get().unwrap();

    if config.mirrors.is_empty() {
        return Err(Error::msg("No mirror found."));
    }

    let scheduler = JobScheduler::new().await?;

    for mirror in &config.mirrors {
        info!("Initializing {}", &mirror.name);

        let root_path = Path::new(&data_path).join(&mirror.name);
        create_dir_all(&root_path).await?;

        info!("Initializing {}", &mirror.name);
        spawn(sync(&mirror));

        if let Some(cron) = &mirror.sync {
            info!("Initializing sync task for {}", &mirror.name);

            scheduler
                .add(Job::new_async(&*(*cron), move |_uuid, _lock| {
                    info!("Sync for {} started by cron", mirror.name);
                    Box::pin(sync(&mirror))
                })?)
                .await?;
        }

        if let Some(listen) = &mirror.serve {
            info!("Initializing server {} for {}", &listen, &mirror.name);

            let hyper_static = Static::new(&root_path);

            spawn(
                Server::bind(&SocketAddr::from_str(&*listen)?).serve(make_service_fn(
                    move |_conn| {
                        let hyper_static = hyper_static.clone();

                        ready(Ok::<_, hyper::Error>(service_fn(move |req| {
                            serve_handler(req, hyper_static.clone())
                        })))
                    },
                )),
            );
        }
    }

    if let Some(server) = &config.admin_server {
        info!("Initializing admin server {}", &server.listen);

        spawn(
            Server::bind(&SocketAddr::from_str(&*server.listen)?).serve(make_service_fn(|_conn| {
                ready(Ok::<_, hyper::Error>(service_fn(|req| admin_handler(req))))
            })),
        );
    }

    scheduler.start().await?;

    ctrl_c().await?;
    info!("Bye!");

    Ok(())
}

async fn serve_handler<B>(
    req: Request<B>,
    hyper_static: Static,
) -> Result<Response<Body>, io::Error> {
    hyper_static.serve(req).await
}

async fn admin_handler<B>(req: Request<B>) -> Result<Response<Body>, Error> {
    let token = &GLOBAL_CONFIG
        .get()
        .unwrap()
        .admin_server
        .as_ref()
        .unwrap()
        .token;
    let headers = req.headers();
    match headers.get(header::AUTHORIZATION) {
        None => {
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body("[MIRROR] forbidden".into())?);
        }
        Some(auth) => {
            if String::from("Bearer ") + token != auth.to_str()? {
                return Ok(Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body("[MIRROR] forbidden".into())?);
            }
        }
    }

    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body("[MIRROR] method not allowed".into())?);
    }

    let request_uri = req.uri().path();
    if !request_uri.starts_with("/sync/") {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("[MIRROR] not found".into())?);
    }

    let name = request_uri.strip_prefix("/sync/");
    if let None = name {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("[MIRROR] not found".into())?);
    }
    let name = name.unwrap();

    match try_sync_by_name(name).await {
        None => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("[MIRROR] not found".into())?),
        Some(_) => {
            info!("Sync for {name} started as request");

            Ok(Response::builder()
                .status(StatusCode::OK)
                .body("[MIRROR] sync started".into())?)
        }
    }
}

async fn sync(mirror: &Mirror) {
    if let Err(e) = sync_intl(mirror).await {
        error!("Error when syncing {}", mirror.name);
        error!("{e}");
        println!("\nBacktrace:\n\n{}", e.backtrace());
    }
}

async fn sync_intl(mirror: &Mirror) -> Result<()> {
    let filename = mirror.name.clone() + ".zip";
    let temp_path = TEMP_PATH.get().unwrap();
    create_dir_all(&temp_path).await?;
    let filepath = Path::new(temp_path).join(&filename);

    let response = get(&mirror.source).await?;

    if response.status() != StatusCode::OK {
        return Err(Error::msg(format!(
            "Sync for {} failed: source response status {}",
            mirror.name,
            response.status()
        )));
    }

    let _ = remove_file(&filepath).await;

    let mut async_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&filepath)
        .await?;

    let mut response_data = response.bytes_stream();
    while let Some(i) = response_data.next().await {
        copy(&mut i?.as_ref(), &mut async_file).await?;
    }

    let data_path = DATA_PATH.get().unwrap();
    let root_path = Path::new(&data_path).join(&mirror.name);

    let file = async_file.into_std().await;
    unzip(file, root_path)?;

    remove_file(filepath).await?;

    Ok(())
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

fn unzip(file: std::fs::File, base_path: PathBuf) -> Result<()> {
    // Use sync operations instead of tokio only in this fn
    use std::{fs, io};

    let mut archive = zip::ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;

        let out_path = match file.enclosed_name() {
            Some(path) => Path::new(&base_path).join(path),
            None => continue,
        };

        if (file.name()).ends_with('/') {
            let _ = fs::create_dir_all(&out_path);
        } else {
            if let Some(p) = out_path.parent() {
                let _ = fs::create_dir_all(p);
            }
            let mut outfile = fs::File::create(&out_path)?;
            io::copy(&mut file, &mut outfile)?;
        }
    }

    Ok(())
}
