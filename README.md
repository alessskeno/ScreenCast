# ScreenMirroring — Kablosuz İkinci Ekran (Windows / Linux → Tizen TV)

Masaüstünü **kablosuz, düşük gecikmeli** olarak Samsung (Tizen) TV'ye ya da
herhangi bir tarayıcıya taşıyan Rust projesi. **Windows ve Linux** yayın yapabilir;
TV/tarayıcı istemcisi (`tv-app/`) her ikisinde de aynıdır.

| | Windows | Linux (GNOME Wayland) |
|---|---|---|
| Yakalama | DXGI Desktop Duplication (ffmpeg `ddagrab`) | Mutter ScreenCast → PipeWire (GStreamer) |
| Kodlama | ffmpeg `h264_nvenc/qsv/amf/libx264` | GStreamer `nvh264enc/vah264enc/x264enc` |
| Ses | WASAPI loopback (cpal) | PipeWire monitör (`@DEFAULT_MONITOR@`) |
| Sanal 2. ekran (`--extend`) | parsec-vdd sürücüsü | Mutter `RecordVirtual` (sürücü GEREKMEZ) |
| Yalnız TV'den ses (`--tv-audio`) | VB-CABLE sürücüsü gerekir | `module-null-sink` (sürücü GEREKMEZ) |
| İmleç | Ayrı veri kanalı, 125 Hz | Videoya gömülü (Wayland kısıtı) |

```
┌───────────────────────────── host (PC) ───────────────────────────────┐
│ Windows: DXGI Desktop Duplication ─┐                                  │
│ Linux:   Mutter ScreenCast/PipeWire┴→ Donanım H.264 (NVENC/QSV/VA)    │
│                                   │                                   │
│   İmleç (yalnız Windows) ──┐      ▼                                   │
│                            │   WebRTC (UDP + DTLS, H.264 track)       │
│                            └── veri kanalı "cursor" (sırasız/anlık)   │
└──────────────────────────────────┬────────────────────────────────────┘
                              WiFi (5 GHz önerilir)
┌──────────────────────────────────▼────────────────────────────────────┐
│ TV / tarayıcı (tv-app): <video> donanım çözücü + imleci kendisi çizer │
└───────────────────────────────────────────────────────────────────────┘
```

Kare hiç CPU'ya inmez: yakalama → renk dönüşümü → kodlama tamamen GPU'da,
kodlanmış H.264 baytları CPU'ya iner (birkaç yüz KB/s) ve ağa gider.

## Durum

- ✅ **Aynalama** — ekranı TV'ye/tarayıcıya yansıtma (Windows + Linux).
- ✅ **Ses** — sistem sesi Opus ile aktarılıyor; `--tv-audio` ile PC susup ses
  yalnız TV'den çalabiliyor.
- ✅ **Gerçek ikinci ekran (`--extend`)** — sanal monitör takılıyor, mouse kenardan
  TV'ye geçiyor. Yayın bitince monitör kendiliğinden sökülüyor.
- ✅ **Kontrol paneli** — argümansız çalıştırınca açılan grafik panel.
- ✅ **Düşük gecikme** — WebCodecs yolunda 1080p/2K'da ölçülen boru gecikmesi <10 ms.

## Gereksinimler

**Ortak:** TV ile PC aynı ağda; 5 GHz WiFi ya da TV'de Ethernet önerilir.

