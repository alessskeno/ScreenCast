//! Kablosuz ikinci ekran — yayın ucu.
//!
//! Windows boru hattı:
//!   DXGI Desktop Duplication / ffmpeg ddagrab → donanım H.264 → WebRTC → TV
//! Linux boru hattı:
//!   Mutter ScreenCast (PipeWire) veya X11 → GStreamer donanım H.264 → WebRTC → TV
//!
//! İmleç Windows'ta videoya gömülmez; konumu ayrı bir veri kanalından gider
//! (RDP tekniği) — böylece video gecikmesinden etkilenmez. Linux'ta Wayland
//! küresel imleç konumunu vermediği için imleç videoya gömülür (bkz. cursor_linux).

mod audio;
mod engine;
mod gui;
mod protocol;
mod session;
mod signaling;

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod convert;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod ffmpeg_engine;
#[cfg(windows)]
mod focus_follow;
#[cfg(windows)]
mod pipeline;
#[cfg(windows)]
mod vdd;

#[cfg(target_os = "linux")]
mod gst_engine;
#[cfg(target_os = "linux")]
mod screencast;

// Platforma göre aynı modül adı altında farklı uygulama.
#[cfg(windows)]
#[path = "cursor_win.rs"]
mod cursor;
#[cfg(target_os = "linux")]
#[path = "cursor_linux.rs"]
mod cursor;

#[cfg(windows)]
#[path = "audio_route_win.rs"]
mod audio_route;
#[cfg(target_os = "linux")]
#[path = "audio_route_linux.rs"]
mod audio_route;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::engine::{PipelineConfig, PipelineHandles, Rect};

/// Kablosuz ikinci ekran — yayın ucu.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Dinlenecek adres:port
    #[arg(long, default_value = "0.0.0.0:47000")]
    bind: SocketAddr,

    /// Kare hızı (60 = akıcı imleç/pencere hareketi)
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Video bit hızı, Mbit/s (12 Mb/s @ 1080p60: netlik/gecikme dengesi)
    #[arg(long, default_value_t = 12)]
    bitrate: u32,

    /// Anahtar kare aralığı, saniye (uzun GOP = aynı kalitede daha az bit;
    /// yeni izleyiciye zaten anında anahtar kare zorlanır)
    #[arg(long, default_value_t = 4)]
    gop: u32,

    /// Yakalanacak monitör dizini (0 = birincil)
    #[arg(long, default_value_t = 0)]
    output: u32,

    /// TV istemcisinin statik dosya kökü
    #[arg(long, default_value = "tv-app")]
    web_root: PathBuf,

    /// Yakalama+kodlama motoru: auto | ffmpeg | mf (yalnız Windows;
    /// Linux'ta daima GStreamer kullanılır)
    #[arg(long, default_value = "auto")]
    engine: String,

    /// (Linux) Ekran yakalama yolu: auto | gnome | x11
    /// (gnome: Mutter ScreenCast/PipeWire — Wayland'ın tek meşru yolu ve
    /// sanal monitörü de bu sağlar; x11: doğrudan X kök penceresinden)
    #[cfg(target_os = "linux")]
    #[arg(long, default_value = "auto")]
    capture: String,

    /// Sistem sesini aktarma (kapatmak için --no-audio)
    #[arg(long)]
    no_audio: bool,

    /// TV'yi GERÇEK ikinci monitör yap: sanal ekran takılır ve o yayınlanır.
    /// (Win+P "Genişlet" gibi — mouse kenardan TV'ye geçer.
    /// Windows'ta parsec-vdd sürücüsü, Linux'ta GNOME/Mutter gerekir.)
    #[arg(long)]
    extend: bool,

    /// Sesi YALNIZ TV'den çal: varsayılan çıkışı sanal aygıta çevirir,
    /// PC hoparlörü susar; çıkışta eski aygıt geri gelir (HDMI TV davranışı).
    #[arg(long)]
    tv_audio: bool,

    /// Takılı kalmış ses yönlendirmesini düzeltip çık (çökme sonrası ilk yardım)
    #[arg(long)]
    restore_audio: bool,

    /// Sanal monitör modu, örn. 3840x2160@60 veya 2560x1440 (yalnız --extend ile)
    #[arg(long)]
    mode: Option<String>,

    /// Alt+Tab ile odak başka monitördeki pencereye geçince imleci oraya ışınla
    /// (yalnız Windows; KDE/GNOME'da bu davranış zaten var)
    #[arg(long)]
    cursor_follow: bool,

    /// (dahili) GUI panelince yönetiliyor: stdin kapanınca düzgün kapan
    #[arg(long, hide = true)]
    managed: bool,
}

