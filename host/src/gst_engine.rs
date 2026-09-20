//! Linux yakalama+kodlama motoru: GStreamer alt süreci.
//!
//! Windows'taki `ffmpeg_engine` ile AYNI desen: `gst-launch-1.0` alt süreç olarak
//! çalışır, stdout'una ham Annex-B H.264 basar; AU'lara bölüp broadcast'e veren
//! ortak kod `engine.rs`'tedir. WebRTC/sinyalleşme tarafı platformu hiç bilmez.
//!
//! Neden ffmpeg değil: bu makinedeki ffmpeg (8.1.2) PipeWire'dan okuyamıyor
//! (`pipewiregrab` filtresi yok, `-devices` listesinde pipewire yok) ve Wayland'da
//! ekran yakalamanın tek meşru yolu PipeWire. GStreamer'ın `pipewiresrc`'ı bunu
//! yapar; X11 oturumlarında ise aynı boru hattı `ximagesrc` ile beslenir.
//!
//! Kodlayıcı otomatik seçilir: NVENC → VA-API (Intel/AMD) → x264 (yazılım).
//! Yanlış özellik adı verilirse gst-launch anında ölür ve sıradaki kodlayıcı
//! denenir — yani liste "iyimser" tutulabilir.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::engine::{
    probe_alive, spawn_stream_process, Attempt, EncodedFrame, PipelineConfig, PipelineHandles, Rect,
};

/// Denenecek GStreamer kodlayıcıları, tercih sırasıyla.
const ENCODERS: &[&str] = &[
    "nvh264enc",   // NVIDIA (CUDA)
    "vah264enc",   // Intel/AMD VA-API
    "vah264lpenc", // VA-API düşük güç (Intel)
    "x264enc",     // yazılım yedeği
];

/// Yakalama kaynağı.
pub enum Source {
    /// Wayland/GNOME: Mutter ScreenCast'ın açtığı PipeWire düğümü.
    PipeWire {
        node_id: u32,
        /// Yalnız SANAL monitörde dolu: caps'e yazılacak boyut.
        /// (Aynalamada None — boyutu Mutter dayatır.)
        size: Option<(u32, u32)>,
    },
    /// X11 oturumu: doğrudan kök pencereden yakalama.
    /// `rect` None ise tüm masaüstü alınır (X11'de monitör listesi çıkarmıyoruz).
    X11 { display: String, rect: Option<Rect> },
}

impl Source {
    /// Kaynak elemanının ve hemen ardından gelen caps süzgecinin belirteçleri.
    fn tokens(&self, fps: u32) -> Vec<String> {
        match self {
            Source::PipeWire { node_id, size } => {
                let mut v = vec![
                    "pipewiresrc".into(),
                    format!("path={node_id}"),
                    "do-timestamp=true".into(),
                    "keepalive-time=1000".into(),
                ];
                // TUZAK: sanal monitörde caps'e width/height ŞART (Mutter monitörü
                // bu boyutta oluşturur), ama framerate YAZILMAMALI — yazılırsa
                // pazarlık "no more input formats" ile düşer ve monitör hiç oluşmaz.
                if let Some((w, h)) = size {
                    v.push("!".into());
                    v.push(format!("video/x-raw,width={w},height={h}"));
                }
                v
            }
            Source::X11 { display, rect } => {
                let mut v = vec![
                    "ximagesrc".into(),
                    format!("display-name={display}"),
                    "use-damage=false".into(),
                    // İmleç Linux'ta videoya gömülür (bkz. cursor_linux.rs).
                    "show-pointer=true".into(),
                ];
                if let Some(r) = rect {
                    v.push(format!("startx={}", r.left));
                    v.push(format!("starty={}", r.top));
                    v.push(format!("endx={}", r.right - 1));
                    v.push(format!("endy={}", r.bottom - 1));
                }
                v.push("!".into());
                v.push(format!("video/x-raw,framerate={fps}/1"));
                v
            }
        }
    }
}

