use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::anyhow;
use askama::Template;
use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use clap::Parser;
use clap::Subcommand;
use clap_stdin::MaybeStdin;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use redb::Database;
use redb::ReadableDatabase;
use redb::ReadableTable;
use redb::TableDefinition;
use reqwest::Client;
use serde::Deserialize;
use text_splitter::TextSplitter;
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
    Serve,
    Push { content: MaybeStdin<String> },
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
                .with_state(AppState {
                    db: Arc::new(db),
                    config: Arc::new(config),
                });

            let listener = tokio::net::TcpListener::bind(&url).await.unwrap();
            axum::serve(listener, router).await?;
        }
        Subcmd::Push { content } => {
            let client = Client::new();
            let content = content.to_string();
            let result = client
                .post(format!("http://{}", config.url))
                .json(&serde_json::json!({
                    "content": content
                }))
                .header("PUSH_PASSWORD", config.push_password)
                .send()
                .await?
                .error_for_status()?
                .json::<PushResponse>()
                .await?;

            println!("index: {}", result.index);
        }
    }

    Ok(())
}

async fn home(
    State(state): State<AppState>,
    Query(HomeQuery { page }): Query<HomeQuery>,
) -> Result<impl IntoResponse, AnyhowError> {
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
                    .ok_or_else(|| anyhow!("Failed to get date by index: {}", index))?
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
                    Timestamp::from_second(timestemp as i64)?
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
) -> Result<impl IntoResponse, AnyhowError> {
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
    let result = table
        .get(index)?
        .ok_or_else(|| anyhow!("Failed to get index: {}", index))?
        .value();

    let template = Tmpl {
        title: config.title.clone(),
        content: result,
    };

    Ok(Html(template.render()?))
}

async fn push(
    header: HeaderMap,
    State(state): State<AppState>,
    Json(content_request): Json<ContentRequest>,
) -> Result<impl IntoResponse, AnyhowError> {
    let AppState { db, config } = state;
    let password = &config.push_password;

    if !header.get("PUSH_PASSWORD").is_some_and(|p| p == password) {
        return Err(anyhow!("Wrong password!").into());
    }

    let ContentRequest { content } = content_request;
    let write_txn = db.begin_write()?;
    let index = write_table(content, &write_txn)?;
    write_txn.commit()?;

    Ok(Json(serde_json::json!({
        "code": 0,
        "index":index,
    })))
}

fn write_table(content: String, write_txn: &redb::WriteTransaction) -> Result<u64, AnyhowError> {
    let index_blog_list: TableDefinition<u64, String> = TableDefinition::new("index_blog_list");
    let index_date_list: TableDefinition<u64, u64> = TableDefinition::new("index_date_list");
    let mut index_blog_table = write_txn.open_table(index_blog_list)?;
    let mut index_date_table = write_txn.open_table(index_date_list)?;

    let last_index = index_blog_table.last()?.map(|v| v.0.value());

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let index = match last_index {
        None => 0,
        Some(i) => i + 1,
    };

    index_blog_table.insert(index, content)?;
    index_date_table.insert(index, now)?;

    Ok(index)
}
