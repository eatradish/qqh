use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use clap_stdin::MaybeStdin;
use constant_time_eq::constant_time_eq;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use redb::{Database, DatabaseError, ReadableDatabase, ReadableTable, TableDefinition};
use reqwest::Client;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use text_splitter::TextSplitter;
use thiserror::Error;
use tracing::info;

#[derive(Debug, Parser)]
struct App {
    #[command(subcommand)]
    subcmd: Subcmd,
    #[arg(short, long, default_value = "config.toml")]
    config_path: PathBuf,
}

#[derive(Debug, Subcommand)]
enum Subcmd {
    /// Start the Web server to host the content and handle requests.
    Serve,
    /// Push new content into the database.
    ///
    /// This command sends the content to the server. You can provide
    /// the content as a command-line argument or pipe it via stdin.
    Push {
        /// The text content to be stored.
        content: MaybeStdin<String>,
    },
    /// Pop the most recent entry from the database.
    ///
    /// This is a destructive operation: it retrieves the latest
    /// record and immediately removes it from the storage.
    Pop,
    /// Remove a specific entry by its index.
    ///
    /// This is a destructive operation that permanently deletes the
    /// record at the specified index from the database.
    Remove {
        /// The unique ID (index) of the entry to be deleted.
        index: u64,
    },
}

#[derive(Debug, Clone)]
struct AppState {
    db: Arc<Database>,
    config: Arc<Config>,
}

#[derive(Debug, Clone, Deserialize)]
struct Config {
    title: String,
    url: String,
    db_path: String,
    page_content: u64,
    split_length: u64,
    push_password: String,
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Authentication failed: Invalid Bearer token")]
    Unauthorized,
    #[error("Database is currently locked by another process")]
    DatabaseLocked,
    #[error("Resource not found")]
    NotFound,
    #[error("Database integrity error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("Table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("Storage transaction failed: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("Network request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Configuration or IO failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("Template rendering failed: {0}")]
    Template(#[from] askama::Error),
    #[error("Internal system error: {0}")]
    Internal(#[from] anyhow::Error),
    #[error("Storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("Commit error: {0}")]
    Commit(#[from] redb::CommitError),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match &self {
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            AppError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::Database(redb::DatabaseError::DatabaseAlreadyOpen) => {
                (StatusCode::LOCKED, self.to_string())
            }
            _ => {
                tracing::error!("Internal error detail: {:?}", self);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "An internal server error occurred".into(),
                )
            }
        };

        (status, Json(serde_json::json!({ "error": error_message }))).into_response()
    }
}

// learned from https://github.com/tokio-rs/axum/blob/main/examples/anyhow-error-response/src/main.rs
pub struct AnyhowError(anyhow::Error);

impl IntoResponse for AnyhowError {
    fn into_response(self) -> Response {
        info!("Returning internal server error for {}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", self.0)).into_response()
    }
}

impl<E> From<E> for AnyhowError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Deserialize)]
struct ContentRequest {
    content: String,
}

#[allow(unused)]
#[derive(Debug, Deserialize)]
struct PushResponse {
    code: i64,
    index: usize,
}

