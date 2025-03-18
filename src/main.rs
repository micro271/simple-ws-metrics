use axum::{
    extract::ConnectInfo,
    http::{Response, StatusCode, Uri},
    response::Html,
    routing::{get, post},
    serve,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::{collections::VecDeque, env, net::SocketAddr, path::PathBuf};
use tokio::{fs, sync::Mutex};
use tower_http::trace::TraceLayer;
use tracing::Span;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Serialize, Deserialize, Default)]
struct Data {
    combustible: Option<f32>,
    humedad: Option<f32>,
    temperatura: Option<f32>,
}

#[derive(Debug)]
struct DataLimit {
    data: Option<Data>,
    limit: u8,
    counter: u8,
}

impl std::default::Default for DataLimit {
    fn default() -> Self {
        Self {
            data: None,
            limit: 5,
            counter: 0,
        }
    }
}

impl DataLimit {
    fn subtract(&mut self) {
        self.counter += 1;
        if self.counter == self.limit {
            self.data = None;
        }
    }
    fn reset_counter(&mut self) {
        self.counter = 0;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    _ = dotenv::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LOG_LEVEL")
                .unwrap_or(EnvFilter::new("info")),
        )
        .init();

    let trace = TraceLayer::new_for_http()
        .make_span_with(|req: &axum::http::Request<axum::body::Body>| {
            let peer = tracing::field::display(
                req.extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .map_or("Unknown".to_string(), |x| x.ip().to_string()),
            );
            tracing::info_span!("http_log", uri = %req.uri(), method = %req.method(), peer = %peer, latency = tracing::field::Empty,
            status = tracing::field::Empty, user = tracing::field::Empty, role = tracing::field::Empty)
        })
        .on_response(|req: &axum::http::response::Response<axum::body::Body>, dur: std::time::Duration, span: &Span,| {
            span.record(
                "latency",
                tracing::field::display(format!("{}ms", dur.as_millis())),
            );
            span.record("status", tracing::field::display(req.status()));
            tracing::info!("Response");
        });

    let socket = format!(
        "{}:{}",
        env::var("IP_ADDRESS")
            .ok()
            .filter(|x| !x.is_empty())
            .unwrap_or("0.0.0.0".to_string()),
        env::var("PORT")
            .ok()
            .filter(|x| !x.is_empty())
            .unwrap_or("3000".to_string())
    );

    let lst = tokio::net::TcpListener::bind(&socket).await?;

    let offices = env::var("OFFICES")
        .map(|x| {
            x.split(",")
                .map(ToString::to_string)
                .collect::<Vec<String>>()
        })
        .expect("The allowed offices are very important");

    tracing::info!("Listening: {}", socket);

    let router_html = get_paths()
        .await?
        .into_iter()
        .fold(axum::Router::new(), |router, path| {
            if path.extension().unwrap().to_str().unwrap() == "html" {
                let stem = path.file_stem().unwrap().to_str().unwrap();
                if offices.contains(&stem.to_string()) {
                    router.route(
                        &format!(
                            "/{}",
                            if stem.eq(offices.first().unwrap()) {
                                ""
                            } else {
                                stem
                            }
                        ),
                        get(async || {
                            Html(
                                fs::read_to_string(path)
                                    .await
                                    .unwrap_or("<h1>Unexpected error</h1>".to_string()),
                            )
                        }),
                    )
                } else {
                    tracing::info!("Exclude {:?}", path);
                    router
                }
            } else {
                router.route(
                    &format!("/{}", path.file_name().unwrap().to_str().unwrap()),
                    get(async || {
                        Response::builder()
                            .header(
                                "Content-Type",
                                if path.extension().unwrap().to_str().unwrap() == "js" {
                                    "application/javascript"
                                } else {
                                    "text/css"
                                },
                            )
                            .body(
                                fs::read_to_string(path)
                                    .await
                                    .unwrap_or("<h1>Unexpected Error</h1>".to_string()),
                            )
                            .unwrap_or_default()
                    }),
                )
            }
        });

    let state = Arc::new(Mutex::new({
        let mut tmp = HashMap::new();
        for office in offices {
            tmp.insert(office, DataLimit::default());
        }
        tracing::info!("List of data: {:#?}", tmp);
        tmp
    }));

    let route = axum::Router::new()
        .merge(router_html)
        .route("/ultimo/{office}", get(handlers::last))
        .route("/api/{office}", post(handlers::insert))
        .with_state(state)
        .fallback(async |uri: Uri| {
            (
                StatusCode::NOT_FOUND,
                format!("Route {} not found", uri),
            )
        })
        .layer(trace)
        .layer(tower_http::cors::CorsLayer::new().allow_origin(tower_http::cors::Any));

    Ok(serve(lst, route).await?)
}

mod handlers {
    use super::{Arc, Data, DataLimit, Mutex};
    use axum::{
        Json,
        extract::{Path, State},
        http::{StatusCode, Uri},
        response::Response,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use tracing::instrument;

    type StateData = Arc<Mutex<HashMap<String, DataLimit>>>;

    pub async fn last(
        State(state): State<StateData>,
        Path(office): Path<String>,
        uri: Uri,
    ) -> Response {
        let mut state = state.lock().await;
        if let Some(data) = state.get_mut(&office).filter(|x| x.data.is_some()) {
            data.subtract();

            Response::builder()
                .header("Content-Type", "application/json")
                .status(StatusCode::OK)
                .body(json!(&data.data).to_string().into())
                .unwrap_or_default()
        } else {
            tracing::error!("The office {} is not present in State", office);
            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(
                    json!({
                        "Error": "Office not found",
                        "Uri": uri.to_string(),
                    })
                    .to_string()
                    .into(),
                )
                .unwrap_or_default()
        }
    }

    #[instrument]
    pub async fn insert(
        State(state): State<StateData>,
        Path(office): Path<String>,
        Json(data): Json<Data>,
    ) -> StatusCode {
        let mut state = state.lock().await;
        
        if let Some(e) = state.get_mut(&office) {
            tracing::info!("New DATA: {:?}", data);
            e.reset_counter();
            e.data = Some(data);
        } else {
            tracing::error!("The office {:?} does not find", data);
        }
        
        StatusCode::OK
    }
}

async fn get_paths() -> Result<Vec<PathBuf>, String> {
    let mut resp = Vec::new();

    let mut dirs = VecDeque::new();

    let allow_extensions = ["css", "js", "html"];

    let path_html = env::var("WWW").unwrap_or("./www".to_string());

    tracing::info!("Web directory: {}", path_html);

    dirs.push_front(PathBuf::from(path_html));

    tracing::info!("Allowed extensions: {:#?}", allow_extensions);

    while let Some(dir) = dirs.pop_front() {
        let mut tmp = fs::read_dir(dir).await.unwrap();

        while let Ok(Some(e)) = tmp.next_entry().await {
            let path = fs::canonicalize(e.path()).await.unwrap();
            if path.is_dir() {
                dirs.push_back(path);
            } else if allow_extensions
                .contains(&path.extension().unwrap_or_default().to_str().unwrap())
            {
                resp.push(path);
            }
        }
    }

    tracing::info!("Files find: {:#?}", resp);

    Ok(resp)
}
