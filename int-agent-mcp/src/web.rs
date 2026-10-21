use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Query, Request as HttpRequest, State, rejection::QueryRejection},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, time::timeout};

use crate::{
    ipc,
    model::{Config, Request, now},
};

mod observation;

const QUERY_DEADLINE: Duration = Duration::from_secs(10);

struct WebState {
    config: Config,
    local_origins: [(String, String); 2],
}

type Shared = Arc<WebState>;
type ApiResult = std::result::Result<Json<Value>, ApiError>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1,"observed_at":now()}))).into_response()
    }
}

pub async fn run(config: Config, port: u16) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .context("binding read-only Web server to 127.0.0.1")?;
    let port = listener.local_addr()?.port();
    eprintln!("只读协作看板 http://127.0.0.1:{port}/ — Ctrl-C 仅关闭 Web，不停止 daemon 或 Agent");
    axum::serve(listener, router(config, port))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving read-only Web dashboard")
}

fn router(config: Config, port: u16) -> Router {
    let local_origins = ["127.0.0.1", "localhost"].map(|host| {
        let host = if port == 80 {
            host.to_owned()
        } else {
            format!("{host}:{port}")
        };
        let origin = format!("http://{host}");
        (host, origin)
    });
    let state = Arc::new(WebState {
        config,
        local_origins,
    });
    Router::new()
        .route("/", get(index))
        .route("/style.css", get(style))
        .route("/app.js", get(script))
        .route("/docs/observability.zh-CN", get(observability_doc))
        .route("/api/status", get(status))
        .route("/api/tasks", get(tasks))
        .route("/api/task", get(task))
        .fallback(|| async {
            ApiError(StatusCode::NOT_FOUND, "只提供明确的只读查询路径".into())
        })
        .layer(middleware::from_fn_with_state(state.clone(), boundary))
        .with_state(state)
}

// This is not a generic daemon proxy: even requests on unknown paths must pass
// the local origin / GET-only boundary before any handler can run.
async fn boundary(State(state): State<Shared>, request: HttpRequest, next: Next) -> Response {
    let mut response = if !local_request(&request, &state.local_origins) {
        ApiError(
            StatusCode::FORBIDDEN,
            "仅允许本机 Host 与同源浏览器查询".into(),
        )
        .into_response()
    } else if request.method() != Method::GET {
        let mut response =
            ApiError(StatusCode::METHOD_NOT_ALLOWED, "只允许 GET 只读查询".into()).into_response();
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET"));
        response
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    for (key, value) in [
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'",
        ),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=()",
        ),
    ] {
        headers.insert(key, HeaderValue::from_static(value));
    }
    response
}

fn local_request(request: &HttpRequest, allowed: &[(String, String); 2]) -> bool {
    let hosts = request.headers().get_all(header::HOST);
    if hosts.iter().count() != 1 {
        return false;
    }
    let Some(host) = hosts.iter().next().and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some((_, origin)) = allowed.iter().find(|(allowed, _)| allowed == host) else {
        return false;
    };
    if request
        .uri()
        .authority()
        .is_some_and(|authority| authority.as_str() != host)
    {
        return false;
    }
    if let Some(supplied_origin) = request.headers().get(header::ORIGIN)
        && supplied_origin.to_str().ok() != Some(origin.as_str())
    {
        return false;
    }
    if let Some(site) = request.headers().get("sec-fetch-site")
        && site != "same-origin"
        && site != "none"
    {
        return false;
    }
    true
}

async fn index() -> Response {
    asset(
        "text/html; charset=utf-8",
        include_str!("../assets/web/index.html"),
    )
}
async fn style() -> Response {
    asset(
        "text/css; charset=utf-8",
        include_str!("../assets/web/style.css"),
    )
}
async fn script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../assets/web/app.js"),
    )
}
async fn observability_doc() -> Response {
    asset(
        "text/plain; charset=utf-8",
        include_str!("../docs/observability.zh-CN.md"),
    )
}
fn asset(content_type: &'static str, content: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], content).into_response()
}

