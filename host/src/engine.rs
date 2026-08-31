//! Platformdan bağımsız motor çekirdeği.
//!
//! Windows (ffmpeg + ddagrab) ve Linux (GStreamer + PipeWire/X11) motorlarının
//! ORTAK kısmı burada: yapılandırma, kodlanmış kare tipi ve "alt süreç stdout'undan
//! Annex-B H.264 oku → erişim birimlerine (AU) böl → broadcast'e ver" mantığı.
//!
//! Her iki motor da aynı desende çalışır: bir alt süreç ham Annex-B akışı stdout'a
//! basar, biz AUD'lere (NAL tipi 9) göre bölüp WebRTC tarafına aynı kanaldan veririz.
//! Böylece oturum/sinyalleşme/imleç katmanı platformu hiç bilmez.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::broadcast;
use tracing::{debug, error, info};

/// Kodlanmış tek bir video karesi (Annex-B, tam erişim birimi).
pub struct EncodedFrame {
    pub data: Bytes,
    pub is_keyframe: bool,
    /// Nominal kare süresi (1/fps) — gerçek süre bilinmiyorsa yedek.
    pub duration: Duration,
    /// Karenin gerçek yakalama zamanı (100ns). RTP zaman damgası bununla
    /// ilerletilmeli; yoksa atlanan karelerde alıcı saati geri kalır.
    pub ts_100ns: i64,
}

/// Ekran dikdörtgeni (Windows RECT ile aynı alanlar, platformdan bağımsız).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        (self.right - self.left).max(1)
    }
    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(1)
    }
}

#[derive(Clone)]
pub struct PipelineConfig {
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Anahtar kare aralığı (kare). Yalnız Windows'un Media Foundation yolu
    /// okur; alt süreç tabanlı motorlar 1 sn'lik sabit GOP kullanır.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub gop_frames: u32,
    /// Yakalanacak çıkışın dizini. Linux'ta yakalama kurulurken monitör
    /// bağlantı adına çevrildiği için motora kadar taşınmaz.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub output_index: u32,
    /// Çıkışın bağlı olduğu DXGI adaptörü (0 = ana GPU; sanal monitör
    /// kendi adaptöründe olabilir — yalnız ffmpeg motoru destekler).
    /// Linux'ta kullanılmaz.
    #[cfg_attr(not(windows), allow(dead_code))]
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

/// Alt süreç tabanlı bir kodlayıcı denemesinin tutamakları.
pub struct Attempt {
    /// true yapılırsa YALNIZ bu deneme öldürülür (sıradaki kodlayıcıya geçmek için).
    pub abort: Arc<AtomicBool>,
    /// Alt süreç kendiliğinden öldüyse true olur (kodlayıcı desteklenmiyor demektir).
    pub dead: Arc<AtomicBool>,
}

/// Deneme başarılı mı: ilk kare geldiyse ya da süreç 3 sn hayatta kaldıysa evet.
///
/// Kare beklemek YANILTIR — masaüstü tamamen hareketsizken ilk kare hiç gelmeyebilir
/// ve çalışan bir kodlayıcı yanlışlıkla elenir (Windows'ta NVENC'te yaşandı).
pub fn probe_alive(frames: &Arc<AtomicU64>, dead: &Arc<AtomicBool>) -> bool {
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

/// Alt sürecin, ebeveyn NASIL ölürse ölsün (SIGKILL, panik, çökme) kendisinin de
/// ölmesini garantiler.
///
/// NEDEN GEREKLİ (yaşandı): bekçi iş parçacığı alt süreci yalnız düzgün kapanışta
/// öldürüyordu; süreç aniden ölünce ya da bekçi 300 ms'lik uykusundayken çıkılınca
/// gst-launch öksüz kalıp SONSUZA DEK çalışmaya devam etti. Ölçüldü: 9 kaçak
/// gst-launch, ~1.5 GB GPU belleği ve tükenmiş NVENC oturumları → sonraki yayın
/// "Failed to open session" alıp yazılım kodlayıcıya düşüyordu.
///
/// Not: boru kırılması (EPIPE) tek başına yetmiyor — hareketsiz ekranda alt süreç
/// çok seyrek yazdığı için kopmayı fark etmesi dakikalar alabiliyor.
#[cfg(target_os = "linux")]
pub fn die_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id();
    unsafe {
        cmd.pre_exec(move || {
            // Ebeveyn ölünce çekirdek bu sürece SIGKILL yollasın.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Yarış: ebeveyn fork ile prctl arasında ölmüş olabilir; o durumda
            // sinyal hiç gelmez. Kontrol et, geç kaldıysak hemen çık.
            if libc::getppid() as u32 != parent {
                libc::_exit(0);
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub fn die_with_parent(_cmd: &mut Command) {}

/// Verilen komutu (ffmpeg ya da gst-launch) çalıştırır ve üç iş parçacığı kurar:
/// stderr → log, bekçi (durdurma/ölüm), okuyucu (Annex-B → AU → broadcast).
///
/// Komut stdout'una ham Annex-B H.264 basmalı ve erişim birimi ayracı (AUD)
/// üretmelidir (ffmpeg: `-bsf:v h264_metadata=aud=insert`, gst: `aud=true`).
pub fn spawn_stream_process(
    mut cmd: Command,
    fps: u32,
    tx: broadcast::Sender<Arc<EncodedFrame>>,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    label: &'static str,
    tag: &str,
) -> Result<Attempt> {
    die_with_parent(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("{label} başlatılamadı"))?;

    let abort = Arc::new(AtomicBool::new(false));
    let dead = Arc::new(AtomicBool::new(false));

    if let Some(stderr) = child.stderr.take() {
        let tag = tag.to_string();
        std::thread::Builder::new().name("enc-err".into()).spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                debug!("{tag}: {line}");
            }
        })?;
    }
    let stdout = child.stdout.take().context("alt süreç stdout yok")?;

    spawn_watchdog(child, stop.clone(), abort.clone(), dead.clone(), tag.to_string())?;
    spawn_au_reader(stdout, tx, stop, abort.clone(), frames, fps, label)?;

    Ok(Attempt { abort, dead })
}

/// Bekçi: durdurma/iptal gelince alt süreci öldürür; kendiliğinden ölürse işaretler.
fn spawn_watchdog(
    mut child: Child,
    stop: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    tag: String,
) -> Result<()> {
    std::thread::Builder::new().name("enc-watch".into()).spawn(move || loop {
        if stop.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                debug!("{tag} kapandı: {status}");
                dead.store(true, Ordering::SeqCst);
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(300)),
            Err(_) => return,
        }
    })?;
    Ok(())
}

/// Okuyucu: Annex-B akışını AU'lara böler ve broadcast kanalına yayınlar.
fn spawn_au_reader(
    mut stdout: impl Read + Send + 'static,
    tx: broadcast::Sender<Arc<EncodedFrame>>,
    stop: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    fps: u32,
    label: &'static str,
) -> Result<()> {
    let nominal = Duration::from_nanos(1_000_000_000u64 / fps.max(1) as u64);
    std::thread::Builder::new().name("enc-read".into()).spawn(move || {
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
                    debug!("{label} akışı bitti");
                    return;
                }
                Ok(n) => n,
                Err(e) => {
                    error!("{label} okuma hatası: {e}");
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
    Ok(())
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
                    if j > 0 && au[j - 1] == 0 {
                        j - 1
                    } else {
                        j
                    }
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
