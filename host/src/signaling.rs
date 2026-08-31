//! HTTP + WebSocket sunucusu.
//!
//! Aynı port iki iş görür:
//! - `/`   : TV istemcisinin statik dosyaları (tarayıcıdan test için de kullanışlı)
//! - `/ws` : WebRTC sinyalleşmesi (offer/answer takası)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use tokio::sync::{broadcast, watch};
use tower_http::services::ServeDir;

use crate::audio::AudioFrame;
use crate::engine::EncodedFrame;
use crate::protocol::CursorState;

#[derive(Clone)]
pub struct AppState {
    pub encoded_tx: broadcast::Sender<Arc<EncodedFrame>>,
    /// 1080p hafif akış (kaynak >1080p ise; "lite" profilli istemciler alır).
    pub lite_tx: Option<broadcast::Sender<Arc<EncodedFrame>>>,
    pub keyframe_request: Arc<AtomicBool>,
    pub cursor_rx: watch::Receiver<CursorState>,
    pub audio_tx: Option<broadcast::Sender<Arc<AudioFrame>>>,
}

/// Uçtan uca (cam-camdan) gecikme ölçümü için ms sayacı sayfası:
/// bu sayfayı sanal ekrana koy, telefon kamerasıyla iki ekranı aynı karede çek —
/// sayaçların farkı = gerçek toplam gecikme.
const CLOCK_HTML: &str = r#"<!doctype html><meta charset="utf-8"><title>saat</title>
<body style="background:#000;color:#0f0;font:700 14vw monospace;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
<div id="t"></div><script>const e=document.getElementById('t');
(function f(){e.textContent=(performance.now()|0)+' ms';requestAnimationFrame(f)})()</script>"#;

async fn clock() -> impl IntoResponse {
    axum::response::Html(CLOCK_HTML)
}

/// SIGTERM beklemesi (Unix). `pkill`, `systemctl stop` ve oturum kapatma bunu
/// yollar; yakalanmazsa süreç aniden ölür ve kapanış temizliği ÇALIŞMAZ —
/// ölçüldü: Linux'ta `--tv-audio` sonrası varsayılan ses aygıtı sanal çıkışta
/// takılı kalıyordu. Windows'ta karşılığı yok, orada sonsuza dek bekler.
#[cfg(unix)]
async fn terminate_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(_) => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn terminate_signal() {
    std::future::pending().await
}

/// Bilgisayar adı: Windows'ta COMPUTERNAME, Linux'ta HOSTNAME ya da
/// /etc/hostname (ağ taramasında hangi PC olduğu görünsün diye).
fn host_name() -> String {
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        return name;
    }
    if let Ok(name) = std::env::var("HOSTNAME") {
        return name;
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Keşif ucu: TV/tarayıcı istemcisi ağı tarayıp bu yanıtı arar.
/// CORS başlığı şart — istemci farklı IP'lere (farklı origin) fetch atar.
async fn ping() -> impl IntoResponse {
    (
        [(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")],
        axum::Json(serde_json::json!({
            "app": "mirror-host",
            "name": host_name(),
        })),
    )
}

// Tek-exe dağıtımı: tv-app dosyaları derleme anında gömülür; diskte tv-app
// klasörü yoksa (exe başka makineye tek başına kopyalandıysa) bellekten servis edilir.
const EMBED_INDEX: &str = include_str!("../../tv-app/index.html");
const EMBED_CSS: &str = include_str!("../../tv-app/css/style.css");
const EMBED_JS: &str = include_str!("../../tv-app/js/app.js");

pub async fn serve(
    state: AppState,
    bind: SocketAddr,
    web_root: PathBuf,
    managed: bool,
) -> Result<()> {
    let app = if web_root.exists() {
        Router::new()
            .route("/ws", get(ws_handler))
            .route("/ping", get(ping))
            .route("/clock", get(clock))
            .fallback_service(ServeDir::new(web_root))
            .with_state(state)
    } else {
        tracing::info!("tv-app klasörü yok; gömülü istemci servis ediliyor");
        use axum::http::header;
        use axum::response::Html;
        Router::new()
            .route("/ws", get(ws_handler))
            .route("/ping", get(ping))
            .route("/clock", get(clock))
            .route("/", get(|| async { Html(EMBED_INDEX) }))
            .route(
                "/css/style.css",
                get(|| async { ([(header::CONTENT_TYPE, "text/css")], EMBED_CSS) }),
            )
            .route(
                "/js/app.js",
                get(|| async { ([(header::CONTENT_TYPE, "application/javascript")], EMBED_JS) }),
            )
            .with_state(state)
    };

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("Sunucu hazır: http://{bind}/  (TV veya herhangi bir tarayıcıdan açın)");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        if managed {
            // GUI panelince yönetim: stdin kapanınca (panel "Durdur" dedi) ya da
            // Ctrl+C ile düzgün kapan — ses aygıtı/monitör geri alma çalışsın.
            let stdin_eof = tokio::task::spawn_blocking(|| {
                use std::io::Read;
                let mut buf = [0u8; 256];
                let mut stdin = std::io::stdin();
                while matches!(stdin.read(&mut buf), Ok(n) if n > 0) {}
            });
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate_signal() => {},
                _ = stdin_eof => {},
            }
        } else {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate_signal() => {},
            }
        }
        tracing::info!("Kapatılıyor…");
    })
    .await?;
    Ok(())
}

async fn ws_handler(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| crate::session::run(socket, state, peer))
}