fn parse_mode(s: &str) -> Result<(u32, u32, u32)> {
    let (res, hz) = s.split_once('@').unwrap_or((s, "60"));
    let (w, h) = res
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("mod biçimi: GENxYÜKSEKLİK@Hz, örn. 3840x2160@60"))?;
    Ok((w.trim().parse()?, h.trim().parse()?, hz.trim().parse()?))
}

fn main() -> Result<()> {
    // Argümansız (çift tıklama) veya --gui ile açılış → kontrol paneli.
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() <= 1 || argv.iter().any(|a| a == "--gui") {
        return gui::run();
    }
    cli_main()
}

#[tokio::main]
async fn cli_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    platform_init();

    if args.restore_audio {
        return audio_route::restore_cli();
    }

    let cfg = PipelineConfig {
        fps: args.fps,
        bitrate_bps: args.bitrate.saturating_mul(1_000_000),
        gop_frames: args.gop.saturating_mul(args.fps),
        output_index: args.output,
        adapter_index: 0,
        capture_size: None,
    };

    // Platforma özgü yakalama kurulumu; `_guard` yakalama oturumunu (Windows'ta
    // sanal monitör keepalive'ı, Linux'ta Mutter ScreenCast oturumu) hayatta tutar.
    let Started { pipeline, cursor_rect, _guard } = start_capture(&args, cfg).await?;

    let rect = cursor_rect.unwrap_or(Rect {
        left: 0,
        top: 0,
        right: pipeline.width as i32,
        bottom: pipeline.height as i32,
    });
    let cursor_rx = cursor::spawn(rect);

    if args.cursor_follow {
        #[cfg(windows)]
        focus_follow::spawn();
        #[cfg(not(windows))]
        tracing::info!("--cursor-follow yalnız Windows'ta gerekli; yok sayıldı");
    }

    // --tv-audio: yakalama başlamadan ÖNCE varsayılanı sanal aygıta çevir ki
    // loopback yeni varsayılanı dinlesin; PC hoparlörü sussun.
    let audio_route = if args.tv_audio && !args.no_audio {
        match audio_route::engage() {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!("TV'ye özel ses açılamadı, ortak ses sürüyor: {e:#}");
                None
            }
        }
    } else {
        None
    };

    // Ses: başarısız olursa None döner, video sessiz devam eder.
    let audio_tx = if args.no_audio { None } else { audio::start(pipeline.stop.clone()) };

    let state = signaling::AppState {
        encoded_tx: pipeline.encoded_tx.clone(),
        lite_tx: pipeline.lite_tx.clone(),
        keyframe_request: pipeline.keyframe_request.clone(),
        cursor_rx,
        audio_tx,
    };

    let result = signaling::serve(state, args.bind, args.web_root, args.managed).await;
    pipeline.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(route) = audio_route {
        route.restore();
    }
    result
}

/// Yakalama kurulumunun sonucu.
struct Started {
    pipeline: PipelineHandles,
    /// İmlecin normalize edileceği masaüstü dikdörtgeni (biliniyorsa).
    cursor_rect: Option<Rect>,
    /// Yayın boyunca yaşaması gereken platform tutamağı.
    _guard: Guard,
}

// ------------------------------------------------------------------ Windows

#[cfg(windows)]
fn platform_init() {
    // DPI farkındalığı: yakalama boyutları ve imleç koordinatları gerçek piksel olsun.
    // COM: ses yönlendirme (audio_route) ana iş parçacığında COM ister.
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
}

/// Windows'ta sanal monitörü yaşatan keepalive bayrağı; düşünce monitör sökülür.
#[cfg(windows)]
struct Guard(std::sync::Arc<std::sync::atomic::AtomicBool>);

