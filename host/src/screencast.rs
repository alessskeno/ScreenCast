//! Linux ekran yakalama oturumu: GNOME/Mutter'ın `org.gnome.Mutter.ScreenCast`
//! D-Bus arayüzü.
//!
//! Neden portal değil de doğrudan Mutter: xdg-desktop-portal her başlatmada
//! kullanıcıya "hangi ekranı paylaşacaksın?" penceresi açar (TV'den yayın
//! başlatmak için kullanışsız) ve sanal monitör oluşturamaz. Mutter'ın kendi
//! arayüzü (gnome-remote-desktop'ın kullandığı) her ikisini de verir:
//!   - `RecordMonitor` → var olan bir monitörü aynala
//!   - `RecordVirtual` → GERÇEK sanal ikinci monitör (Windows'taki parsec-vdd'nin
//!     karşılığı; `--extend` bununla çalışır)
//!
//! Yakalama D-Bus BAĞLANTISI YAŞADIĞI SÜRECE ayaktadır — bağlantı kapanınca
//! Mutter oturumu ve sanal monitörü kendiliğinden söker (hayalet monitör kalmaz,
//! Windows'taki keepalive'ın karşılığı ama bedava).
//!
//! TUZAK (ölçüldü): sanal monitörde `pipewiresrc`'ın caps'ine `framerate` YAZMA —
//! Mutter "no more input formats" ile pazarlığı reddeder ve monitör hiç oluşmaz.
//! Yalnız width/height ver; kare hızını aşağıda `videorate` ile sınırla.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use tracing::info;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{proxy, Connection};

use crate::engine::Rect;

/// `a{sv}` özellik sözlüğü.
type Props<'a> = HashMap<&'a str, Value<'a>>;

// GetCurrentState imzası: ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv}
type ModeInfo = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type MonitorRaw = ((String, String, String, String), Vec<ModeInfo>, HashMap<String, OwnedValue>);
type LogicalRaw = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, OwnedValue>,
);
type CurrentState = (u32, Vec<MonitorRaw>, Vec<LogicalRaw>, HashMap<String, OwnedValue>);

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast",
    default_service = "org.gnome.Mutter.ScreenCast",
    default_path = "/org/gnome/Mutter/ScreenCast"
)]
trait ScreenCast {
    fn create_session(&self, properties: Props<'_>) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Session",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait Session {
    fn start(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn record_monitor(
        &self,
        connector: &str,
        properties: Props<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn record_virtual(&self, properties: Props<'_>) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Stream",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait Stream {
    #[zbus(signal)]
    fn pipe_wire_stream_added(&self, node_id: u32) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    fn get_current_state(&self) -> zbus::Result<CurrentState>;
}

/// Bilgisayardaki bir monitör (Mutter'ın mantıksal yerleşimine göre).
#[derive(Clone, Debug)]
pub struct MonitorInfo {
    /// Bağlantı adı, örn. "HDMI-1" — RecordMonitor bunu ister.
    pub connector: String,
    pub rect: Rect,
    pub primary: bool,
}

impl MonitorInfo {
    pub fn width(&self) -> u32 {
        self.rect.width() as u32
    }
    pub fn height(&self) -> u32 {
        self.rect.height() as u32
    }
}

/// Mutter'ın bildirdiği monitörleri, mantıksal konumlarıyla listeler.
fn parse_state(state: CurrentState) -> Vec<MonitorInfo> {
    let (_serial, monitors, logicals, _props) = state;
    let mut out = Vec::new();
    for (x, y, scale, _transform, primary, members, _lprops) in logicals {
        for (connector, _v, _p, _s) in members {
            // Bu bağlantının geçerli modundan piksel boyutunu al.
            let size = monitors.iter().find(|m| m.0 .0 == connector).and_then(|m| {
                m.1.iter()
                    .find(|md| {
                        md.6.get("is-current")
                            .and_then(|v| bool::try_from(v.try_clone().ok()?).ok())
                            .unwrap_or(false)
                    })
                    .map(|md| (md.1, md.2))
            });
            let (w, h) = size.unwrap_or((1920, 1080));
            // Mantıksal boyut = piksel / ölçek (kesirli ölçekte imleç için doğru olan bu).
            let scale = if scale > 0.0 { scale } else { 1.0 };
            let lw = (w as f64 / scale).round() as i32;
            let lh = (h as f64 / scale).round() as i32;
            out.push(MonitorInfo {
                connector,
                rect: Rect { left: x, top: y, right: x + lw, bottom: y + lh },
                primary,
            });
        }
    }
    out
}

/// Monitörleri listeler (async — yayın tarafı için).
pub async fn list_monitors() -> Result<Vec<MonitorInfo>> {
    let conn = Connection::session().await.context("D-Bus oturum veri yolu yok")?;
    let dc = DisplayConfigProxy::new(&conn)
        .await
        .context("org.gnome.Mutter.DisplayConfig bulunamadı (GNOME çalışmıyor olabilir)")?;
    Ok(parse_state(dc.get_current_state().await?))
}

/// Monitörleri listeler (bloklu — GUI paneli için; tokio çalışma zamanı yok).
pub fn list_monitors_blocking() -> Result<Vec<MonitorInfo>> {
    let conn = zbus::blocking::Connection::session().context("D-Bus oturum veri yolu yok")?;
    let dc = DisplayConfigProxyBlocking::new(&conn)
        .context("org.gnome.Mutter.DisplayConfig bulunamadı")?;
    Ok(parse_state(dc.get_current_state()?))
}

/// Canlı bir yakalama oturumu. DÜŞÜRÜLDÜĞÜNDE (drop) Mutter oturumu kapanır,
/// sanal monitör varsa sökülür — bu yüzden yayın boyunca hayatta tutulmalı.
pub struct Capture {
    // Sıra önemli: bağlantı en sonda düşmeli (oturum ona bağlı).
    session: SessionProxy<'static>,
    _conn: Connection,
    /// PipeWire düğüm kimliği — GStreamer `pipewiresrc path=<id>` ile bağlanır.
    pub node_id: u32,
    /// Sanal monitörde ŞART: caps'e bu boyut yazılır, monitör bu boyutta oluşur.
    pub size: Option<(u32, u32)>,
    /// Yakalanan alanın masaüstündeki dikdörtgeni (imleç normalizasyonu için).
    pub rect: Rect,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Nazikçe kapat; bağlantı zaten düşse de Mutter oturumu temizler.
        let session = self.session.clone();
        // Zaten bir çalışma zamanındaysak görev olarak, değilsek sessizce geç.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = session.stop().await;
            });
        }
    }
}

