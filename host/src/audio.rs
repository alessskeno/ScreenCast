//! Faz 2: Sistem sesi yakalama ve Opus kodlama.
//!
//! Yakalama platforma göre değişir, kodlama ortaktır:
//! - Windows: WASAPI loopback (cpal ile çıkış aygıtı girdi olarak açılır).
//! - Linux:   PipeWire/PulseAudio'nun varsayılan çıkış "monitör" kaynağı
//!   (`@DEFAULT_MONITOR@`) GStreamer alt süreciyle okunur.
//!
//! Ortak yol: 48 kHz stereo f32 örnekler → 20 ms'lik çerçeveler → Opus (128 kb/s)
//! → broadcast kanalı. Her WebRTC oturumu kanala abone olup kendi ses track'ine yazar.
//!
//! Ses aygıtı açılamazsa uygulama sessiz devam eder (video etkilenmez).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Kodlanmış tek bir Opus paketi (20 ms ses).
pub struct AudioFrame {
    pub data: Bytes,
    pub duration: Duration,
}

const TARGET_RATE: u32 = 48_000; // Opus'un ana örnekleme hızı
const FRAME_MS: usize = 20;
const FRAME_SAMPLES: usize = TARGET_RATE as usize / 1000 * FRAME_MS; // kanal başına 960

/// Platformdan bağımsız ham PCM kaynağı.
struct PcmSource {
    rx: mpsc::Receiver<Vec<f32>>,
    rate: u32,
    channels: usize,
    /// Kaynağı hayatta tutan tutamak (cpal akışı / gst alt süreci).
    /// Düşerse yakalama durur; bu yüzden kodlama döngüsü boyunca tutulur.
    _keep: Box<dyn std::any::Any>,
}

pub fn start(stop: Arc<AtomicBool>) -> Option<broadcast::Sender<Arc<AudioFrame>>> {
    let (tx, _) = broadcast::channel::<Arc<AudioFrame>>(256);
    let tx_out = tx.clone();
    match std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            if let Err(e) = run(tx, stop) {
                warn!("Ses yakalama devre dışı: {e:#}");
            }
        }) {
        Ok(_) => Some(tx_out),
        Err(_) => None,
    }
}

fn run(tx: broadcast::Sender<Arc<AudioFrame>>, stop: Arc<AtomicBool>) -> Result<()> {
    let src = open_source()?;
    encode_loop(src, tx, stop)
}

// ---------------------------------------------------------------- Windows kaynağı

/// WASAPI loopback: varsayılan ÇIKIŞ aygıtını girdi akışı olarak açar.
#[cfg(windows)]
fn open_source() -> Result<PcmSource> {
    use anyhow::Context;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host.default_output_device().context("varsayılan ses çıkış aygıtı yok")?;
    let config = device.default_output_config().context("ses aygıtı yapılandırması alınamadı")?;
    let rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    info!(
        "Ses: {} ({} Hz, {} kanal) → Opus 48 kHz stereo 128 kb/s",
        device.name().unwrap_or_default(),
        rate,
        channels
    );

    let (pcm_tx, rx) = mpsc::channel::<Vec<f32>>();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data: &[f32], _| {
                let _ = pcm_tx.send(data.to_vec());
            },
            |e| warn!("ses akışı hatası: {e}"),
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data: &[i16], _| {
                let _ = pcm_tx.send(data.iter().map(|&s| s as f32 / 32768.0).collect());
            },
            |e| warn!("ses akışı hatası: {e}"),
            None,
        )?,
        other => anyhow::bail!("desteklenmeyen ses örnek biçimi: {other:?}"),
    };
    stream.play().context("ses akışı başlatılamadı")?;
    Ok(PcmSource { rx, rate, channels, _keep: Box::new(stream) })
}

// ------------------------------------------------------------------ Linux kaynağı

/// Alt süreci düşürüldüğünde öldüren sarmalayıcı (yayın durunca ses de dursun).
#[cfg(target_os = "linux")]
struct ChildGuard(std::process::Child);

