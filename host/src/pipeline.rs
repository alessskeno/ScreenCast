//! Yakalama → dönüştürme → kodlama boru hattı.
//!
//! İki adanmış iş parçacığı:
//! - "capture": DXGI'den kare alır, GPU'da NV12'ye çevirir, tek karelik
//!   "en tazesi kazanır" yuvasına koyar (kuyruk birikmesi = gecikme; istemeyiz).
//! - "encode": MFT olay döngüsünü çevirir, kodlanan kareleri yayın kanalına basar.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::broadcast;
use tracing::{error, info, warn};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::capture::{Capturer, Grab};
use crate::convert::{create_texture, Converter};
use crate::encoder::{EncodedFrame, Encoder};

/// Kodlayıcıya giden, GPU'da yaşayan bir NV12 kare.
pub struct FrameTex {
    pub tex: ID3D11Texture2D,
    pub ts_100ns: i64,
}
// COM işaretçisi iş parçacıkları arasında taşınır; cihaz çok iş parçacıklı korumada.
unsafe impl Send for FrameTex {}

#[derive(Clone)]
pub struct PipelineConfig {
    pub fps: u32,
    pub bitrate_bps: u32,
    pub gop_frames: u32,
    pub output_index: u32,
    /// Çıkışın bağlı olduğu DXGI adaptörü (0 = ana GPU; sanal monitör
    /// kendi adaptöründe olabilir — yalnız ffmpeg motoru destekler).
    pub adapter_index: u32,
    /// Yakalanan ekranın boyutu (biliniyorsa; log ve imleç yedeği için).
    pub capture_size: Option<(u32, u32)>,
}

pub struct PipelineHandles {
    pub encoded_tx: broadcast::Sender<Arc<EncodedFrame>>,
    /// 1080p "hafif" akış (kaynak 1080p'den büyükse; TV gibi zayıf çözücüler için).
    pub lite_tx: Option<broadcast::Sender<Arc<EncodedFrame>>>,
    pub keyframe_request: Arc<AtomicBool>,
    pub stop: Arc<AtomicBool>,
    pub width: u32,
    pub height: u32,
}

struct Slot {
    frame: Mutex<Option<FrameTex>>,
    cv: Condvar,
}

struct CaptureSide {
    device: ID3D11Device,
    capturer: Capturer,
    bgra: ID3D11Texture2D,
    converter: Converter,
    output_index: u32,
}
unsafe impl Send for CaptureSide {}