/// İmleç kipi: 0=gizli, 1=videoya gömülü, 2=ayrı meta veri.
/// Linux'ta 1 kullanılır — Wayland'da imlecin küresel konumunu okumanın
/// güvenli bir yolu yok (Windows'taki GetCursorInfo'nun karşılığı yok),
/// bu yüzden imleç videoya gömülür ve istemci kendi imlecini çizmez.
const CURSOR_EMBEDDED: u32 = 1;

async fn open_session(conn: &Connection) -> Result<SessionProxy<'static>> {
    let sc = ScreenCastProxy::new(conn).await.context(
        "org.gnome.Mutter.ScreenCast bulunamadı — GNOME (Mutter) oturumu gerekli.\n\
         X11 oturumundaysanız `--capture x11` ile deneyin.",
    )?;
    let path = sc.create_session(HashMap::new()).await.context("ScreenCast oturumu açılamadı")?;
    SessionProxy::builder(conn)
        .path(path)?
        .build()
        .await
        .context("oturum nesnesine bağlanılamadı")
}

/// Yayın başlar ve PipeWire düğüm kimliği döner.
async fn start_and_wait(
    conn: &Connection,
    session: &SessionProxy<'static>,
    stream_path: OwnedObjectPath,
) -> Result<u32> {
    let stream = StreamProxy::builder(conn).path(stream_path)?.build().await?;
    // Sinyale ÖNCE abone ol, sonra Start çağır — yoksa sinyali kaçırabiliriz.
    let mut added = stream.receive_pipe_wire_stream_added().await?;
    session.start().await.context("ScreenCast oturumu başlatılamadı")?;
    let msg = tokio::time::timeout(Duration::from_secs(10), added.next())
        .await
        .context("PipeWireStreamAdded sinyali 10 sn içinde gelmedi")?
        .context("sinyal akışı beklenmedik şekilde kapandı")?;
    Ok(msg.args()?.node_id)
}

/// Var olan bir monitörü aynalar.
pub async fn mirror(connector: Option<&str>) -> Result<Capture> {
    let conn = Connection::session().await.context("D-Bus oturum veri yolu yok")?;
    let monitors = {
        let dc = DisplayConfigProxy::new(&conn).await?;
        parse_state(dc.get_current_state().await?)
    };
    if monitors.is_empty() {
        bail!("Mutter hiç monitör bildirmedi");
    }
    let chosen = match connector {
        Some(name) => monitors
            .iter()
            .find(|m| m.connector == name)
            .with_context(|| {
                format!(
                    "'{name}' adlı monitör yok. Mevcut olanlar: {}",
                    monitors.iter().map(|m| m.connector.as_str()).collect::<Vec<_>>().join(", ")
                )
            })?
            .clone(),
        None => monitors.iter().find(|m| m.primary).unwrap_or(&monitors[0]).clone(),
    };

    let session = open_session(&conn).await?;
    let mut props: Props = HashMap::new();
    props.insert("cursor-mode", Value::U32(CURSOR_EMBEDDED));
    let stream_path = session
        .record_monitor(&chosen.connector, props)
        .await
        .context("RecordMonitor başarısız")?;
    let node_id = start_and_wait(&conn, &session, stream_path).await?;
    info!(
        "Ekran yakalama: {} ({}x{}) → PipeWire düğüm {}",
        chosen.connector,
        chosen.width(),
        chosen.height(),
        node_id
    );
    Ok(Capture {
        session,
        _conn: conn,
        node_id,
        size: None,
        rect: chosen.rect,
    })
}

/// GERÇEK sanal ikinci monitör oluşturur ve onu yakalar (`--extend`).
///
/// Monitör, GStreamer caps pazarlığı tamamlanınca (yani yayın gerçekten
/// başlayınca) belirir ve oturum kapanınca kendiliğinden gider.
pub async fn extend(width: u32, height: u32) -> Result<Capture> {
    let conn = Connection::session().await.context("D-Bus oturum veri yolu yok")?;
    let session = open_session(&conn).await?;
    let mut props: Props = HashMap::new();
    props.insert("cursor-mode", Value::U32(CURSOR_EMBEDDED));
    let stream_path = session.record_virtual(props).await.context(
        "RecordVirtual başarısız — sanal monitör için GNOME 43+ (Mutter ScreenCast v3+) gerekir",
    )?;
    let node_id = start_and_wait(&conn, &session, stream_path).await?;
    info!("Sanal monitör: {width}x{height} → PipeWire düğüm {node_id}");
    Ok(Capture {
        session,
        _conn: conn,
        node_id,
        size: Some((width, height)),
        // Sanal monitör masaüstünün sağına eklenir; imleç Linux'ta videoya
        // gömülü olduğu için tam konumun önemi yok.
        rect: Rect { left: 0, top: 0, right: width as i32, bottom: height as i32 },
    })
}
