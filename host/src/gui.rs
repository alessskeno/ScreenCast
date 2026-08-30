//! GUI modu (egui/eframe): exe'ye çift tıklayınca açılan kontrol paneli.
//!
//! Mimari: GUI, yayını KENDİ içinde çalıştırmaz — aynı exe'yi `--managed` ve
//! seçilen bayraklarla alt süreç olarak başlatır ve loglarını gösterir.
//! Böylece yayın çökse bile panel ayakta kalır; "Durdur" stdin'i kapatır,
//! alt süreç düzgün kapanır (ses aygıtı geri gelir, sanal monitör sökülür).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
// (etiket, --mode değeri, piksel sayısı — bitrate şablonu seçiminde kullanılır)
const MODES: &[(&str, Option<&str>, u64)] = &[
    ("Sürücü varsayılanı (1080p)", None, 1920 * 1080),
    ("1080p — 1920x1080@60", Some("1920x1080@60"), 1920 * 1080),
    ("2K — 2560x1440@60", Some("2560x1440@60"), 2560 * 1440),
    ("4K — 3840x2160@60", Some("3840x2160@60"), 3840 * 2160),
];

/// Çözünürlük + fps'e göre bit hızı şablonları (Mb/s).
/// Kaynak: Sunshine/Moonlight topluluk kılavuzları (oyun akışı için 1080p60≈20,
/// 1440p60≈35-50, 4K60≈50+); masaüstü kullanımı için alt uçları biraz indirdik.
struct Tier {
    name: &'static str,
    mbps: u32,
}

fn tiers_for(pixels: u64, fps: u32) -> [Tier; 3] {
    let base: [u32; 3] = if pixels >= 3840 * 2160 {
        [20, 35, 50]
    } else if pixels >= 2560 * 1440 {
        [12, 25, 40]
    } else {
        [8, 15, 25]
    };
    let scale = if fps <= 30 { 0.65 } else { 1.0 };
    let v = base.map(|b| ((b as f32 * scale).round() as u32).max(4));
    [
        Tier { name: "Tasarruf", mbps: v[0] },
        Tier { name: "Dengeli", mbps: v[1] },
        Tier { name: "Kalite", mbps: v[2] },
    ]
}

/// Bilgisayardaki gerçek monitörler (ana GPU'nun çıkışları).
struct MonitorEntry {
    label: String,
    output_index: u32,
    pixels: u64,
}

fn list_monitors() -> Vec<MonitorEntry> {
    crate::vdd::all_outputs()
        .into_iter()
        .filter(|o| o.adapter_index == 0)
        .map(|o| {
            let w = o.rect.right - o.rect.left;
            let h = o.rect.bottom - o.rect.top;
            let primary = o.rect.left == 0 && o.rect.top == 0;
            MonitorEntry {
                label: format!(
                    "Monitör {} — {}x{}{}",
                    o.output_index + 1,
                    w,
                    h,
                    if primary { " (birincil)" } else { "" }
                ),
                output_index: o.output_index,
                pixels: (w.max(1) as u64) * (h.max(1) as u64),
            }
        })
        .collect()
}

#[derive(Serialize, Deserialize, Clone)]
struct GuiConfig {
    extend: bool,
    mode_index: usize,
    bitrate: u32,
    fps: u32,
    audio: bool,
    tv_audio: bool,
    cursor_follow: bool,
    port: u16,
    output_index: u32,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            extend: true,
            mode_index: 1, // 1080p
            bitrate: 12,
            fps: 60,
            audio: true,
            tv_audio: true,
            cursor_follow: true,
            port: 47000,
            output_index: 0,
        }
    }
}

fn config_path() -> Option<std::path::PathBuf> {
    Some(
        std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("mirror-host")
            .join("gui.json"),
    )
}

struct App {
    cfg: GuiConfig,
    child: Option<Child>,
    child_stdin: Option<ChildStdin>,
    stopping_since: Option<Instant>,
    logs: Arc<Mutex<VecDeque<String>>>,
    local_ip: String,
    monitors: Vec<MonitorEntry>,
    /// VB-CABLE kurulu mu (tv-audio için gerekli)?
    sink_ok: bool,
    cable_installing: Arc<std::sync::atomic::AtomicBool>,
    cable_was_installing: bool,
}

