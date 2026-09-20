//! Faz 1.5: ffmpeg tabanlı yakalama+kodlama motoru (Windows).
//!
//! Neden: NVIDIA'nın Media Foundation sarmalayıcısı ProcessInput'ta kare başına
//! ~25ms senkron bekliyor (ölçüldü) → ~15-20 fps tavanı. ffmpeg ise ddagrab
//! (ekran yakalama) + donanım H.264 ile aynı makinede 60 fps veriyor
//! (10 sn'lik kıyas testiyle doğrulandı, 2026-08-30).
//!
//! ffmpeg alt süreç olarak çalışır, stdout'una ham Annex-B H.264 basar;
//! akışı erişim birimlerine bölüp broadcast'e veren ORTAK kod `engine.rs`'tedir
//! (Linux/GStreamer motoru da aynısını kullanır). WebRTC, imleç ve sinyalleşme
//! tarafı hiç değişmez.
//!
//! Taşınabilirlik: kodlayıcı otomatik seçilir (NVENC → Intel QSV → AMD AMF →
//! yazılım libx264) ve ffmpeg.exe exe'ye gömülü gelir (build.rs + assets/).
//! Yani tek exe, her Windows makinede kendi kendine yeter.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::broadcast;
use tracing::{info, warn};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::engine::{
    probe_alive, spawn_stream_process, Attempt, EncodedFrame, PipelineConfig, PipelineHandles,
};

/// Denenecek kodlayıcılar, tercih sırasıyla; yanında kodlayıcıya özgü argümanlar.
/// AU ayracı hepsinde `h264_metadata` bit akışı filtresiyle eklenir (evrensel).
const ENCODERS: &[(&str, &[&str])] = &[
    ("h264_nvenc", &["-preset", "p1", "-tune", "ll", "-delay", "0", "-forced-idr", "1"]),
    ("h264_qsv", &["-preset", "veryfast"]),
    ("h264_amf", &["-usage", "lowlatency"]),
    ("libx264", &["-preset", "ultrafast", "-tune", "zerolatency"]),
];

/// ffmpeg.exe'yi bul: PATH → winget linki → winget paket klasörü → gömülü kopya.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let on_path = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if on_path {
        return Some(PathBuf::from("ffmpeg"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        let link = local.join(r"Microsoft\WinGet\Links\ffmpeg.exe");
        if link.exists() {
            return Some(link);
        }
        let packages = local.join(r"Microsoft\WinGet\Packages");
        if let Ok(entries) = std::fs::read_dir(packages) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("Gyan.FFmpeg") {
                    if let Ok(subs) = std::fs::read_dir(entry.path()) {
                        for sub in subs.flatten() {
                            let cand = sub.path().join("bin").join("ffmpeg.exe");
                            if cand.exists() {
                                return Some(cand);
                            }
                        }
                    }
                }
            }
        }
    }
    embedded_ffmpeg()
}

/// Exe'ye gömülü ffmpeg'i %LOCALAPPDATA%\mirror-host\ altına çıkarır (bir kez;
/// boyut aynıysa tekrar yazmaz). Tek exe dağıtımının anahtarı budur.
#[cfg(embedded_ffmpeg)]
fn embedded_ffmpeg() -> Option<PathBuf> {
    static BYTES: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/ffmpeg.exe"));
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("mirror-host");
    let path = dir.join("ffmpeg.exe");
    let cached = std::fs::metadata(&path)
        .map(|m| m.len() == BYTES.len() as u64)
        .unwrap_or(false);
    if !cached {
        std::fs::create_dir_all(&dir).ok()?;
        if std::fs::write(&path, BYTES).is_err() && !path.exists() {
            return None;
        }
        info!("Gömülü ffmpeg çıkarıldı: {}", path.display());
    }
    Some(path)
}

#[cfg(not(embedded_ffmpeg))]
fn embedded_ffmpeg() -> Option<PathBuf> {
    None
}