pub fn start(cfg: PipelineConfig) -> Result<PipelineHandles> {
    if cfg.adapter_index != 0 {
        anyhow::bail!("MF motoru yalnız ana adaptörü destekler; --extend için ffmpeg motoru gerekli");
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    // BGRA (masaüstü biçimi) + VIDEO (renk dönüştürücü) destekli D3D11 cihazı.
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.context("D3D11 cihazı oluşturulamadı")?;
    let context = context.context("D3D11 bağlamı oluşturulamadı")?;

    // Aynı cihaz üç taraftan kullanılacak (yakalama, dönüştürme, MF kodlayıcı):
    // dahili kilitleme şart.
    unsafe {
        let _ = context.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
    }

    let capturer = Capturer::new(&device, cfg.output_index)?;
    let (w, h) = (capturer.width, capturer.height);
    let (ew, eh) = (w & !1, h & !1); // NV12 çift boyut ister
    info!("Yakalama {w}x{h} → kodlama {ew}x{eh} @ {} fps", cfg.fps);

    let bgra = create_texture(&device, w, h, DXGI_FORMAT_B8G8R8A8_UNORM)?;
    let converter = Converter::new(&device, &bgra, (w, h), (ew, eh), cfg.fps)?;
    let encoder = Encoder::new(&device, ew, eh, cfg.fps, cfg.bitrate_bps, cfg.gop_frames)?;

    // 240 kare ≈ 4 sn tampon: izleyici takılırsa "Lagged" alır ve anahtar kareyle toparlar.
    let (encoded_tx, _) = broadcast::channel::<Arc<EncodedFrame>>(240);
    let keyframe_request = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let slot = Arc::new(Slot { frame: Mutex::new(None), cv: Condvar::new() });

    // Teşhis sayaçları: 3 sn'de bir loglanır. "Yakalama fps"i düşükse kaynak
    // masaüstünün kendisi az güncelleniyor; "kodlama fps" düşükse boru hattı sorunu.
    let stat_cap = Arc::new(AtomicU64::new(0));
    let stat_enc = Arc::new(AtomicU64::new(0));
    let stat_bytes = Arc::new(AtomicU64::new(0));

    // --- Yakalama iş parçacığı ---
    {
        let mut side = CaptureSide {
            device: device.clone(),
            capturer,
            bgra,
            converter,
            output_index: cfg.output_index,
        };
        let slot = slot.clone();
        let stop = stop.clone();
        let fps = cfg.fps.max(1);
        let cap_counter = stat_cap.clone();
        std::thread::Builder::new().name("capture".into()).spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let t0 = Instant::now();
            // Hedef fps'e sabitle: 165Hz panelde her kareyi dönüştürmek GPU'yu
            // boğar ve NVENC'i açlığa iter (ölçüldü: yakalama 150fps → kodlama 6fps).
            let interval = Duration::from_micros(1_000_000u64 / fps as u64);
            let mut last_pub = Instant::now() - interval;
            while !stop.load(Ordering::Relaxed) {
                let due = last_pub.elapsed() >= interval;
                match side.capturer.grab_into(&side.bgra, 1000 / fps, due) {
                    Ok(Grab::New) => match side.converter.convert() {
                        Ok(tex) => {
                            last_pub = Instant::now();
                            let ts = (t0.elapsed().as_nanos() / 100) as i64;
                            *slot.frame.lock().unwrap() = Some(FrameTex { tex, ts_100ns: ts });
                            slot.cv.notify_one();
                            cap_counter.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            error!("Renk dönüşümü hatası: {e:#}");
                            break;
                        }
                    },
                    Ok(Grab::NoChange) | Ok(Grab::Timeout) => {}
                    Ok(Grab::Lost) => {
                        warn!("Masaüstü erişimi koptu (mod değişimi olabilir); yakalayıcı yenileniyor");
                        loop {
                            if stop.load(Ordering::Relaxed) {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(200));
                            match Capturer::new(&side.device, side.output_index) {
                                Ok(c) => {
                                    side.capturer = c;
                                    break;
                                }
                                Err(e) => warn!("Yakalayıcı yenilenemedi: {e:#}"),
                            }
                        }
                    }
                    Err(e) => {
                        error!("Yakalama hatası: {e:#}");
                        break;
                    }
                }
            }
        })?;
    }

    // --- Kodlama iş parçacığı ---
    {
        let slot = slot.clone();
        let stop_flag = stop.clone();
        let keyframe = keyframe_request.clone();
        let tx = encoded_tx.clone();
        let enc_counter = stat_enc.clone();
        let byte_counter = stat_bytes.clone();
        std::thread::Builder::new().name("encode".into()).spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let next = || -> Option<FrameTex> {
                let mut guard = slot.frame.lock().unwrap();
                loop {
                    if stop_flag.load(Ordering::Relaxed) {
                        return None;
                    }
                    if let Some(f) = guard.take() {
                        return Some(f);
                    }
                    let (g, _) = slot
                        .cv
                        .wait_timeout(guard, Duration::from_millis(100))
                        .unwrap();
                    guard = g;
                }
            };
            let on_encoded = |f: EncodedFrame| {
                enc_counter.fetch_add(1, Ordering::Relaxed);
                byte_counter.fetch_add(f.data.len() as u64, Ordering::Relaxed);
                let _ = tx.send(Arc::new(f)); // izleyici yoksa hata döner; önemsiz
            };
            if let Err(e) = encoder.drive(next, on_encoded, &keyframe, &stop_flag) {
                error!("Kodlayıcı durdu: {e:#}");
            }
        })?;
    }

    // --- Teşhis logu iş parçacığı ---
    {
        let stop = stop.clone();
        std::thread::Builder::new().name("stats".into()).spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(3));
                let cap = stat_cap.swap(0, Ordering::Relaxed) as f64 / 3.0;
                let enc = stat_enc.swap(0, Ordering::Relaxed) as f64 / 3.0;
                let mbps = stat_bytes.swap(0, Ordering::Relaxed) as f64 * 8.0 / 3.0 / 1e6;
                info!("Boru hattı: yakalama {cap:.0} fps | kodlama {enc:.0} fps | {mbps:.1} Mb/s");
            }
        })?;
    }

    Ok(PipelineHandles { encoded_tx, lite_tx: None, keyframe_request, stop, width: ew, height: eh })
}