/// Kodlayıcının çıkış caps'inde zorlanacak H.264 profili.
///
/// ÖLÇÜLDÜ: `nvh264enc` serbest bırakılınca **Constrained Baseline** üretiyor
/// (CABAC yok → aynı bit hızında gözle görülür daha kötü görüntü). Windows'taki
/// ffmpeg h264_nvenc varsayılan olarak High veriyor; eşitlemek için caps'te
/// açıkça istiyoruz. VA-API kodlayıcılarında zorlamıyoruz — düşük güç (LP)
/// kipinde High desteklenmeyebilir ve pazarlık düşerse kodlayıcı boşuna elenir.
fn encoder_profile(name: &str) -> Option<&'static str> {
    match name {
        "nvh264enc" | "x264enc" | "vah264enc" => Some("high"),
        _ => None,
    }
}

/// Kodlayıcıya özgü özellikler. Bit hızı her yerde kbit/s.
fn encoder_args(name: &str, kbps: u32, gop: u32) -> Vec<String> {
    match name {
        "nvh264enc" => vec![
            format!("bitrate={kbps}"),
            format!("max-bitrate={kbps}"),
            format!("gop-size={gop}"),
            "aud=true".into(),
            "zerolatency=true".into(),
            "rc-mode=cbr".into(),
            "preset=p1".into(),
            "tune=ultra-low-latency".into(),
        ],
        "vah264enc" | "vah264lpenc" => vec![
            format!("bitrate={kbps}"),
            format!("key-int-max={gop}"),
            "aud=true".into(),
            "rate-control=cbr".into(),
            "target-usage=7".into(),
        ],
        // x264: ultrafast + zerolatency (B kare yok, lookahead yok)
        "x264enc" => vec![
            format!("bitrate={kbps}"),
            format!("key-int-max={gop}"),
            "aud=true".into(),
            "speed-preset=ultrafast".into(),
            "tune=zerolatency".into(),
            "b-adapt=false".into(),
            "bframes=0".into(),
        ],
        _ => vec![format!("bitrate={kbps}")],
    }
}

/// Kodlayıcı bu makinede kullanılabilir mi?
///
/// İki koşul: eleman kurulu OLMALI ve `aud` özelliği BULUNMALI. AUD (erişim
/// birimi ayracı) şart, çünkü `engine.rs` akışı karelere AUD'lere bakarak
/// bölüyor — ayraç üretmeyen bir kodlayıcı sessizce bozuk akış verirdi.
/// Eksik olanı denemek yerine baştan eliyoruz; log da nedenini söylüyor.
fn encoder_usable(name: &str) -> bool {
    let out = match Command::new("gst-inspect-1.0").arg(name).stderr(Stdio::null()).output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return false,
    };
    // gst-inspect satırı: "  aud                 : Use AU (Access Unit) delimiter"
    let has_aud = String::from_utf8_lossy(&out)
        .lines()
        .any(|l| l.trim_start().starts_with("aud "));
    if !has_aud {
        warn!("{name} atlandı: 'aud' (erişim birimi ayracı) özelliği yok");
    }
    has_aud
}

