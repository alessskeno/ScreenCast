//! Faz 1.5: ffmpeg tabanlı yakalama+kodlama motoru.
//!
//! Neden: NVIDIA'nın Media Foundation sarmalayıcısı ProcessInput'ta kare başına
//! ~25ms senkron bekliyor (ölçüldü) → ~15-20 fps tavanı. ffmpeg ise ddagrab
//! (ekran yakalama) + donanım H.264 ile aynı makinede 60 fps veriyor
//! (10 sn'lik kıyas testiyle doğrulandı, 2026-08-30).
//!
//! ffmpeg alt süreç olarak çalışır, stdout'una ham Annex-B H.264 basar;
//! biz akışı erişim birimlerine (AU) bölüp aynı broadcast kanalına veririz.
//! WebRTC, imleç ve sinyalleşme tarafı hiç değişmez.
//!
//! Taşınabilirlik: kodlayıcı otomatik seçilir (NVENC → Intel QSV → AMD AMF →
//! yazılım libx264) ve ffmpeg.exe exe'ye gömülü gelir (build.rs + assets/).
//! Yani tek exe, her Windows makinede kendi kendine yeter.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::encoder::EncodedFrame;
use crate::pipeline::{PipelineConfig, PipelineHandles};

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
        let (abort, dead) = match spawn_attempt(&ffmpeg, &cfg, name, extra, encoded_tx.clone(), stop.clone(), frames.clone(), false, "ffmpeg") {
            Ok(pair) => pair,
            Err(e) => {
                warn!("{name} başlatılamadı: {e:#}");
                continue;
            }
        };
        // Ölçüt: donanım desteklenmiyorsa ffmpeg ~1 sn'de ölür. Süreç 3 sn hayatta
        // kaldıysa kodlayıcı çalışıyor demektir — kare beklemek YANILTIR, çünkü
        // masaüstü tamamen hareketsizken ilk kare hiç gelmeyebilir.
        if !probe_alive(&frames, &dead) {
            warn!("{name} çalışmadı; sıradaki kodlayıcı denenecek");
            abort.store(true, Ordering::SeqCst);
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
            match spawn_attempt(&ffmpeg, &lcfg, name, extra, ltx.clone(), stop.clone(), lframes.clone(), true, "ffmpeg-1080p") {
                Ok((labort, ldead)) => {
                    if probe_alive(&lframes, &ldead) {
                        info!(
                            "Hafif akış hazır: 1920x1080 @ {} Mb/s (TV bunu alacak)",
                            lcfg.bitrate_bps / 1_000_000
                        );
                        lite_tx = Some(ltx);
                    } else {
                        warn!("Hafif 1080p akışı başlatılamadı; TV tam akışı alacak");
                        labort.store(true, Ordering::SeqCst);
                    }
                }
                Err(e) => warn!("Hafif akış başlatılamadı: {e:#}"),
            }
        }

        return Ok(PipelineHandles { encoded_tx, lite_tx, keyframe_request, stop, width, height });
    }
    anyhow::bail!("Hiçbir H.264 kodlayıcı çalışmadı (ffmpeg: {})", ffmpeg.display())
}

/// Deneme başarılı mı: ilk kare geldiyse ya da süreç 3 sn hayatta kaldıysa evet.
fn probe_alive(frames: &Arc<AtomicU64>, dead: &Arc<AtomicBool>) -> bool {
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(250));
        if frames.load(Ordering::Relaxed) > 0 {
            return true;
        }
        if dead.load(Ordering::Relaxed) {
            return false;
        }
    }
    !dead.load(Ordering::Relaxed)
}

