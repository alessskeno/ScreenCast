# ScreenMirroring — Kablosuz İkinci Ekran (Windows → Tizen TV)

Windows masaüstünü **kablosuz, düşük gecikmeli** olarak Samsung (Tizen) TV'ye ya da
herhangi bir tarayıcıya taşıyan Rust projesi.

```
┌─────────────────────────── Windows (host) ────────────────────────────┐
│ DXGI Desktop Duplication → D3D11 VideoProcessor → MF Donanım H.264    │
│        (GPU, BGRA)            (GPU, NV12)         (NVENC/QSV, GPU)    │
│                                   │                                   │
│   GetCursorInfo (125 Hz) ──┐      ▼                                   │
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

- ✅ **Faz 1 — Aynalama MVP** (bu depo): birincil ekranı TV'ye/tarayıcıya yansıtma.
  Boru hattı gerçek donanımda doğrulandı (NVIDIA NVENC ile ilk kare kodlanıyor).
- ⬜ **Faz 2** — ses (WASAPI loopback → Opus), mDNS keşif, çoklu monitör seçimi.
- ⬜ **Faz 3 — Gerçek "ikinci ekran" (genişletme)**: IddCx sanal monitör sürücüsü.
  Windows'a sahte bir monitör tanıtılır; Ayarlar → Ekran'da yeri seçilir, mouse
  kenardan TV'ye geçer. Başlangıç noktaları:
  - Microsoft IddCx örneği: `Windows-driver-samples/video/IndirectDisplay`
  - Hazır açık kaynak sürücü: "Virtual Display Driver" (IddSampleDriver türevleri)
  - Sürücü kurulunca bu host'ta tek değişiklik: `--output` ile sanal monitörü seçmek.

## Gereksinimler

- Windows 10/11, donanım H.264 kodlayıcılı GPU (NVIDIA/Intel/AMD — hepsi olur)
- Rust (kurulu) + VS Build Tools 2022 (kurulu)
- TV ile PC aynı ağda; 5 GHz WiFi ya da TV'de Ethernet önerilir

## Çalıştırma

```powershell
cd C:\Users\Ales\Documents\ScreenMirroring
cargo run --release
# özelleştirme örneği:
cargo run --release -- --fps 60 --bitrate 12 --gop 4 --output 0 --bind 0.0.0.0:47000
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
host/                    Rust yayın ucu
  src/main.rs            CLI + kablolama
  src/capture.rs         DXGI Desktop Duplication (GPU'da BGRA)
  src/convert.rs         D3D11 VideoProcessor: BGRA → NV12 (GPU)
  src/encoder.rs         MF donanım H.264 (asenkron MFT, düşük gecikme)
  src/pipeline.rs        yakalama+kodlama iş parçacıkları, "en tazesi kazanır" yuvası
  src/session.rs         izleyici başına WebRTC oturumu (H.264 track + imleç kanalı)
  src/signaling.rs       HTTP (tv-app servis eder) + WebSocket (offer/answer)
  src/cursor.rs          GetCursorInfo 125 Hz → watch kanalı
  src/protocol.rs        JSON mesaj biçimleri
tv-app/                  Tizen web uygulaması (tarayıcıda da çalışır)
  index.html / css / js  <video> + WebRTC istemcisi + imleç + istatistik
  config.xml             Tizen TV paket tanımı
```

## Bilinen sınırlar (Faz 1)

- Görüntü **aynalama**dır; gerçek "genişletme" Faz 3'te (IddCx sürücüsü) gelecek.
- Ses yok (Faz 2).
- Birincil monitör (0,0) varsayılır; `--output` ile başka ekran seçilirse imleç
  koordinatları o ekranın konumuna göre düzeltilmelidir (TODO).
- RDP oturumu içinde çalışmaz (Desktop Duplication kısıtı).
- DRM korumalı içerik (Netflix vb.) siyah görünebilir (OS kısıtı).