#[derive(Debug, Deserialize)]
struct HomeQuery {
    page: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RemoveRequest {
    index: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app = App::parse();

    let config_path = app.config_path;

    let config = std::fs::read_to_string(config_path)?;
    let config: Config = toml::from_str(&config)?;

    match app.subcmd {
        Subcmd::Serve => {
            let db = Database::create(&config.db_path)?;
            let url = config.url.clone();

            let router = Router::new()
                .route("/", get(home))
                .route("/", post(push))
                .route("/{id}", get(get_content))
                .route("/remove", post(remove))
                .route("/newset", get(newest))
                .route("/pop", get(pop))
                .with_state(AppState {
                    db: Arc::new(db),
                    config: Arc::new(config),
                });

            let listener = tokio::net::TcpListener::bind(&url).await.unwrap();
            axum::serve(listener, router).await?;
        }
        Subcmd::Push { content } => match Database::create(&config.db_path) {
            Ok(db) => {
                let write_txn = db.begin_write()?;
                let index = write_table(content.to_string(), &write_txn)?;
                println!("index: {}", index);
            }
            Err(e) => {
                if let DatabaseError::DatabaseAlreadyOpen = e {
                    let client = Client::new();
                    let content = content.to_string();
                    let result = client
                        .post(format!("http://{}", config.url))
                        .json(&serde_json::json!({
                            "content": content
                        }))
                        .header(AUTHORIZATION, format!("Bearer {}", config.push_password))
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<PushResponse>()
                        .await?;
                    println!("index: {}", result.index);
                } else {
                    return Err(e.into());
                }
            }
        },
        Subcmd::Remove { index } => match Database::open(&config.db_path) {
            Ok(db) => {
                let write = db.begin_write()?;
                remove_from_table(index, &write)?;
                write.commit()?;
                println!("Index: {}", index);
            }
            Err(e) => {
                if let DatabaseError::DatabaseAlreadyOpen = e {
                    let client = Client::new();
                    http_remove_inner(&client, &config, index).await?;
                }
            }
        },
        Subcmd::Pop => match Database::open(&config.db_path) {
            Ok(db) => {
                let write_txn = db.begin_write()?;
                pop_from_table(&write_txn)?;
                write_txn.commit()?;
            }
            Err(e) => {
                if let DatabaseError::DatabaseAlreadyOpen = e {
                    let client = Client::new();
                    client
                        .post(format!("http://{}/pop", config.url))
                        .header(AUTHORIZATION, format!("Bearer {}", config.push_password))
                        .send()
                        .await?
                        .error_for_status()?;
                    println!("Popped the most recent entry.");
                } else {
                    return Err(e.into());
                }
            }
        },
    }

    Ok(())
}

async fn http_remove_inner(
    client: &Client,
    config: &Config,
    index: u64,
) -> Result<(), anyhow::Error> {
    let result = client
        .post(format!("http://{}/remove", config.url))
        .json(&serde_json::json!({
            "index": index,
        }))
        .header(AUTHORIZATION, format!("Bearer {}", config.push_password))
        .send()
        .await?
        .error_for_status()?
        .json::<RemoveRequest>()
        .await?;

    println!("Index: {}", result.index);

    Ok(())
}

async fn home(
    State(state): State<AppState>,
    Query(HomeQuery { page }): Query<HomeQuery>,
) -> Result<impl IntoResponse, AppError> {
    let AppState { db, config } = state;
    let page = page.unwrap_or(1);
    let start = (page - 1) * config.page_content;
    let end = start + config.page_content;

    #[derive(Debug, Template)]
    #[template(path = "index.html")]
    struct Tmpl {
        title: String,
        contents: Vec<(String, String)>,
    }

    let index_blog_list: TableDefinition<u64, String> = TableDefinition::new("index_blog_list");
    let index_date_list: TableDefinition<u64, u64> = TableDefinition::new("index_date_list");

    let read = db.begin_read()?;
    let mut contents = vec![];
    {
        let index_blog_table = read.open_table(index_blog_list)?;
        let index_date_table = read.open_table(index_date_list)?;
        if let Ok(index_blog_table) = index_blog_table.range(start..end) {
            for i in index_blog_table {
                let i = i?;
                let (index, content) = (i.0.value(), i.1.value());
                let timestemp = index_date_table
                    .get(index)?
                    .ok_or_else(|| AppError::Internal(anyhow!("Missing date for index {}", index)))?
                    .value();

                let split = TextSplitter::new(config.split_length as usize);
                let mut split = split.chunks(&content);
                let content = split.next().unwrap_or_default();
                let content = if split.next().is_some() {
                    format!("{}..", content)
                } else {
                    content.to_string()
                };

                contents.push((
                    content,
                    Timestamp::from_second(timestemp as i64)
                        .map_err(|e| {
                            AppError::Internal(anyhow!("Failed to convert timestemp to date: {e}"))
                        })?
                        .to_zoned(TimeZone::system())
                        .strftime("%Y-%m-%d %H:%M:%S")
                        .to_string(),
                ));
            }
        }
    }

    contents.reverse();

    let template = Tmpl {
        title: config.title.clone(),
        contents,
    };

    Ok(Html(template.render()?))
}

async fn get_content(
    State(state): State<AppState>,
    Path(index): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    #[derive(Debug, Template)]
    #[template(path = "page.html")]
    struct Tmpl {
        title: String,
        content: String,
    }

    let AppState { db, config } = state;
    let read = db.begin_read()?;
    let index_blog_list: TableDefinition<u64, String> = TableDefinition::new("index_blog_list");
    let table = read.open_table(index_blog_list)?;
    let result = table.get(index)?.ok_or_else(|| AppError::NotFound)?.value();

    let template = Tmpl {
        title: config.title.clone(),
        content: result,
    };

    Ok(Html(template.render()?))
}

async fn pop(
    header: HeaderMap,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    check(header, &state.config.push_password)?;

    let AppState { db, .. } = state;
    let write_txn = db.begin_write()?;

    pop_from_table(&write_txn)?;

    write_txn.commit()?;

    Ok(Json(serde_json::json!({
        "status": 0
    })))
}

async fn push(
    header: HeaderMap,
    State(state): State<AppState>,
    Json(content_request): Json<ContentRequest>,
) -> Result<impl IntoResponse, AppError> {
    let AppState { db, config } = state;
    let password = &config.push_password;

    check(header, password)?;

    let ContentRequest { content } = content_request;
    let write_txn = db.begin_write()?;
    let index = write_table(content, &write_txn)?;
    write_txn.commit()?;

    Ok(Json(serde_json::json!({
        "code": 0,
        "index":index,
    })))
}

fn check(header: HeaderMap, password: &str) -> Result<(), anyhow::Error> {
    if !header
        .get(AUTHORIZATION)
        .and_then(|p| p.to_str().unwrap_or_default().strip_prefix("Bearer "))
        .map(|s| s.trim())
        .is_some_and(|p| constant_time_eq(p.as_bytes(), password.as_bytes()))
    {
        return Err(anyhow!("Wrong password!").into());
    }

    Ok(())
}

async fn remove(
    header: HeaderMap,
    State(state): State<AppState>,
    Json(request): Json<RemoveRequest>,
) -> Result<impl IntoResponse, AnyhowError> {
    let AppState { db, config } = state;
    let password = &config.push_password;
    let RemoveRequest { index } = request;

    check(header, password)?;

    let write_txn = db.begin_write()?;

    {
        remove_from_table(index, &write_txn)?;
    }

    write_txn.commit()?;

    Ok(Json(serde_json::json!({
        "code": 0,
        "index": index,
    })))
}

fn remove_from_table(index: u64, write_txn: &redb::WriteTransaction) -> Result<(), anyhow::Error> {
    let index_blog_list: TableDefinition<u64, String> = TableDefinition::new("index_blog_list");
    let index_date_list: TableDefinition<u64, u64> = TableDefinition::new("index_date_list");
    let mut index_blog_table = write_txn.open_table(index_blog_list)?;
    let mut index_date_table = write_txn.open_table(index_date_list)?;
    index_blog_table.remove(index)?;
    index_date_table.remove(index)?;

    Ok(())
}

fn pop_from_table(write_txn: &redb::WriteTransaction) -> Result<(), anyhow::Error> {
    let index_blog_list: TableDefinition<u64, String> = TableDefinition::new("index_blog_list");
    let index_date_list: TableDefinition<u64, u64> = TableDefinition::new("index_date_list");
    let mut index_blog_table = write_txn.open_table(index_blog_list)?;
    let mut index_date_table = write_txn.open_table(index_date_list)?;

    index_blog_table.pop_last()?;
    index_date_table.pop_last()?;

    Ok(())
}

async fn newest(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let AppState { db, config } = state;
    let read = db.begin_read()?;
    let last = get_last_index(read)?.ok_or_else(|| AppError::NotFound)?;

    Ok(Redirect::to(&format!("https://{}/{}", config.url, last)))
}

fn get_last_index(read: redb::ReadTransaction) -> Result<Option<u64>, AppError> {
    let index_blog_list: TableDefinition<u64, String> = TableDefinition::new("index_blog_list");
    let index_blog_table = read.open_table(index_blog_list)?;
    let last = index_blog_table.last()?.map(|r| r.0.value());

    Ok(last)
}

fn write_table(content: String, write_txn: &redb::WriteTransaction) -> Result<u64, AppError> {
    let index_blog_list: TableDefinition<u64, String> = TableDefinition::new("index_blog_list");
    let index_date_list: TableDefinition<u64, u64> = TableDefinition::new("index_date_list");
    let mut index_blog_table = write_txn.open_table(index_blog_list)?;
    let mut index_date_table = write_txn.open_table(index_date_list)?;

    let last_index = index_blog_table.last()?.map(|v| v.0.value());

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AppError::Internal(anyhow!("Failed to convert timestemp to date: {e}")))?
        .as_secs();

    let index = match last_index {
        None => 0,
        Some(i) => i + 1,
    };

    index_blog_table.insert(index, content)?;
    index_date_table.insert(index, now)?;

    Ok(index)
}