async fn query(state: &WebState, request: Request) -> std::result::Result<Value, ApiError> {
    match timeout(
        QUERY_DEADLINE,
        ipc::request(&state.config.socket(), &request),
    )
    .await
    {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => Err(ApiError(StatusCode::BAD_GATEWAY, format!("{error:#}"))),
        Err(_) => Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            "daemon 只读查询超过 10 秒；已有页面数据可能过期".into(),
        )),
    }
}

async fn status(State(state): State<Shared>) -> ApiResult {
    let result = query(&state, Request::Ping {}).await?;
    Ok(Json(json!({"observed_at":now(),"result":result})))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    after: Option<String>,
    limit: Option<u32>,
}

fn bad_query(error: QueryRejection) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, error.body_text())
}
fn validate_id(id: &str) -> std::result::Result<(), ApiError> {
    if !crate::config::safe_name(id) || id == ".." {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "任务或游标标识须为 1–100 个简单 ASCII 字符".into(),
        ));
    }
    Ok(())
}
async fn tasks(
    State(state): State<Shared>,
    params: std::result::Result<Query<ListQuery>, QueryRejection>,
) -> ApiResult {
    let Query(params) = params.map_err(bad_query)?;
    if let Some(after) = &params.after {
        validate_id(after)?;
    }
    let limit = params.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "limit 须为 1–100".into()));
    }
    let result = query(
        &state,
        Request::List {
            after: params.after,
            limit: Some(limit),
        },
    )
    .await?;
    Ok(Json(json!({"observed_at":now(),"result":result})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskQuery {
    task_id: String,
    events_after: Option<i64>,
    event_limit: Option<u32>,
}
async fn task(
    State(state): State<Shared>,
    params: std::result::Result<Query<TaskQuery>, QueryRejection>,
) -> ApiResult {
    let Query(params) = params.map_err(bad_query)?;
    validate_id(&params.task_id)?;
    let after = params.events_after.unwrap_or(0);
    let limit = params.event_limit.unwrap_or(25);
    if after < 0 || limit > 100 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "events_after 须非负，event_limit 须为 0–100".into(),
        ));
    }
    let result = query(
        &state,
        Request::Get {
            task_id: params.task_id,
            events_after: Some(after),
            event_limit: Some(limit),
        },
    )
    .await?;
    let task = observation::Runs::deserialize(&result["task"]).map_err(|error| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            format!("daemon 任务运行身份无法解析: {error}"),
        )
    })?;
    let observations = observation::inspect(&state.config, &task).await;
    Ok(Json(
        json!({"observed_at":now(),"result":result,"observations":observations}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request as Builder,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };
    use tower::ServiceExt;

    fn config(directory: &std::path::Path) -> Config {
        serde_json::from_value(json!({"repository": directory, "worktree_root":directory.join("worktrees"), "state_dir":directory})).unwrap()
    }
    fn request(method: &str, uri: &str, host: &str) -> HttpRequest {
        Builder::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    }
    async fn document(response: Response) -> Value {
        serde_json::from_slice(
            &to_bytes(response.into_body(), 8 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn serves_only_embedded_observability_doc_without_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let app = router(config(directory.path()), 8765);
        let uri = "/docs/observability.zh-CN";
        let response = app
            .clone()
            .oneshot(request("GET", uri, "localhost:8765"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["cache-control"], "no-store");
        let csp = response.headers()["content-security-policy"].clone();
        let index = app
            .clone()
            .oneshot(request("GET", "/", "localhost:8765"))
            .await
            .unwrap();
        assert_eq!(csp, index.headers()["content-security-policy"]);

        let response = app
            .clone()
            .oneshot(request("POST", uri, "localhost:8765"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], "GET");
        assert_eq!(response.headers()["content-security-policy"], csp);

        let response = app
            .clone()
            .oneshot(request("GET", uri, "evil.example:8765"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()["content-security-policy"], csp);
        let mut cross_origin = request("GET", uri, "localhost:8765");
        cross_origin.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert_eq!(
            app.clone().oneshot(cross_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        for unknown in [
            "/docs/unknown",
            "/docs/observability.zh-CN.md",
            "/docs/observability.zh-CN/",
            "/docs/../Cargo.toml",
            "/docs/%2e%2e/Cargo.toml",
            "/docs/observability.zh-CN/../../Cargo.toml",
        ] {
            let response = app
                .clone()
                .oneshot(request("GET", unknown, "localhost:8765"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{unknown}");
            assert_eq!(response.headers()["content-security-policy"], csp);
        }
    }

    #[tokio::test]
    async fn rejects_write_and_cross_origin_without_contacting_ipc() {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(directory.path().join("controller.sock")).unwrap();
        let app = router(config(directory.path()), 8765);
        for method in ["POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD"] {
            let response = app
                .clone()
                .oneshot(request(method, "/api/status", "127.0.0.1:8765"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
        for host in [
            "evil.example:8765",
            "127.0.0.1.evil:8765",
            "127.0.0.2:8765",
            "localhost:80",
            "127.0.0.1",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request("GET", "/api/status", host))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        let mut cross_site = request("GET", "/api/status", "localhost:8765");
        cross_site.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert_eq!(
            app.clone().oneshot(cross_site).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let mut duplicate_host = request("GET", "/api/status", "localhost:8765");
        duplicate_host
            .headers_mut()
            .append(header::HOST, HeaderValue::from_static("evil.example"));
        assert_eq!(
            app.clone().oneshot(duplicate_host).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let mut cross_site = request("GET", "/api/status", "localhost:8765");
        cross_site
            .headers_mut()
            .insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert_eq!(
            app.clone().oneshot(cross_site).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    "GET",
                    "http://evil.example/api/status",
                    "localhost:8765"
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let mut no_host = request("GET", "/api/status", "localhost:8765");
        no_host.headers_mut().remove(header::HOST);
        assert_eq!(
            app.clone().oneshot(no_host).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        for uri in ["/api/cancel?task_id=x", "/api/file?path=/etc/passwd"] {
            assert_eq!(
                app.clone()
                    .oneshot(request("GET", uri, "localhost:8765"))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
        assert!(
            timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_invalid_cursors_and_extra_operation_fields_before_ipc() {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(directory.path().join("controller.sock")).unwrap();
        let app = router(config(directory.path()), 8765);
        for uri in [
            "/api/tasks?limit=0",
            "/api/tasks?limit=101",
            "/api/tasks?after=..",
            "/api/tasks?op=cancel",
            "/api/task?task_id=x&events_after=-1",
            "/api/task?task_id=x&event_limit=101",
            "/api/task?task_id=../../secret",
            "/api/task",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request("GET", uri, "127.0.0.1:8765"))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST,
                "{uri}"
            );
        }
        assert!(
            timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    // A real private Unix socket validates the browser-to-IPC isolation boundary:
    // cursor values survive unchanged and errors are not replaced with empty data.
    #[tokio::test]
    async fn preserves_list_pagination_and_daemon_errors_over_real_ipc() {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(directory.path().join("controller.sock")).unwrap();
        let server = tokio::spawn(async move {
            for expected in [
                json!({"op":"list","after":"t-cursor","limit":2}),
                json!({"op":"get","task_id":"missing","events_after":41,"event_limit":3}),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                stream.read_to_end(&mut data).await.unwrap();
                assert_eq!(serde_json::from_slice::<Value>(&data).unwrap(), expected);
                let response = if expected["op"] == "list" {
                    json!({"ok":true,"result":{"tasks":[{"id":"t-second"}],"next_after":"t-second","integration_blockers":["t-blocked"]}})
                } else {
                    json!({"ok":false,"error":"unknown task missing"})
                };
                stream
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let app = router(config(directory.path()), 8765);
        let response = app
            .clone()
            .oneshot(request(
                "GET",
                "/api/tasks?after=t-cursor&limit=2",
                "localhost:8765",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            document(response).await["result"],
            json!({"tasks":[{"id":"t-second"}],"next_after":"t-second","integration_blockers":["t-blocked"]})
        );
        let response = app
            .oneshot(request(
                "GET",
                "/api/task?task_id=missing&events_after=41&event_limit=3",
                "localhost:8765",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(document(response).await["error"], "unknown task missing");
        server.await.unwrap();
        assert!(
            !directory.path().join("team.sqlite3").exists(),
            "Web queries must not open a database"
        );
    }
}