/// Verilen kodlayıcıyla bir ffmpeg denemesi başlatır; stderr/bekçi/okuyucu iş
/// parçacıklarını kurar. Dönen `abort` bayrağı bu denemeyi tekil öldürür.
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
) -> Result<(Arc<AtomicBool>, Arc<AtomicBool>)> {
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

    let mut child = Command::new(ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("ffmpeg başlatılamadı")?;
    info!("{label} denemesi: {encoder}");

    let abort = Arc::new(AtomicBool::new(false));
    let dead = Arc::new(AtomicBool::new(false));

    // stderr → log
    if let Some(stderr) = child.stderr.take() {
        let enc = encoder.to_string();
        std::thread::Builder::new().name("ffmpeg-err".into()).spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                debug!("ffmpeg[{enc}]: {line}");
            }
        })?;
    }

    let mut stdout = child.stdout.take().context("ffmpeg stdout yok")?;

    // Bekçi: durdurma/iptal gelince alt süreci öldür; kendiliğinden ölürse logla.
    {
        let stop = stop.clone();
        let abort = abort.clone();
        let dead = dead.clone();
        let enc = encoder.to_string();
        std::thread::Builder::new().name("ffmpeg-watch".into()).spawn(move || {
            loop {
                if stop.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    return;
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        debug!("ffmpeg[{enc}] kapandı: {status}");
                        dead.store(true, Ordering::SeqCst);
                        return;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(300)),
                    Err(_) => return,
                }
            }
        })?;
    }

    // Okuyucu: Annex-B akışını AU'lara böl, yayınla.
    {
        let stop = stop.clone();
        let abort = abort.clone();
        let nominal = Duration::from_nanos(1_000_000_000u64 / cfg.fps.max(1) as u64);
        std::thread::Builder::new().name("ffmpeg-read".into()).spawn(move || {
            let t0 = Instant::now();
            let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
            let mut chunk = [0u8; 65536];
            let mut sps_pps: Vec<u8> = Vec::new();
            let mut n_bytes = 0u64;
            let mut n_frames_log = 0u64;
            let mut last_log = Instant::now();

            loop {
                if stop.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed) {
                    return;
                }
                let n = match stdout.read(&mut chunk) {
                    Ok(0) => {
                        debug!("ffmpeg akışı bitti");
                        return;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        error!("ffmpeg okuma hatası: {e}");
                        return;
                    }
                };
                buf.extend_from_slice(&chunk[..n]);

                // AUD (tip 9) konumlarına göre böl: ardışık iki AUD arası bir AU'dur.
                let auds = find_aud_positions(&buf);
                if auds.len() >= 2 {
                    for w in auds.windows(2) {
                        if let Some(size) = emit_au(&buf[w[0]..w[1]], &mut sps_pps, &tx, t0, nominal) {
                            frames.fetch_add(1, Ordering::Relaxed);
                            n_frames_log += 1;
                            n_bytes += size as u64;
                        }
                    }
                    buf.drain(..*auds.last().unwrap());
                }

                if last_log.elapsed() >= Duration::from_secs(3) {
                    let secs = last_log.elapsed().as_secs_f64();
                    info!(
                        "Boru hattı ({label}): kodlama {:.0} fps | {:.1} Mb/s",
                        n_frames_log as f64 / secs,
                        n_bytes as f64 * 8.0 / secs / 1e6
                    );
                    n_frames_log = 0;
                    n_bytes = 0;
                    last_log = Instant::now();
                }
            }
        })?;
    }

    Ok((abort, dead))
}

/// Tampondaki AUD (erişim birimi ayracı, NAL tipi 9) başlangıç konumlarını bulur.
fn find_aud_positions(buf: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            if buf[i + 3] & 0x1f == 9 {
                // 4 baytlık başlangıç kodu (00 00 00 01) kullanılmışsa onu da kapsa
                out.push(if i > 0 && buf[i - 1] == 0 { i - 1 } else { i });
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Bir AU'yu inceler: SPS/PPS önbelleğini günceller, anahtar kareye gerekirse
/// SPS/PPS ekler ve kanala yayınlar. Yayınlanan bayt sayısını döndürür.
fn emit_au(
    au: &[u8],
    sps_pps: &mut Vec<u8>,
    tx: &broadcast::Sender<Arc<EncodedFrame>>,
    t0: Instant,
    nominal: Duration,
) -> Option<usize> {
    let mut has_idr = false;
    let mut has_sps = false;
    let mut new_sps_pps: Vec<u8> = Vec::new();

    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let ty = au[i + 3] & 0x1f;
            match ty {
                5 => has_idr = true,
                7 => has_sps = true,
                _ => {}
            }
            if ty == 7 || ty == 8 {
                // Bu NAL'ın sonunu (sıradaki başlangıç kodunu) bul, önbelleğe al.
                let start = if i > 0 && au[i - 1] == 0 { i - 1 } else { i };
                let mut j = i + 3;
                while j + 3 < au.len() && !(au[j] == 0 && au[j + 1] == 0 && au[j + 2] == 1) {
                    j += 1;
                }
                let end = if j + 3 < au.len() {
                    if j > 0 && au[j - 1] == 0 { j - 1 } else { j }
                } else {
                    au.len()
                };
                new_sps_pps.extend_from_slice(&au[start..end]);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    if !new_sps_pps.is_empty() {
        *sps_pps = new_sps_pps;
    }

    // Geç katılan izleyicinin çözücüsü için: anahtar karede SPS/PPS yoksa başına ekle.
    let data = if has_idr && !has_sps && !sps_pps.is_empty() {
        let mut v = Vec::with_capacity(sps_pps.len() + au.len());
        v.extend_from_slice(sps_pps);
        v.extend_from_slice(au);
        Bytes::from(v)
    } else {
        Bytes::copy_from_slice(au)
    };

    let size = data.len();
    let _ = tx.send(Arc::new(EncodedFrame {
        data,
        is_keyframe: has_idr,
        duration: nominal,
        ts_100ns: (t0.elapsed().as_nanos() / 100) as i64,
    }));
    Some(size)
}
