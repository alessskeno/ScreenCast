//! Kablosuz ikinci ekran — Windows yayın ucu.
//!
//! Boru hattı: DXGI Desktop Duplication (GPU'da BGRA yakalama)
//!   → D3D11 Video Processor (GPU'da NV12'ye çevirme)
//!   → Media Foundation donanım H.264 kodlayıcı (düşük gecikme kipi)
//!   → WebRTC (UDP, şifreli, kayıp toleranslı) → TV / tarayıcı.
//!
//! İmleç videoya gömülmez; ayrı bir veri kanalından anlık konum olarak gider
//! (RDP'nin yaptığı gibi) — böylece imleç video gecikmesinden etkilenmez.

mod audio;
mod audio_route;
mod capture;
mod convert;
mod cursor;
mod encoder;
mod ffmpeg_engine;
mod focus_follow;
mod gui;
mod pipeline;
mod protocol;
mod session;
mod signaling;
mod vdd;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

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

    /// Yakalama+kodlama motoru: auto | ffmpeg | mf
    /// (ffmpeg: ddagrab+NVENC, ~60fps; mf: Media Foundation, NVIDIA'da ~15fps ama bağımsız)
    #[arg(long, default_value = "auto")]
    engine: String,

    /// Sistem sesini aktarma (kapatmak için --no-audio)
    #[arg(long)]
    no_audio: bool,

    /// TV'yi GERÇEK ikinci monitör yap: sanal ekran takılır ve o yayınlanır.
    /// (Win+P "Genişlet" gibi — mouse kenardan TV'ye geçer. parsec-vdd sürücüsü gerekir.)
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
    /// (KDE/GNOME davranışı; Windows'ta yerleşik yok)
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

    // DPI farkındalığı: yakalama boyutları ve imleç koordinatları gerçek piksel olsun.
    // COM: ses yönlendirme (audio_route) ana iş parçacığında COM ister.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }

    if args.restore_audio {
        return audio_route::restore_cli();
    }

    let mut cfg = pipeline::PipelineConfig {
        fps: args.fps,
        bitrate_bps: args.bitrate.saturating_mul(1_000_000),
        gop_frames: args.gop.saturating_mul(args.fps),
        output_index: args.output,
        adapter_index: 0,
        capture_size: None,
    };

    // --extend: sanal monitörü tak, yayın hedefini ve imleç alanını ona çevir.
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
        cfg.capture_size = Some((
            (out.rect.right - out.rect.left).max(1) as u32,
            (out.rect.bottom - out.rect.top).max(1) as u32,
        ));
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

    // İmleç: yakalanan monitörün gerçek dikdörtgenine göre (çoklu monitör desteği).
    let rect = extend_rect
        .or_else(|| cursor::output_rect(args.output))
        .unwrap_or(windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: pipeline.width as i32,
            bottom: pipeline.height as i32,
        });
    let cursor_rx = cursor::spawn(rect);

    if args.cursor_follow {
        focus_follow::spawn();
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
    vdd_stop.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(route) = audio_route {
        route.restore();
    }
    result
}
