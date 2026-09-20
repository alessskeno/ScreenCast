# PC Mirror — Kurulum

Kablosuz ekran aynalama / ikinci ekran: **PC → Samsung Tizen TV veya tarayıcı**.

| Platform | Ne kurarsın |
|---|---|
| **Linux (GNOME Wayland)** | `mirror-host` + GStreamer/PipeWire (+ isteğe bağlı QS eklentisi) |
| **Windows** | Tek `mirror-host.exe` (ffmpeg gömülü) |
| **İzleyici** | Tarayıcı yeterli; TV için ayrı `.wgt` |

---

## Hızlı kurulum (Linux — Arch / CachyOS)

```bash
# 1) Kaynak
git clone https://github.com/alessskeno/ScreenCast.git
cd ScreenCast

# 2) Bağımlılıklar (GStreamer, PipeWire, Rust…)
bash scripts/setup-arch.sh

# 3) Derle
cargo build --release

# 4) Çalıştır
./target/release/mirror-host                  # grafik panel
# veya
./target/release/mirror-host --bind 0.0.0.0:47000

# 5) (İsteğe bağlı) GNOME Quick Settings
bash gnome-extension/install.sh
# Wayland: bir kez oturumdan çıkıp gir → sistem menüsünde "PC Mirror"
```

Aynı Wi‑Fi/Ethernet’te telefondan veya TV tarayıcısından:

```text
http://<PC-IP>:47000/
```

PC IP: `ip -4 addr` → `wlp…` / `enp…` satırı (ör. `192.168.1.77`).

---

## Hızlı kurulum (Linux — Ubuntu / Debian)

```bash
git clone https://github.com/alessskeno/ScreenCast.git
cd ScreenCast
bash scripts/setup-ubuntu.sh
cargo build --release
./target/release/mirror-host --bind 0.0.0.0:47000
```

---

## Hızlı kurulum (Release binary — derlemeden)

GitHub [Releases](https://github.com/alessskeno/ScreenCast/releases) sayfasından
`mirror-host-linux-x64` indir:

```bash
# Bağımlılıklar yine gerekli (GStreamer/PipeWire sistemden gelir)
bash scripts/setup-arch.sh          # veya setup-ubuntu.sh — Rust atlanabilir ama paketler şart

chmod +x mirror-host-linux-x64
./mirror-host-linux-x64 --bind 0.0.0.0:47000
```

GNOME QS eklentisi için release’deki `pc-mirror@ales.zip` veya repodaki:

```bash
bash gnome-extension/install.sh
```

---

## Hızlı kurulum (Windows)

1. [Releases](https://github.com/alessskeno/ScreenCast/releases) → `mirror-host.exe`  
   (veya kaynak: `cargo build --release`)
2. Çift tıkla → kontrol paneli → **Başlat**
3. Güvenlik duvarında özel ağ izni ver
4. Tarayıcı / TV: `http://<PC-IP>:47000/`

`--extend` (gerçek 2. ekran) için **parsec-vdd** gerekir; panel yoksa “İndir ve kur” sunar.  
`--tv-audio` için **VB-CABLE** önerilir.

---

## Sık kullanılan komutlar

```bash
./target/release/mirror-host --bind 0.0.0.0:47000              # aynalama
./target/release/mirror-host --extend --mode 1920x1080@60      # 2. ekran
./target/release/mirror-host --tv-audio                        # ses yalnız TV
./target/release/mirror-host --restore-audio                   # takılı sesi düzelt
./target/release/mirror-host --list-monitors                   # JSON monitör listesi
```

Güvenlik duvarı (LAN ile sınırlı tutun):

```bash
# ufw örneği — ağı kendinize göre değiştirin
sudo ufw allow from 192.168.1.0/24 to any port 47000 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 47100:47120 proto udp
sudo ufw allow from 192.168.1.0/24 to any port 5353 proto udp
```

---

## Samsung Tizen TV

Tarayıcı çoğu iş için yeter. Kalıcı uygulama istiyorsan:

1. TV’de **Developer Mode** açık, Host PC IP = bu bilgisayarın IP’si  
2. Tizen Studio CLI ile `tv-app` → `.wgt` paketle / yükle (ayrıntı: [README](README.md#tizen-tvye-kurulum))  
3. `.wgt` adında **boşluk olmasın** (`PCMirror.wgt`)

---

## Ne kurulur, ne kurulmaz?

| Bileşen | Zorunlu? | Not |
|---|---|---|
| `mirror-host` | Evet | Yayın motoru |
| GStreamer + PipeWire (Linux) | Evet | setup betiği kurar |
| GNOME QS eklentisi | Hayır | Kolaylık; egui panel yeterli |
| Tizen `.wgt` | Hayır | Tarayıcı ile de izlenir |
| NVIDIA/Intel/AMD H.264 | Önerilir | Yoksa yazılım `x264enc` (daha yavaş) |

**Linux sınırları:** GNOME Wayland (`--extend` Mutter ister). KDE/wlroots henüz yok.  
İmleç Linux’ta videoya gömülür (ayrı kanal yok).

---

## Sorun giderme (kısa)

| Belirti | Ne yap |
|---|---|
| Sayfa açılır, görüntü yok | UDP `47100–47120` + `5353` (mDNS) açık mı? |
| QS eklentisi görünmüyor | Oturumdan **çıkış** (kilit yetmez) → `gnome-extensions enable pc-mirror@ales` |
| TV siyah / 2×2 | Host’u yeniden başlat; kesirli ölçekte host 1080p’ye ölçekler |
| Ses PC’de takılı kaldı | `mirror-host --restore-audio` |
| `mirror-host bulunamadı` (QS) | Eklenti tercihlerinde binary yolu veya `~/.local/bin`’e kopyala |

Daha fazla teknik ayrıntı: [README.md](README.md) · [CLAUDE.md](CLAUDE.md)