impl App {
    fn new() -> Self {
        let cfg = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            cfg,
            child: None,
            child_stdin: None,
            stopping_since: None,
            logs: Arc::new(Mutex::new(VecDeque::new())),
            local_ip: local_ip(),
            monitors: list_monitors(),
            sink_ok: crate::audio_route::virtual_sink_available(),
            cable_installing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cable_was_installing: false,
        }
    }

    /// VB-CABLE'ı resmî siteden indirip kurulumcusunu (UAC ile) çalıştırır.
    /// Kurulumcu exe'ye GÖMÜLMEZ — VB-Audio lisansı yeniden dağıtıma izin vermez;
    /// indirme kullanıcının makinesinde vendor'dan yapılır.
    fn install_cable(&mut self) {
        use std::sync::atomic::Ordering;
        if self.cable_installing.swap(true, Ordering::SeqCst) {
            return;
        }
        Self::push_log(&self.logs, "[panel] VB-CABLE indiriliyor (vb-audio.com)…".into());
        let flag = self.cable_installing.clone();
        let logs = self.logs.clone();
        std::thread::spawn(move || {
            let script = r#"$ProgressPreference='SilentlyContinue';
$dir = Join-Path $env:TEMP 'vbcable-dl'; New-Item -ItemType Directory -Force $dir | Out-Null;
$zip = Join-Path $dir 'cable.zip'; $ok = $false;
foreach ($u in @('https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip','https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack43.zip')) {
  try { Invoke-WebRequest -Uri $u -OutFile $zip -UseBasicParsing; $ok = $true; break } catch {}
}
if (-not $ok) { exit 1 }
Expand-Archive $zip -DestinationPath $dir -Force;
$setup = Get-ChildItem $dir -Recurse -Filter 'VBCABLE_Setup_x64.exe' | Select-Object -First 1;
if (-not $setup) { exit 2 }
Start-Process -FilePath $setup.FullName -ArgumentList '-i','-h' -Verb RunAs -Wait;
exit 0"#;
            let status = Command::new("powershell")
                .args(["-NoProfile", "-Command", script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match status {
                Ok(s) if s.success() => {
                    Self::push_log(&logs, "[panel] VB-CABLE kurulumu tamamlandı".into())
                }
                _ => Self::push_log(
                    &logs,
                    "[panel] VB-CABLE kurulamadı — elle kurun: vb-audio.com/Cable".into(),
                ),
            }
            flag.store(false, Ordering::SeqCst);
        });
    }

    /// Şablon seçimi için kaynak çözünürlük: genişletmede seçili sanal mod,
    /// aynalamada seçili gerçek monitörün pikselleri.
    fn source_pixels(&self) -> u64 {
        if self.cfg.extend {
            MODES[self.cfg.mode_index.min(MODES.len() - 1)].2
        } else {
            self.monitors
                .iter()
                .find(|m| m.output_index == self.cfg.output_index)
                .map(|m| m.pixels)
                .unwrap_or(1920 * 1080)
        }
    }

    fn save_config(&self) {
        if let Some(p) = config_path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            if let Ok(s) = serde_json::to_string_pretty(&self.cfg) {
                let _ = std::fs::write(p, s);
            }
        }
    }

    fn push_log(logs: &Arc<Mutex<VecDeque<String>>>, line: String) {
        let mut l = logs.lock().unwrap();
        if l.len() >= 400 {
            l.pop_front();
        }
        l.push_back(line);
    }

    fn start_stream(&mut self) {
        self.save_config();
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                Self::push_log(&self.logs, format!("[hata] exe yolu alınamadı: {e}"));
                return;
            }
        };
        let mut args: Vec<String> = vec![
            "--managed".into(),
            "--bind".into(),
            format!("0.0.0.0:{}", self.cfg.port),
            "--bitrate".into(),
            self.cfg.bitrate.to_string(),
            "--fps".into(),
            self.cfg.fps.to_string(),
        ];
        if self.cfg.extend {
            args.push("--extend".into());
            if let Some(mode) = MODES[self.cfg.mode_index.min(MODES.len() - 1)].1 {
                args.push("--mode".into());
                args.push(mode.into());
            }
        } else {
            args.push("--output".into());
            args.push(self.cfg.output_index.to_string());
        }
        if !self.cfg.audio {
            args.push("--no-audio".into());
        } else if self.cfg.tv_audio {
            args.push("--tv-audio".into());
        }
        if self.cfg.cursor_follow {
            args.push("--cursor-follow".into());
        }

        // Çalışma dizini exe'nin yanı: tv-app klasörü varsa diskten servis edilir.
        let workdir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let spawned = {
            use std::os::windows::process::CommandExt;
            Command::new(&exe)
                .args(&args)
                .current_dir(workdir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
        };
        match spawned {
            Ok(mut child) => {
                self.child_stdin = child.stdin.take();
                if let Some(out) = child.stdout.take() {
                    let logs = self.logs.clone();
                    std::thread::spawn(move || {
                        for line in BufReader::new(out).lines().map_while(Result::ok) {
                            Self::push_log(&logs, line);
                        }
                    });
                }
                if let Some(err) = child.stderr.take() {
                    let logs = self.logs.clone();
                    std::thread::spawn(move || {
                        for line in BufReader::new(err).lines().map_while(Result::ok) {
                            Self::push_log(&logs, format!("[!] {line}"));
                        }
                    });
                }
                Self::push_log(&self.logs, format!("[panel] yayın başlatıldı: {}", args.join(" ")));
                self.child = Some(child);
                self.stopping_since = None;
            }
            Err(e) => Self::push_log(&self.logs, format!("[hata] başlatılamadı: {e}")),
        }
    }

    /// Nazik durdurma: stdin kapanır → alt süreç düzgün kapanır (ses/monitör geri).
    fn request_stop(&mut self) {
        self.child_stdin = None; // pipe kapanır
        self.stopping_since = Some(Instant::now());
        Self::push_log(&self.logs, "[panel] durduruluyor…".into());
    }

    fn poll_child(&mut self) {
        if let Some(child) = &mut self.child {
            if let Ok(Some(status)) = child.try_wait() {
                Self::push_log(&self.logs, format!("[panel] yayın kapandı ({status})"));
                self.child = None;
                self.child_stdin = None;
                self.stopping_since = None;
            } else if let Some(t) = self.stopping_since {
                if t.elapsed() > Duration::from_secs(4) {
                    let _ = child.kill();
                    Self::push_log(&self.logs, "[panel] yanıt yok, zorla kapatıldı".into());
                    self.stopping_since = None;
                }
            }
        }
    }

    /// Pencere kapatılırken bloklu nazik kapatma.
    fn shutdown_blocking(&mut self) {
        self.child_stdin = None;
        if let Some(mut child) = self.child.take() {
            for _ in 0..30 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_child();
        // VB-CABLE kurulumu bitince aygıt listesini tazele.
        let installing = self.cable_installing.load(std::sync::atomic::Ordering::SeqCst);
        if self.cable_was_installing && !installing {
            self.sink_ok = crate::audio_route::virtual_sink_available();
        }
        self.cable_was_installing = installing;
        let running = self.child.is_some();
        let stopping = self.stopping_since.is_some();

        {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("PC Mirror");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, color) = if stopping {
                        ("● Durduruluyor…", egui::Color32::YELLOW)
                    } else if running {
                        ("● Yayında", egui::Color32::from_rgb(60, 220, 120))
                    } else {
                        ("● Hazır", egui::Color32::GRAY)
                    };
                    ui.colored_label(color, text);
                });
            });
            ui.add_space(6.0);

            ui.add_enabled_ui(!running, |ui| {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Ekran").strong());
                    ui.checkbox(&mut self.cfg.extend, "TV'yi ikinci ekran yap (genişlet)");
                    if self.cfg.extend {
                        egui::ComboBox::from_label("Sanal ekran çözünürlüğü")
                            .selected_text(MODES[self.cfg.mode_index.min(MODES.len() - 1)].0)
                            .show_ui(ui, |ui| {
                                for (i, (label, _, _)) in MODES.iter().enumerate() {
                                    ui.selectable_value(&mut self.cfg.mode_index, i, *label);
                                }
                            });
                    } else if self.monitors.is_empty() {
                        ui.horizontal(|ui| {
                            ui.label("Yansıtılacak monitör:");
                            ui.add(egui::DragValue::new(&mut self.cfg.output_index).range(0..=3));
                        });
                    } else {
                        ui.horizontal(|ui| {
                            let current = self
                                .monitors
                                .iter()
                                .find(|m| m.output_index == self.cfg.output_index)
                                .map(|m| m.label.clone())
                                .unwrap_or_else(|| "Monitör seç".into());
                            egui::ComboBox::from_label("Yansıtılacak monitör")
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    for m in &self.monitors {
                                        ui.selectable_value(
                                            &mut self.cfg.output_index,
                                            m.output_index,
                                            &m.label,
                                        );
                                    }
                                });
                            if ui.small_button("↻").on_hover_text("Monitör listesini yenile").clicked() {
                                self.monitors = list_monitors();
                            }
                        });
                    }
                });

                ui.group(|ui| {
                    ui.label(egui::RichText::new("Görüntü").strong());
                    ui.horizontal(|ui| {
                        ui.label("Kare hızı:");
                        ui.selectable_value(&mut self.cfg.fps, 30, "30 fps");
                        ui.selectable_value(&mut self.cfg.fps, 60, "60 fps");
                    });
                    // Çözünürlüğe uygun hazır şablonlar (Sunshine/Moonlight kılavuz
                    // değerlerinden; masaüstü kullanımına göre yumuşatıldı).
                    let tiers = tiers_for(self.source_pixels(), self.cfg.fps);
                    ui.horizontal(|ui| {
                        ui.label("Şablon:");
                        for t in &tiers {
                            let selected = self.cfg.bitrate == t.mbps;
                            if ui
                                .selectable_label(selected, format!("{} · {} Mb/s", t.name, t.mbps))
                                .clicked()
                            {
                                self.cfg.bitrate = t.mbps;
                            }
                        }
                    });
                    ui.add(
                        egui::Slider::new(&mut self.cfg.bitrate, 4..=60)
                            .text("Mb/s bit hızı")
                            .clamping(egui::SliderClamping::Always),
                    );
                    let net_need = (self.cfg.bitrate as f32 * 1.5).round() as u32;
                    let wire = if self.cfg.bitrate >= 30 { " — bu hızda TV'de Ethernet önerilir" } else { "" };
                    ui.weak(format!("Ağda ~{net_need} Mb/s boş bant olmalı{wire}"));
                });

                ui.group(|ui| {
                    ui.label(egui::RichText::new("Ses ve Davranış").strong());
                    ui.checkbox(&mut self.cfg.audio, "Sesi aktar");
                    ui.add_enabled(
                        self.cfg.audio,
                        egui::Checkbox::new(
                            &mut self.cfg.tv_audio,
                            "Ses yalnız TV'den çalsın (PC sussun)",
                        ),
                    );
                    if self.cfg.audio && self.cfg.tv_audio && !self.sink_ok {
                        let installing = self
                            .cable_installing
                            .load(std::sync::atomic::Ordering::SeqCst);
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "⚠ Sanal ses aygıtı (VB-CABLE) kurulu değil",
                            );
                            if installing {
                                ui.spinner();
                            } else if ui.button("İndir ve kur").clicked() {
                                self.install_cable();
                            }
                        });
                    }
                    ui.checkbox(
                        &mut self.cfg.cursor_follow,
                        "Alt+Tab'da imleç pencereye ışınlansın",
                    );
                    ui.horizontal(|ui| {
                        ui.label("Port:");
                        ui.add(egui::DragValue::new(&mut self.cfg.port).range(1024..=65535));
                    });
                });
            });

            ui.add_space(8.0);
            ui.vertical_centered_justified(|ui| {
                if running {
                    let btn = egui::Button::new(
                        egui::RichText::new("⏹  Durdur").size(18.0),
                    )
                    .fill(egui::Color32::from_rgb(140, 45, 45))
                    .min_size(egui::vec2(0.0, 40.0));
                    if ui.add_enabled(!stopping, btn).clicked() {
                        self.request_stop();
                    }
                } else {
                    let btn = egui::Button::new(
                        egui::RichText::new("▶  Yayını Başlat").size(18.0),
                    )
                    .fill(egui::Color32::from_rgb(35, 100, 200))
                    .min_size(egui::vec2(0.0, 40.0));
                    if ui.add(btn).clicked() {
                        self.start_stream();
                    }
                }
            });

            ui.add_space(6.0);
            let url = format!("http://{}:{}/", self.local_ip, self.cfg.port);
            ui.horizontal(|ui| {
                ui.label("İzleme adresi:");
                if ui.link(&url).clicked() {
                    ui.ctx().copy_text(url.clone());
                }
                ui.weak("(tıkla = kopyala)");
            });

            ui.add_space(4.0);
            ui.separator();
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let logs = self.logs.lock().unwrap();
                    for line in logs.iter() {
                        ui.label(egui::RichText::new(line).monospace().size(11.0));
                    }
                });
        }

        // Alt süreç logları için düzenli tazeleme.
        ui.ctx().request_repaint_after(Duration::from_millis(300));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_config();
        self.shutdown_blocking();
    }
}

fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "PC-IP".into())
}

pub fn run() -> anyhow::Result<()> {
    // Çift tıklamayla açıldıysa arkadaki konsol penceresinden kurtul.
    unsafe {
        let _ = windows::Win32::System::Console::FreeConsole();
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 720.0])
            .with_min_inner_size([470.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "PC Mirror",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(App::new()))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI başlatılamadı: {e}"))
}