/// gst-launch-1.0 kurulu mu?
pub fn available() -> bool {
    Command::new("gst-launch-1.0")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn start(cfg: PipelineConfig, src: Source) -> Result<PipelineHandles> {
    if !available() {
        anyhow::bail!(
            "gst-launch-1.0 bulunamadı. Kurulum:\n  \
             Arch/CachyOS: sudo pacman -S gstreamer gst-plugins-base gst-plugins-good \
             gst-plugin-pipewire gst-plugins-bad gst-plugins-ugly\n  \
             Ubuntu: sudo apt install gstreamer1.0-tools gstreamer1.0-plugins-base \
             gstreamer1.0-plugins-good gstreamer1.0-pipewire gstreamer1.0-plugins-bad \
             gstreamer1.0-plugins-ugly"
        );
    }

    // Bildirilen boyut: TV/WebRTC tarafına yazılan genişlik/yükseklik.
    // Wayland kesirli ölçek (ör. 1536x864) standart değil → kodlamada 1080p'ye
    // çekilir; aksi halde bazı TV çözücüleri 2x2/siyah ekran gösterir (ölçüldü).
    let (raw_w, raw_h) = match &src {
        Source::PipeWire { size: Some((w, h)), .. } => (*w, *h),
        Source::X11 { rect: Some(r), .. } => (r.width() as u32, r.height() as u32),
        _ => cfg.capture_size.unwrap_or((1920, 1080)),
    };
    let (width, height) = encode_target(raw_w, raw_h, false);
    if (width, height) != (raw_w, raw_h) {
        warn!("Kaynak {raw_w}x{raw_h} standart değil → kodlama {width}x{height}");
    }

    let (encoded_tx, _) = broadcast::channel::<Arc<EncodedFrame>>(240);
    // gst-launch alt sürecine anlık IDR zorlatamayız (ffmpeg'de de öyle);
    // 1 sn'lik GOP telafi eder — yeni izleyici en geç 1 sn'de görüntü alır.
    let keyframe_request = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let candidates: Vec<&&str> = ENCODERS.iter().filter(|n| encoder_usable(n)).collect();
    if candidates.is_empty() {
        anyhow::bail!(
            "Hiçbir H.264 kodlayıcı elemanı bulunamadı (nvh264enc/vah264enc/x264enc).\n\
             Arch/CachyOS: sudo pacman -S gst-plugins-ugly gst-plugins-bad\n\
             Ubuntu: sudo apt install gstreamer1.0-plugins-ugly gstreamer1.0-plugins-bad"
        );
    }

    for name in candidates {
        let frames = Arc::new(AtomicU64::new(0));
        let attempt = match spawn_attempt(
            &cfg,
            &src,
            name,
            encoded_tx.clone(),
            stop.clone(),
            frames.clone(),
            false,
            "gst",
        ) {
            Ok(a) => a,
            Err(e) => {
                warn!("{name} başlatılamadı: {e:#}");
                continue;
            }
        };
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
        // 1080p akış (Windows'takiyle aynı gerekçe: TV 4K'da 8-9 fps'e düşüyor).
        // Aynı PipeWire düğümüne ikinci bir tüketici bağlanır.
        let mut lite_tx = None;
        if height > 1200 {
            let (ltx, _) = broadcast::channel::<Arc<EncodedFrame>>(240);
            let mut lcfg = cfg.clone();
            lcfg.bitrate_bps = if cfg.fps >= 60 { 12_000_000 } else { 8_000_000 };
            let lframes = Arc::new(AtomicU64::new(0));
            match spawn_attempt(
                &lcfg,
                &src,
                name,
                ltx.clone(),
                stop.clone(),
                lframes.clone(),
                true,
                "gst-1080p",
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
    anyhow::bail!("Hiçbir H.264 kodlayıcı çalışmadı (GStreamer)")
}

#[allow(clippy::too_many_arguments)]
fn spawn_attempt(
    cfg: &PipelineConfig,
    src: &Source,
    encoder: &str,
    tx: broadcast::Sender<Arc<EncodedFrame>>,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    lite: bool,
    label: &'static str,
) -> Result<Attempt> {
    let fps = cfg.fps.max(1);
    let kbps = (cfg.bitrate_bps / 1000).max(500);
    let gop = fps; // 1 sn GOP

    // -q: gst-launch'ın kendi mesajları stdout'a karışmasın (orada H.264 var).
    let mut args: Vec<String> = vec!["-q".into()];
    args.extend(src.tokens(fps));

    // SABİT kare hızı (CFR) — hem tavan hem TABAN.
    //
    // Tavan gerekli: 144Hz panelde her kareyi kodlamak GPU'yu boğuyor (Windows
    // dersi). Taban da gerekli ve bu LINUX'A ÖZGÜ: PipeWire/Mutter kareyi
    // yalnız ekran DEĞİŞİNCE üretir. Hareketsiz ekranda kare akışı ~1 fps'e
    // düşer ve `gop-size` KARE cinsinden olduğu için anahtar kare aralığı
    // saniyelerce uzar — ÖLÇÜLDÜ: boş sanal ekranda 33 sn'de 57 kare ve
    // SIFIR anahtar kare. İstemci anahtar kare gelmeden çizmediği için
    // sonuç SİYAH EKRAN olur (yaşandı).
    //
    // `drop-only` KULLANMA: videorate'in eksik kareleri çoğaltmasını engeller.
    // Çoğaltılan kareler değişmediği için neredeyse sıfır bit tutar, ama
    // sayaç ilerlediğinden anahtar kare tam 1 sn'de bir gelir.
    // (Windows'taki ddagrab de zaten sabit hızda üretiyor — davranış eşitlendi.)
    args.extend(["!", "videorate"].iter().map(|s| s.to_string()));
    args.push("!".into());
    args.push(format!("video/x-raw,framerate={fps}/1"));

    // Ölçekleme: hafif akış VEYA kesirli/standart-dışı kaynak (1536x864 → 1080p).
    let (raw_w, raw_h) = source_raw_size(src, cfg);
    let (ew, eh) = encode_target(raw_w, raw_h, lite);
    // Her zaman videoscale: (1) kesirli ölçek → 1080p, (2) sanal monitör
    // dma-buf/modifier'lı BGRx → sistem belleğine kopya. Ölçüldü: aynalama
    // videoscale ile düzeldi, extend aynı boyutta kalsın diye scale yokken TV
    // hâlâ 2x2/siyah görebiliyor (donanım çözücü + boş Meta-0).
    let need_scale = true;
    let _ = (raw_w, raw_h); // log için start() zaten uyarıyor
    args.push("!".into());
    if need_scale {
        args.push("videoscale".into());
        args.push("n-threads=4".into());
        args.push("method=0".into()); // en yakın komşu / hızlı
        args.push("!".into());
    }
    args.push("videoconvert".into());
    args.push("n-threads=4".into());
    args.push("!".into());
    args.push(format!("video/x-raw,format=NV12,width={ew},height={eh}"));

    args.push("!".into());
    args.push(encoder.to_string());
    args.extend(encoder_args(encoder, kbps, gop));

    // sync=false: canlı akışta saate göre bekleme = boşuna gecikme.
    let mut caps = "video/x-h264,stream-format=byte-stream,alignment=au".to_string();
    if let Some(profile) = encoder_profile(encoder) {
        caps.push_str(&format!(",profile={profile}"));
    }
    args.push("!".into());
    args.push(caps);
    args.extend(["!", "fdsink", "fd=1", "sync=false"].iter().map(|s| s.to_string()));

    let mut cmd = Command::new("gst-launch-1.0");
    cmd.args(&args);
    info!("{label} denemesi: {encoder}");
    tracing::debug!("gst boru hattı: {}", args.join(" "));
    spawn_stream_process(cmd, cfg.fps, tx, stop, frames, label, &format!("gst[{encoder}]"))
}

/// TV/tarayıcı dostu kodlama boyutu. Kesirli Wayland ölçeği (1536x864 vb.)
/// standart 720p/1080p/2K/4K dışındaysa 1920x1080'e çekilir.
fn encode_target(w: u32, h: u32, lite: bool) -> (u32, u32) {
    if lite {
        return (1920, 1080);
    }
    match (w, h) {
        (1920, 1080) | (2560, 1440) | (3840, 2160) | (1280, 720) => (w, h),
        _ => (1920, 1080),
    }
}

fn source_raw_size(src: &Source, cfg: &PipelineConfig) -> (u32, u32) {
    match src {
        Source::PipeWire { size: Some((w, h)), .. } => (*w, *h),
        Source::X11 { rect: Some(r), .. } => (r.width() as u32, r.height() as u32),
        _ => cfg.capture_size.unwrap_or((1920, 1080)),
    }
}