**Windows:** Windows 10/11, donanım H.264 kodlayıcılı GPU (NVIDIA/Intel/AMD),
Rust + VS Build Tools 2022.
Aynalama için ek kurulum gerekmez (ffmpeg exe'ye gömülüdür). **Gerçek ikinci
ekran (`--extend`) için parsec-vdd sürücüsü gerekir** — çekirdek sürücüsü olduğu
için exe'ye gömülemez; panel "genişlet" seçiliyken sürücü yoksa "İndir ve kur"
düğmesi gösterir (yönetici izni ister).

**Linux:** GNOME (Wayland) oturumu — Ubuntu 22.04+, Fedora, CachyOS/Arch vb.
GStreamer + PipeWire + donanım H.264 kodlayıcı. Hepsini kuran betikler var:

```bash
bash scripts/setup-arch.sh      # Arch / CachyOS / Manjaro
bash scripts/setup-ubuntu.sh    # Ubuntu / Debian / Pop!_OS / Mint
```

Betikler paketleri kurar, Rust yoksa rustup'ı getirir ve sonunda hangi yakalama
elemanı + H.264 kodlayıcının kullanılabilir olduğunu tek tek doğrular.

## Çalıştırma (Linux)

```bash
cargo build --release

./target/release/mirror-host                          # kontrol paneli (grafik)
./target/release/mirror-host --bind 0.0.0.0:47000     # aynalama
./target/release/mirror-host --extend --mode 1920x1080@60   # gerçek 2. ekran
./target/release/mirror-host --tv-audio               # ses yalnız TV'den
./target/release/mirror-host --restore-audio          # takılı ses ayarını düzelt

# GNOME Quick Settings (GNOME Shell 47+)
bash gnome-extension/install.sh
# Wayland: oturumu yenile; sonra sistem menüsünde "PC Mirror"
```

Sonra aynı ağdaki tarayıcıdan / TV'den `http://<PC-IP>:47000/` açın.
İzlenecek monitörü `--output 0|1|…` seçer (sıra panelde de aynıdır).

**Linux notları**

- **GNOME Quick Settings:** `bash gnome-extension/install.sh` — egui ile aynı
  `--managed` alt süreç modeli; ayarlar chevron menüde (aynala/genişlet, çözünürlük,
  bitrate, fps, monitör, ses). egui paneli KDE/diğer DE'ler için kalır.
- **Oturum tipi önemli:** `--extend` ve Wayland yakalaması GNOME/Mutter'a bağlıdır.
  X11 oturumunda aynalama `ximagesrc` ile çalışır ama `--extend` çalışmaz;
  KDE/wlroots Wayland oturumları henüz desteklenmiyor (bkz. Bilinen sınırlar).
- **İmleç videoya gömülüdür.** Wayland bir uygulamanın küresel imleç konumunu
  okumasına izin vermez, bu yüzden Windows'taki "ayrı imleç kanalı" numarası
  Linux'ta kullanılamıyor; imleç video ile birlikte gelir.
- **Sürücü kurmak gerekmez:** sanal ekran Mutter'dan, sanal ses aygıtı
  PipeWire'dan geliyor — ikisi de yayın bitince kayboluyor.

## Çalıştırma (Windows)

```powershell
cargo run --release
```

İlk çalıştırmada Windows Güvenlik Duvarı sorarsa **özel ağlarda izin ver** deyin
(sormazsa: `netsh advfirewall firewall add rule name="mirror-host" dir=in action=allow program="...\target\release\mirror-host.exe" enable=yes`).

PC'nin IP'sini öğrenin: `ipconfig` → IPv4 Address.

### Hızlı test (TV'siz)

Aynı ağdaki herhangi bir cihazın tarayıcısından `http://<PC-IP>:47000/` açın —
telefon, tablet, başka bilgisayar, hatta TV'nin kendi tarayıcısı.
Ekran görüntüsü + canlı imleç gelmeli. `0` veya `i` tuşu istatistik kaplamasını açar
(çözünürlük, fps, Mb/s, tampon süresi, paket kaybı).

## TV'ye kurulum (Tizen paketi)

TV'nin tarayıcısı işi görür, ama gerçek uygulama daha akıcıdır:

1. **Tizen Studio** kur (Web app geliştirme + TV Extensions).
2. TV'de **Developer Mode**: Apps ekranında kumandayla `1-2-3-4-5` gir,
   açılan pencerede Developer Mode ON + PC'nin IP'sini yaz, TV'yi yeniden başlat.
3. Sertifika: Tizen Studio → Certificate Manager → yeni Samsung sertifika profili
   (TV'nin DUID'si otomatik eklenir).
4. Paketle ve kur:
   ```
   tizen package -t wgt -s <sertifika-profili> -- tv-app
   sdb connect <TV-IP>:26101
   tizen install -n tv-app/PCMirror.wgt -t <TV-adı>
   ```
5. Uygulama ilk açılışta PC'nin adresini sorar (`192.168.1.x:47000`), sonra hatırlar.

## Performans ayarları (neden böyle?)

| Ayar | Değer | Nedeni |
|---|---|---|
| Kodlayıcı | Donanım MFT (NVENC/QSV/VCN) | CPU ~%0; yazılım kodlayıcıya göre çok düşük gecikme |
| Girdi yolu | DXGI → D3D11 doku → MFT (sıfır kopya) | Kare CPU belleğine hiç inmez |
| Renk dönüşümü | D3D11 VideoProcessor (GPU sabit işlev) | Bedava denecek kadar ucuz, BT.709 doğru renk |
| Hız kontrolü | CBR @ 12 Mb/s (1080p60) | Sabit ağ yükü, ani bit patlaması yok |
| B-frame | 0 (düşük gecikme kipi) | B-frame = kare bekletme = gecikme |
| GOP | 4 sn + istek üzerine anahtar kare | Uzun GOP kaliteyi artırır; yeni izleyici/kayıpta PLI ile anında IDR |
| Taşıma | WebRTC (UDP, DTLS-SRTP) | TCP kafa-kuyruk beklemesi yok; kayıp toparlama (NACK/PLI) hazır |
| Kare atlama | Değişmeyen kare kodlanmaz | Masaüstü sabitken bant ~0'a düşer |
| İmleç | Ayrı veri kanalı, 125 Hz, sırasız | Video gecikmesinden bağımsız, anında imleç (RDP tekniği) |
| İzleyici tamponu | `playoutDelayHint = 0` | Tarayıcı/TV tarafında oynatma tamponu kapatılır |

Beklenen uçtan uca gecikme: iyi 5 GHz ağda **~60–120 ms**.

## Dosya haritası

```
host/                     Rust yayın ucu
  src/main.rs             CLI/GUI ayrımı + platforma göre yakalama kurulumu
  src/engine.rs           ORTAK: yapılandırma, EncodedFrame, Annex-B → AU ayrıştırma
  src/gui.rs              egui kontrol paneli (her iki platform)
  src/session.rs          izleyici başına WebRTC oturumu (H.264 track + imleç kanalı)
  src/signaling.rs        HTTP (tv-app servis eder) + WebSocket (offer/answer)
  src/audio.rs            ses yakalama (platforma göre) → Opus
  src/protocol.rs         JSON mesaj biçimleri
  — Windows'a özel —
  src/ffmpeg_engine.rs    ffmpeg ddagrab + donanım H.264
  src/capture.rs          DXGI Desktop Duplication (GPU'da BGRA)
  src/convert.rs          D3D11 VideoProcessor: BGRA → NV12 (GPU)
  src/encoder.rs          MF donanım H.264 (asenkron MFT, yedek motor)
  src/pipeline.rs         MF motorunun yakalama+kodlama iş parçacıkları
  src/vdd.rs              parsec-vdd sanal monitör denetimi
  src/cursor_win.rs       GetCursorInfo 125 Hz → watch kanalı
  src/audio_route_win.rs  IPolicyConfig ile varsayılan ses aygıtını çevirme
  src/focus_follow.rs     odak değişince imleci taşıma (WinEvent kancası)
  — Linux'a özel —
  src/screencast.rs       Mutter ScreenCast (D-Bus): aynalama + sanal monitör
  src/gst_engine.rs       GStreamer boru hattı (pipewiresrc/ximagesrc → H.264)
  src/cursor_linux.rs     imleç videoya gömülü (Wayland kısıtı) — kanal sessiz
  src/audio_route_linux.rs  pactl module-null-sink ile varsayılan çıkışı çevirme
gnome-extension/          GNOME Quick Settings eklentisi (pc-mirror@ales)
scripts/                  setup-arch.sh / setup-ubuntu.sh (bağımlılık kurulumu)
tv-app/                   Tizen web uygulaması (tarayıcıda da çalışır)
  index.html / css / js   <video>/<canvas> + WebRTC istemcisi + imleç + istatistik
  config.xml              Tizen TV paket tanımı
```

## Bilinen sınırlar

**Ortak**

- DRM korumalı içerik (Netflix vb.) siyah görünebilir (işletim sistemi kısıtı).
- Alt süreç tabanlı kodlayıcıya anlık anahtar kare zorlanamaz; bunun yerine
  1 sn'lik GOP kullanılır (yeni izleyici en geç 1 sn'de görüntü alır).