pub fn start(cfg: PipelineConfig, ffmpeg: PathBuf) -> Result<PipelineHandles> {
    // Yakalanan ekran boyutu: --extend bunu geçirir; yoksa birincil monitör.
    let (width, height) = cfg.capture_size.unwrap_or_else(|| unsafe {
        (GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32)
    });

    let (encoded_tx, _) = broadcast::channel::<Arc<EncodedFrame>>(240);
    let keyframe_request = Arc::new(AtomicBool::new(false)); // ffmpeg'e IDR zorlatamayız; 1 sn GOP telafi eder
    let stop = Arc::new(AtomicBool::new(false));

    for (name, extra) in ENCODERS {
        let frames = Arc::new(AtomicU64::new(0));
        let attempt = match spawn_attempt(
            &ffmpeg,
            &cfg,
            name,
            extra,
            encoded_tx.clone(),
            stop.clone(),
            frames.clone(),
            false,
            "ffmpeg",
        ) {
            Ok(a) => a,
            Err(e) => {
                warn!("{name} başlatılamadı: {e:#}");
                continue;
            }
        };
        // Ölçüt: donanım desteklenmiyorsa ffmpeg ~1 sn'de ölür. Süreç 3 sn hayatta
        // kaldıysa kodlayıcı çalışıyor demektir — kare beklemek YANILTIR, çünkü
        // masaüstü tamamen hareketsizken ilk kare hiç gelmeyebilir.
        if !probe_alive(&frames, &attempt.dead) {
            warn!("{name} çalışmadı; sıradaki kodlayıcı denenecek");
            attempt.abort.store(true, Ordering::SeqCst);
            continue;
        }
        info!(
            "Kodlayıcı seçildi: {name} ({width}x{height} @ {} fps, {} Mb/s)",
            cfg.fps,
            cfg.bitrate_bps / 1_000_000
        );

        // Kaynak 1080p'den büyükse TV gibi zayıf çözücüler için ikinci, hafif
        // bir 1080p akışı üret (ölçüldü: TV 4K'da 8-9 fps'e düşüyor, 1080p'de 60).
        // Küçültme CPU'da fast_bilinear ile (bu ffmpeg d3d11→cuda köprüsünü
        // desteklemiyor); 4K60 kaynakta ~55 fps ölçüldü — fazlasıyla yeterli.
        let mut lite_tx = None;
        if height > 1200 {
            let (ltx, _) = broadcast::channel::<Arc<EncodedFrame>>(240);
            let mut lcfg = cfg.clone();
            lcfg.bitrate_bps = if cfg.fps >= 60 { 12_000_000 } else { 8_000_000 };
            let lframes = Arc::new(AtomicU64::new(0));
            match spawn_attempt(
                &ffmpeg,
                &lcfg,
                name,
                extra,
                ltx.clone(),
                stop.clone(),
                lframes.clone(),
                true,
                "ffmpeg-1080p",
            ) {
                Ok(la) => {
                    if probe_alive(&lframes, &la.dead) {
                        info!(
                            "Hafif akış hazır: 1920x1080 @ {} Mb/s (TV bunu alacak)",
                            lcfg.bitrate_bps / 1_000_000
                        );
                        lite_tx = Some(ltx);
                    } else {
                        warn!("Hafif 1080p akışı başlatılamadı; TV tam akışı alacak");
                        la.abort.store(true, Ordering::SeqCst);
                    }
                }
                Err(e) => warn!("Hafif akış başlatılamadı: {e:#}"),
            }
        }

        return Ok(PipelineHandles {
            encoded_tx,
            lite_tx,
            keyframe_request,
            stop,
            encoder_dead: attempt.dead.clone(),
            width,
            height,
        });
    }
    anyhow::bail!("Hiçbir H.264 kodlayıcı çalışmadı (ffmpeg: {})", ffmpeg.display())
}

/// Verilen kodlayıcıyla bir ffmpeg denemesi başlatır (iş parçacıklarını
/// `engine::spawn_stream_process` kurar). Dönen `abort` bu denemeyi tekil öldürür.
#[allow(clippy::too_many_arguments)]
fn spawn_attempt(
    ffmpeg: &PathBuf,
    cfg: &PipelineConfig,
    encoder: &str,
    extra: &[&str],
    tx: broadcast::Sender<Arc<EncodedFrame>>,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    lite: bool,
    label: &'static str,
) -> Result<Attempt> {
    let filter = format!(
        "ddagrab=output_idx={}:framerate={}:draw_mouse=0",
        cfg.output_index, cfg.fps
    );
    let bitrate = cfg.bitrate_bps.to_string();
    let bufsize = (cfg.bitrate_bps / 2).to_string();
    let gop = cfg.fps.to_string(); // 1 sn GOP: yeni izleyici en geç 1 sn'de bağlanır

    let mut args: Vec<String> =
        ["-hide_banner", "-loglevel", "warning"].iter().map(|s| s.to_string()).collect();
    // Sanal monitör kendi adaptöründe olabilir: ddagrab'a doğru cihazı ver,
    // kareyi CPU'ya indirip kodlayıcıya sistem belleğinden besle
    // (adaptörler arası doku paylaşımından kaçınmanın basit ve sağlam yolu).
    if cfg.adapter_index > 0 {
        args.extend(
            ["-init_hw_device", &format!("d3d11va=dd:{}", cfg.adapter_index), "-filter_hw_device", "dd"]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    args.extend(["-f", "lavfi", "-i", &filter, "-c:v", encoder].iter().map(|s| s.to_string()));
    args.extend(extra.iter().map(|s| s.to_string()));
    if lite {
        // Hafif akış: CPU'da hızlı küçültme (fast_bilinear ŞART — varsayılan
        // bicubic 4K60'ı 38fps'e düşürüyor, fast_bilinear 55fps; ölçüldü).
        args.extend(
            ["-vf", "hwdownload,format=bgra,scale=1920:-2:flags=fast_bilinear,format=bgr0"]
                .iter()
                .map(|s| s.to_string()),
        );
    } else if cfg.adapter_index > 0 {
        args.extend(["-vf", "hwdownload,format=bgra,format=bgr0"].iter().map(|s| s.to_string()));
    }
    args.extend(
        [
            "-bf", "0",
            "-g", &gop,
            "-b:v", &bitrate,
            "-maxrate", &bitrate,
            "-bufsize", &bufsize,
            "-bsf:v", "h264_metadata=aud=insert",
            "-f", "h264",
            "-",
        ]
        .iter()
        .map(|s| s.to_string()),
    );

    let mut cmd = Command::new(ffmpeg);
    cmd.args(&args);
    info!("{label} denemesi: {encoder}");
    spawn_stream_process(cmd, cfg.fps, tx, stop, frames, label, &format!("ffmpeg[{encoder}]"))
}