#[cfg(target_os = "linux")]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// PipeWire/PulseAudio monitör kaynağı: `@DEFAULT_MONITOR@` varsayılan çıkışın
/// monitörünü işaret eder ve varsayılan aygıt değişince kendiliğinden takip eder
/// (`--tv-audio` sanal çıkışa geçtiğinde de doğru kaynağı dinler).
///
/// Doğrudan 48 kHz stereo F32LE istenir; böylece kodlama döngüsündeki yeniden
/// örnekleme yolu Linux'ta hiç çalışmaz.
#[cfg(target_os = "linux")]
fn open_source() -> Result<PcmSource> {
    use anyhow::Context;
    use std::io::Read;
    use std::process::{Command, Stdio};

    let device = std::env::var("MIRROR_AUDIO_SOURCE").unwrap_or_else(|_| "@DEFAULT_MONITOR@".into());
    let mut cmd = Command::new("gst-launch-1.0");
    // Host ölürse ses alt süreci de ölsün (bkz. engine::die_with_parent).
    crate::engine::die_with_parent(&mut cmd);
    let mut child = cmd
        .args([
            "-q",
            "pulsesrc",
            &format!("device={device}"),
            "provide-clock=false",
            "do-timestamp=true",
            // ~10 ms yakalama gecikmesi: Opus çerçevesi 20 ms, daha küçüğü anlamsız.
            "latency-time=10000",
            "buffer-time=40000",
            "!",
            "audioconvert",
            "!",
            "audioresample",
            "!",
            "audio/x-raw,format=F32LE,channels=2,rate=48000,layout=interleaved",
            "!",
            "fdsink",
            "fd=1",
            "sync=false",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context(
            "gst-launch-1.0 ile ses yakalanamadı \
             (Arch: gst-plugins-good, Ubuntu: gstreamer1.0-plugins-good gerekir)",
        )?;
    let mut stdout = child.stdout.take().context("ses alt sürecinin stdout'u yok")?;
    info!("Ses: {device} (PipeWire monitör) → Opus 48 kHz stereo 128 kb/s");

    let (pcm_tx, rx) = mpsc::channel::<Vec<f32>>();
    std::thread::Builder::new().name("audio-read".into()).spawn(move || {
        // 20 ms stereo = 960 kare = 7680 bayt (tam bir Opus çerçevesi).
        //
        // `read_exact` ŞART, `read` DEĞİL: boru okuması 4'ün katı olmayan bir
        // sayıda bayt döndürebilir ve artan 1-3 bayt atılırsa akış kalıcı olarak
        // bir-iki bayt kayar — bundan sonraki TÜM örnekler bozulur (L/R yer
        // değiştirir, gürültü olur). read_exact tampon dolana kadar bekleyerek
        // hizayı kendiliğinden korur.
        let mut buf = [0u8; FRAME_SAMPLES * 2 * 4];
        loop {
            if stdout.read_exact(&mut buf).is_err() {
                return; // akış bitti ya da alt süreç öldü
            }
            let samples: Vec<f32> = buf
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            if pcm_tx.send(samples).is_err() {
                return;
            }
        }
    })?;

    Ok(PcmSource { rx, rate: TARGET_RATE, channels: 2, _keep: Box::new(ChildGuard(child)) })
}

// --------------------------------------------------------------- Ortak kodlama

/// Ham PCM'i stereo 48 kHz'e getirip 20 ms'lik Opus paketleri üretir.
fn encode_loop(
    src: PcmSource,
    tx: broadcast::Sender<Arc<AudioFrame>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    use anyhow::Context;

    let PcmSource { rx: pcm_rx, rate: src_rate, channels: src_channels, _keep } = src;

    let mut encoder =
        opus::Encoder::new(TARGET_RATE, opus::Channels::Stereo, opus::Application::Audio)
            .context("Opus kodlayıcı oluşturulamadı")?;
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(128_000));

    // Stereo'ya indirgenmiş, 48kHz'e çevrilmiş örnek kuyruğu (interleaved L,R).
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 4);
    let frame_dur = Duration::from_millis(FRAME_MS as u64);
    // Basit doğrusal yeniden örnekleme durumu (kaynak 48kHz değilse).
    let step = src_rate as f64 / TARGET_RATE as f64;
    let mut phase = 0.0f64;
    let mut prev = [0.0f32; 2];
    // Telemetri: akış durumu değişince logla (sessizlik teşhisi için).
    let mut sent_packets = 0u64;
    let mut last_report = std::time::Instant::now();
    let mut flowing = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            let now_flowing = sent_packets > 0;
            if now_flowing != flowing {
                if now_flowing {
                    info!("Ses akışı başladı ({} paket/5sn)", sent_packets);
                } else {
                    warn!("Ses aygıtından veri gelmiyor (PC'de çalan ses yok ya da aygıt sessiz)");
                }
                flowing = now_flowing;
            }
            sent_packets = 0;
            last_report = std::time::Instant::now();
        }
        let chunk = match pcm_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // Kanalları stereo'ya indir (mono → çiftle, >2 → ilk ikisi).
        let frames = chunk.len() / src_channels.max(1);
        for f in 0..frames {
            let l = chunk[f * src_channels];
            let r = if src_channels >= 2 { chunk[f * src_channels + 1] } else { l };
            if src_rate == TARGET_RATE {
                pending.push(l);
                pending.push(r);
            } else {
                // Doğrusal enterpolasyonla 48 kHz üret.
                phase += 1.0;
                while phase >= step {
                    phase -= step;
                    let t = (1.0 - phase / step) as f32;
                    pending.push(prev[0] + (l - prev[0]) * t);
                    pending.push(prev[1] + (r - prev[1]) * t);
                }
                prev = [l, r];
            }
        }
        // 20 ms'lik çerçeveler halinde kodla.
        while pending.len() >= FRAME_SAMPLES * 2 {
            let frame: Vec<f32> = pending.drain(..FRAME_SAMPLES * 2).collect();
            match encoder.encode_vec_float(&frame, 1500) {
                Ok(pkt) => {
                    sent_packets += 1;
                    let _ = tx.send(Arc::new(AudioFrame {
                        data: Bytes::from(pkt),
                        duration: frame_dur,
                    }));
                }
                Err(e) => warn!("Opus kodlama hatası: {e}"),
            }
        }
    }
    drop(_keep); // yakalama kaynağını açıkça kapat
    Ok(())
}