#[cfg(windows)]
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(windows)]
async fn start_capture(args: &Args, mut cfg: PipelineConfig) -> Result<Started> {
    let vdd_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut extend_rect = None;
    if args.extend {
        let mut out = vdd::attach_virtual_display(vdd_stop.clone())?;
        // İstenen çözünürlük/tazeleme moduna geç, konum/boyutu tazele.
        if let Some(mode) = &args.mode {
            let (w, h, hz) = parse_mode(mode)?;
            vdd::set_display_mode(&out.device_name, w, h, hz)?;
            std::thread::sleep(std::time::Duration::from_millis(500));
            if let Some(updated) = vdd::find_output_by_name(&out.device_name) {
                out = updated;
            }
            tracing::info!("Sanal monitör modu: {w}x{h}@{hz}");
        }
        cfg.output_index = out.output_index;
        cfg.adapter_index = out.adapter_index;
        cfg.capture_size = Some((out.rect.width() as u32, out.rect.height() as u32));
        extend_rect = Some(out.rect);
    } else if args.mode.is_some() {
        tracing::warn!("--mode yalnız --extend ile anlamlıdır; yok sayıldı");
    }

    let pipeline = match args.engine.as_str() {
        "mf" => pipeline::start(cfg)?,
        "ffmpeg" => {
            let ff = ffmpeg_engine::find_ffmpeg()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg bulunamadı (winget install Gyan.FFmpeg)"))?;
            ffmpeg_engine::start(cfg, ff)?
        }
        _ => match ffmpeg_engine::find_ffmpeg() {
            Some(ff) => ffmpeg_engine::start(cfg, ff)?,
            None => {
                if args.extend {
                    anyhow::bail!("--extend için ffmpeg motoru gerekli ama ffmpeg bulunamadı");
                }
                tracing::warn!("ffmpeg yok; Media Foundation motoruna düşülüyor (~15fps)");
                pipeline::start(cfg)?
            }
        },
    };

    let cursor_rect = extend_rect.or_else(|| cursor::output_rect(args.output));
    Ok(Started { pipeline, cursor_rect, _guard: Guard(vdd_stop) })
}

// -------------------------------------------------------------------- Linux

#[cfg(target_os = "linux")]
fn platform_init() {}

/// Linux'ta Mutter ScreenCast oturumu; düşünce yakalama ve varsa sanal monitör gider.
#[cfg(target_os = "linux")]
struct Guard(#[allow(dead_code)] Option<screencast::Capture>);

#[cfg(target_os = "linux")]
async fn start_capture(args: &Args, mut cfg: PipelineConfig) -> Result<Started> {
    if args.engine == "mf" {
        anyhow::bail!("--engine mf yalnız Windows'ta vardır; Linux'ta GStreamer kullanılır");
    }
    if args.engine == "ffmpeg" {
        tracing::warn!(
            "Linux'ta ffmpeg motoru yok (ffmpeg PipeWire'dan okuyamıyor); GStreamer kullanılıyor"
        );
    }

    let wayland = std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE").map(|s| s == "wayland").unwrap_or(false);
    let mode = match args.capture.as_str() {
        "auto" => {
            if wayland {
                "gnome"
            } else {
                "x11"
            }
        }
        other => other,
    };

    match mode {
        "gnome" => {
            let cap = if args.extend {
                let (w, h, _hz) = match &args.mode {
                    Some(m) => parse_mode(m)?,
                    None => (1920, 1080, 60),
                };
                cfg.capture_size = Some((w, h));
                screencast::extend(w, h).await?
            } else {
                let monitors = screencast::list_monitors().await?;
                let chosen = monitors.get(args.output as usize).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--output {} yok; bu makinede {} monitör var: {}",
                        args.output,
                        monitors.len(),
                        monitors
                            .iter()
                            .enumerate()
                            .map(|(i, m)| format!("{i}={}", m.connector))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
                cfg.capture_size = Some((chosen.width(), chosen.height()));
                if args.mode.is_some() {
                    tracing::warn!("--mode yalnız --extend ile anlamlıdır; yok sayıldı");
                }
                screencast::mirror(Some(&chosen.connector)).await?
            };
            let src = gst_engine::Source::PipeWire { node_id: cap.node_id, size: cap.size };
            let rect = cap.rect;
            let pipeline = gst_engine::start(cfg, src)?;
            Ok(Started { pipeline, cursor_rect: Some(rect), _guard: Guard(Some(cap)) })
        }
        "x11" => {
            if args.extend {
                anyhow::bail!(
                    "X11 oturumunda sanal monitör (--extend) desteklenmiyor.\n\
                     GNOME Wayland oturumuna geçin (oturum açma ekranında dişli → GNOME),\n\
                     ya da --extend olmadan aynalama yapın."
                );
            }
            let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
            tracing::warn!(
                "X11 yakalama: tüm masaüstü yakalanır, --output yok sayılır \
                 (çoklu monitörde GNOME Wayland oturumu önerilir)"
            );
            let src = gst_engine::Source::X11 { display, rect: None };
            let pipeline = gst_engine::start(cfg, src)?;
            Ok(Started { pipeline, cursor_rect: None, _guard: Guard(None) })
        }
        other => anyhow::bail!("bilinmeyen --capture değeri: {other} (auto | gnome | x11)"),
    }
}