**Windows**

- RDP oturumu içinde çalışmaz (Desktop Duplication kısıtı).
- `--tv-audio` için VB-CABLE gibi bir sanal ses aygıtı gerekir (panel kurabilir).

**Linux**

- Yalnız **GNOME (Mutter)** Wayland oturumu tam desteklenir. KDE/wlroots
  oturumlarında ekran yakalama için xdg-desktop-portal yolu henüz yazılmadı;
  o sistemlerde X11 oturumu kullanılabilir (aynalama çalışır, `--extend` çalışmaz).
- İmleç videoya gömülüdür (Wayland küresel imleç konumu vermez) — imleç video
  gecikmesine tabidir. LAN'da ölçülen gecikme <10 ms olduğu için fark edilmiyor.
- Kare hızı sınırı yaklaşıktır: 144 Hz bir monitörde 60 fps hedefine karşı
  52–84 fps ölçüldü (`videorate` canlı kaynakta tam kesmiyor). Kodlayıcı
  yetiştiği için sorun çıkarmıyor, yalnız bir miktar fazladan GPU işi demek.
- H.264 kodlayıcı `aud` (erişim birimi ayracı) özelliğini desteklemelidir;
  desteklemeyen eleman (ör. `openh264enc`) otomatik elenir ve loglanır.
