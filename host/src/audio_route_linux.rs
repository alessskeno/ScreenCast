//! "TV'ye özel ses" (`--tv-audio`) — Linux tarafı (PipeWire / PulseAudio).
//!
//! Sorun Windows'takiyle aynı: loopback sesin KOPYASINI alır, ses PC
//! hoparlöründen de çalar. Çözüm de aynı fikirde — yayın sırasında varsayılan
//! çıkışı sessiz bir sanal aygıta çevir; uygulamalar oraya çalsın, monitörü
//! yakalansın, TV'den duyulsun, PC sussun. Çıkışta eski aygıt geri gelir.
//!
//! Windows'a göre BÜYÜK avantaj: sanal aygıt için üçüncü parti sürücü (VB-CABLE)
//! kurmak gerekmez — PipeWire/PulseAudio `module-null-sink`'i çalışma anında
//! oluşturur, admin/UAC istemez ve çıkışta tamamen kaybolur.
//!
//! `audio.rs` kaynak olarak `@DEFAULT_MONITOR@` kullandığı için varsayılan
//! değişince yakalama kendiliğinden yeni aygıtı dinler; burada ayrıca
//! bir şey bağlamaya gerek yoktur.

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

/// Oluşturulan sanal çıkışın adı (pactl'de bu adla görünür).
const SINK_NAME: &str = "mirror_tv";

/// Çökme sonrası ilk yardım için önceki varsayılanın yazıldığı dosya.
fn restore_file() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Some(dir.join("mirror-host-audio-restore"))
}

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("pactl çalıştırılamadı (Arch: libpulse, Ubuntu: pulseaudio-utils)")?;
    if !out.status.success() {
        bail!(
            "pactl {} başarısız: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Sanal çıkış oluşturulabilir mi? (Linux'ta PipeWire/Pulse varsa daima evet —
/// panelde "sürücü kur" uyarısı çıkmasın diye bu bilgi kullanılır.)
pub fn virtual_sink_available() -> bool {
    Command::new("pactl")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct AudioRoute {
    /// `module-null-sink` modül kimliği (kaldırmak için).
    module_id: String,
    /// Yayından önceki varsayılan çıkış.
    previous_sink: String,
    pub target_name: String,
}

pub fn engage() -> Result<AudioRoute> {
    if !virtual_sink_available() {
        bail!("pactl yok — PipeWire/PulseAudio çalışmıyor olabilir");
    }
    let previous_sink = pactl(&["get-default-sink"])?;
    if previous_sink == SINK_NAME {
        // Önceki çalışmadan kalmış olabilir; önce temizle.
        warn!("Varsayılan çıkış zaten sanal aygıt; eski yönlendirme temizleniyor");
        let _ = restore_cli();
    }

    // Sessiz sanal çıkış oluştur. Aygıt açıklaması kullanıcıya ses ayarlarında
    // "PC Mirror (TV)" olarak görünür.
    let module_id = pactl(&[
        "load-module",
        "module-null-sink",
        &format!("sink_name={SINK_NAME}"),
        "sink_properties=device.description='PC_Mirror_(TV)'",
    ])
    .context("sanal ses çıkışı oluşturulamadı")?;

    // Varsayılanı çevir; sonra ZATEN ÇALAN akışları da taşı (yoksa mevcut
    // uygulamalar eski hoparlörden çalmaya devam eder — Windows'ta bu sorun yok).
    if let Err(e) = pactl(&["set-default-sink", SINK_NAME]) {
        let _ = pactl(&["unload-module", &module_id]);
        return Err(e).context("varsayılan çıkış değiştirilemedi");
    }
    move_streams_to(SINK_NAME);

    if let Some(p) = restore_file() {
        let _ = std::fs::write(p, format!("{module_id}\n{previous_sink}\n"));
    }
    info!("TV'ye özel ses açık: varsayılan çıkış → {SINK_NAME} (PC hoparlörü susar)");
    Ok(AudioRoute {
        module_id,
        previous_sink,
        target_name: SINK_NAME.to_string(),
    })
}

/// Çalan tüm akışları hedef çıkışa taşır.
fn move_streams_to(sink: &str) {
    let Ok(list) = pactl(&["list", "short", "sink-inputs"]) else {
        return;
    };
    for line in list.lines() {
        if let Some(id) = line.split_whitespace().next() {
            let _ = pactl(&["move-sink-input", id, sink]);
        }
    }
}

impl AudioRoute {
    pub fn restore(&self) {
        if let Err(e) = pactl(&["set-default-sink", &self.previous_sink]) {
            warn!("Varsayılan ses aygıtı geri alınamadı: {e:#}");
        }
        move_streams_to(&self.previous_sink);
        if let Err(e) = pactl(&["unload-module", &self.module_id]) {
            warn!("Sanal ses aygıtı kaldırılamadı: {e:#}");
        } else {
            info!(
                "Ses yönlendirmesi geri alındı: {} ({} kaldırıldı)",
                self.previous_sink, self.target_name
            );
        }
        if let Some(p) = restore_file() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Takılı kalmış yönlendirmeyi düzeltir (`--restore-audio`).
///
/// Not: Linux'ta sanal çıkış PipeWire'ın süreç ömrüne bağlı değildir, ama host
/// çökerse modül asılı kalabilir; bu komut onu temizler.
pub fn restore_cli() -> Result<()> {
    let saved = restore_file().and_then(|p| std::fs::read_to_string(p).ok());
    if let Some(text) = saved {
        let mut lines = text.lines();
        let module_id = lines.next().unwrap_or_default().to_string();
        let previous = lines.next().unwrap_or_default().to_string();
        if !previous.is_empty() {
            let _ = pactl(&["set-default-sink", &previous]);
            move_streams_to(&previous);
        }
        if !module_id.is_empty() {
            let _ = pactl(&["unload-module", &module_id]);
        }
        if let Some(p) = restore_file() {
            let _ = std::fs::remove_file(p);
        }
        info!("Ses yönlendirmesi geri alındı: {previous}");
        return Ok(());
    }

    // Kayıt yoksa: adına göre asılı kalmış sanal çıkışı bul ve kaldır.
    let modules = pactl(&["list", "short", "modules"]).unwrap_or_default();
    let mut cleaned = false;
    for line in modules.lines() {
        if line.contains("module-null-sink") && line.contains(SINK_NAME) {
            if let Some(id) = line.split_whitespace().next() {
                let _ = pactl(&["unload-module", id]);
                cleaned = true;
            }
        }
    }
    if cleaned {
        info!("Asılı kalmış sanal ses aygıtı kaldırıldı");
    } else {
        info!("Düzeltilecek bir ses yönlendirmesi yok");
    }
    Ok(())
}
