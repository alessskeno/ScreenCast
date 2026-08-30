# CLAUDE.md — ScreenMirroring Proje Rehberi

Bu dosya her oturumda otomatik yüklenir. Amaç: aynı şeyleri yeniden araştırmamak.

## BAKIM KURALLARI (her oturumda geçerli)

1. **Yeni bir dosya oluşturduğunda veya bir dosyanın görevi değiştiğinde, aşağıdaki
   "Dosya Haritası" bölümüne ekle/güncelle** — dosyanın yolu + tek cümlelik görevi.
2. Yeni bir teknik tuzak/çözüm keşfedince "Teknik Tuzaklar" bölümüne ekle.
3. Faz durumu değişince "Durum ve Yol Haritası"nı güncelle.
4. Kullanıcıyla iletişim **Türkçe ve sade dilde**; kod yorumları da Türkçe.
5. Kullanıcının çalışma prensibi: **önce tüm bileşenleri uçtan uca test et,
   optimizasyona ancak somut bir sorun görülünce gir.** Erken optimizasyon önerme.

## Proje Nedir?

Windows laptop ekranını **kablosuz** olarak Samsung (Tizen) TV'ye/tarayıcıya taşıyan
Rust projesi. Nihai hedef: TV'yi gerçek "ikinci monitör" yapmak (Win+P Genişlet gibi,
mouse kenardan TV'ye geçecek). Şimdilik aynalama (mirror) çalışıyor.

## Derleme ve Çalıştırma

```powershell
# Proje kökünde (workspace: host/ üyesi):
cargo build --release          # derle
cargo run --release            # çalıştır (varsayılan: 0.0.0.0:47000, 60fps, 12Mb/s)
cargo run --release -- --fps 60 --bitrate 12 --gop 4 --output 0 --bind 0.0.0.0:47000
```

- Rust winget ile kurulu; yeni terminalde `cargo` PATH'te. Eski oturumlarda tam yol:
  `& "$env:USERPROFILE\.cargo\bin\cargo.exe"`.
- MSVC: VS Build Tools 2022 kurulu (vswhere ile bulunur).
- Log seviyesi: `$env:RUST_LOG = "debug"` (varsayılan info).
- İlk çalıştırmada güvenlik duvarı izni gerekir (0.0.0.0 bind). 127.0.0.1 bind sorusuz.
- Test: aynı ağdaki herhangi bir tarayıcıdan `http://<PC-IP>:47000/` aç
  (host, tv-app/ klasörünü statik servis eder). `0` veya `i` tuşu istatistik açar.
- Sağlıklı açılış logları sırasıyla: "Yakalama WxH → kodlama ..." →
  "Donanım kodlayıcı: NVIDIA H.264 Encoder MFT" → "Sunucu hazır" → "İlk kare kodlandı".

## TV'ye Kurulum (Tizen) — çalışan komutlar

Tizen Studio CLI kurulu: `C:\tizen-studio` (tizen.bat: `tools\ide\bin\`, sdb: `tools\`).
Sertifika profili: **TVProfile** (author: `C:\tizen-studio-data\keystore\author\alesauthor.p12`,
şifre: `local-notes.md`'de — git dışıdır; distributor: varsayılan tizen-distributor-signer).
TV: Samsung QN90A 43" (QE43QN90AAUXRU), IP genelde **192.168.0.104**; PC Ethernet IP **192.168.0.105**.

```powershell
& "C:\tizen-studio\tools\ide\bin\tizen.bat" package -t wgt -s TVProfile -- tv-app
Copy-Item "tv-app\PC Mirror.wgt" "tv-app\PCMirror.wgt" -Force   # boşluksuz ad ŞART
& "C:\tizen-studio\tools\sdb.exe" connect 192.168.0.104:26101
& "C:\tizen-studio\tools\ide\bin\tizen.bat" install -n "tv-app\PCMirror.wgt" -s 192.168.0.104:26101
& "C:\tizen-studio\tools\ide\bin\tizen.bat" run -p AzMirr0001.PCMirror -s 192.168.0.104:26101
```

## GUI Paneli (2026-08-30)

- Exe **argümansız** (çift tıklama) veya `--gui` ile açılırsa egui/eframe paneli çıkar;
  argümanla açılırsa eski CLI. eframe 0.36: **glow backend'i ŞART**
  (`default-features=false, features=["default_fonts","glow"]`) — varsayılan wgpu,
  farklı windows crate sürümü çekip derlemeyi kırıyor. 0.36 API: `App::update` yerine
  `App::ui(&mut self, ui, frame)`.
- Panel yayını AYNI exe'yi `--managed` + bayraklarla alt süreç olarak başlatır
  (CREATE_NO_WINDOW), stdout/stderr'ini log paneline akıtır. "Durdur" = stdin'i
  kapatmak → host'ta `--managed` stdin-EOF'u Ctrl+C gibi düzgün kapanışa çevirir
  (ses aygıtı geri gelir, sanal monitör sökülür). 4 sn yanıt yoksa kill.
- Ayarlar %LOCALAPPDATA%\mirror-host\gui.json'da kalıcı. GUI kendi konsolunu
  FreeConsole ile kapatır ("Win32_System_Console" feature).
- Aynalama modunda gerçek monitör listesi gösterilir (vdd::all_outputs, adaptör 0)
  + ↻ yenile. Bit hızı şablonları (Tasarruf/Dengeli/Kalite) kaynak çözünürlüğe ve
  fps'e göre hesaplanır (tiers_for): 1080p60=8/15/25, 1440p60=12/25/40,
  4K60=20/35/50 Mb/s (30fps ≈ x0.65; Sunshine/Moonlight kılavuzlarından masaüstüne
  yumuşatıldı). Ağ ihtiyacı ipucu = bitrate x1.5; ≥30 Mb/s'te Ethernet önerisi.

## Mimari (veri yolu)

```
DXGI Desktop Duplication → D3D11 VideoProcessor → MF Donanım H.264 → WebRTC → TV
     (GPU, BGRA)              (GPU, BGRA→NV12)      (NVENC, Annex-B)   (UDP)
GetCursorInfo 125Hz ──────────────────────────────→ "cursor" veri kanalı (ayrı)
```

- Kare CPU'ya hiç inmez; sadece kodlanmış H.264 baytları iner.
- Değişmeyen kareler kodlanmaz (LastPresentTime==0 → atla).
- İmleç videoya gömülmez; ayrı sırasız/yeniden-gönderimsiz veri kanalından gider,
  TV kendi çizer (video gecikmesinden bağımsız).
- Yakalama ve kodlama ayrı std iş parçacıklarında; aralarında tek karelik
  "en tazesi kazanır" yuvası (Mutex+Condvar) — kuyruk birikmesi = gecikme, istemeyiz.
- Kodlanan kareler tokio broadcast(240) kanalına; her izleyici abone olur.
- Yeni izleyici / Lagged / PLI → keyframe_request AtomicBool → encoder IDR zorlar;
  izleyici anahtar kare gelene dek kare yazmaz (çözücü bozulmasın).
- Sinyalleşme: TV offer yollar, host answer döner; trickle ICE yok (LAN'da gereksiz).
  İmleç kanalı iki uçta da `negotiated, id: 0` — SDP pazarlığı beklemez.
- Yalnız H.264 kodek kaydedilir (TV donanım çözücüsü; VP8/9 pazarlığı engellenir).

## Dosya Haritası

```
Cargo.toml               Workspace + release profili (LTO, panic=abort, opt dev-deps)
CLAUDE.md                Bu dosya
README.md                Kullanıcıya dönük kurulum/kullanım (değişiklikte senkron tut)
.gitignore               target/, *.wgt, loglar
host/Cargo.toml          Bağımlılıklar: windows 0.61, webrtc 0.12, axum 0.8, tokio...
host/src/main.rs         GUI/CLI ayrımı (argümansız veya --gui → panel) + CLI (clap) + motor seçimi
host/src/gui.rs          egui/eframe kontrol paneli: ayarlar + Başlat/Durdur + canlı log (alt süreç yönetir)
host/src/ffmpeg_engine.rs Faz 1.5 motoru: ffmpeg alt süreci (ddagrab + otomatik kodlayıcı zinciri) → AU ayrıştırma → broadcast; 60fps; gömülü ffmpeg çıkarma
host/build.rs            assets/ffmpeg.exe varsa embedded_ffmpeg cfg'sini açar
host/assets/ffmpeg.exe   Gömülecek ffmpeg (gitignore'da; gyan.dev essentials önerilir)
host/src/capture.rs      DXGI Desktop Duplication; Grab{New,NoChange,Timeout,Lost}
host/src/convert.rs      D3D11 VideoProcessor BGRA→NV12 (4'lü doku havuzu) + create_texture
host/src/encoder.rs      MF asenkron donanım H.264 MFT; düşük gecikme/CBR; VARIANT yardımcıları
host/src/pipeline.rs     Cihaz kurulumu + capture/encode iş parçacıkları + broadcast kanalı
host/src/session.rs      İzleyici başına WebRTC oturumu (track + PLI görevi + imleç pompası)
host/src/signaling.rs    axum: / → tv-app statik, /ws → WebSocket; AppState
host/src/cursor.rs       GetCursorInfo 125Hz → watch kanalı; output_rect ile çoklu monitör normalizasyonu
host/src/audio.rs        WASAPI loopback (cpal) → stereo/48kHz → Opus 20ms çerçeveler → broadcast
host/src/vdd.rs          parsec-vdd denetimi: sanal monitör tak/yaşat + DXGI çıkış keşfi (--extend)
host/src/audio_route.rs  --tv-audio: IPolicyConfig ile varsayılan ses aygıtını sanala çevir/geri al
host/src/focus_follow.rs --cursor-follow: odak başka monitöre geçince imleci ışınla (WinEvent kancası)
host/src/protocol.rs     JSON mesajları: SignalMessage{Offer,Answer}, CursorState
tv-app/index.html        TV/tarayıcı istemcisi iskeleti (video + imleç svg + kurulum paneli)
tv-app/css/style.css     Tam ekran siyah tema, object-fit: contain
tv-app/js/app.js         WS sinyalleşme + WebRTC alıcı + imleç çizimi + istatistik ('0' tuşu)
tv-app/config.xml        Tizen TV paket tanımı (id AzMirr0001.PCMirror, profil tv)
tv-app/icon.png          Basit uygulama ikonu (117x117)
```

## Teknik Tuzaklar (yeniden keşfetme!)

**windows crate 0.61 (0.61.3 kilitli):**
- `VARIANT` ham C union'dır: `Win32::System::Variant::VARIANT` (windows::core'da YOK).
  `ICodecAPI::SetValue` metodunun görünmesi için `Win32_System_Ole` feature ŞART
  (Com+Ole+Variant üçlüsü). Sayı/bool için `encoder.rs` içindeki `variant_u32/variant_bool`.
- Şunlar out-param değil değer döndürür: `GetImmediateContext()`, `IDXGIOutputDuplication::GetDesc()`,
  `CreateVideoProcessorEnumerator(&desc)`, `CreateVideoProcessor(&enum, 0)`.
- Şunlar out-param kalır: `CreateTexture2D(.., Some(&mut t))`,
  `CreateVideoProcessorInputView/OutputView(.., Some(&mut v))`, `MFCreateDXGIDeviceManager`.

**NVENC / Media Foundation:**
- NVENC MFT sıfırdan kurulan giriş türünü 0xC00D36B4 ile reddeder →
  `GetInputAvailableType` ile önerilen NV12 türünü al, FRAME_SIZE/RATE üzerine yaz, geri ver.
- Sıra önemli: ASYNC_UNLOCK → SET_D3D_MANAGER → SetOutputType → SetInputType → ICodecAPI.
- NVENC şunları desteklemez (uyarı loglanır, zararsız): CODECAPI_AVEncCommonLowLatency,
  CODECAPI_AVEncMPVDefaultBPictureCount. AVLowLatencyMode kabul edilir (kritik olan bu).
- Asenkron MFT olay değerleri sabittir: NeedInput=601, HaveOutput=602.
- İlk ProcessOutput'ta MF_E_TRANSFORM_STREAM_CHANGE normaldir → çıkış türünü yenile, devam.
- Anahtar kare tespiti: `MFSampleExtension_CleanPoint == 1`.

**Tizen / TV kurulumu:**
- **.wgt dosya adında boşluk olursa TV'deki kurulum "Failed to install Tizen application"
  ile hiç açıklamasız çöker** (config.xml `<name>PC Mirror</name>` → "PC Mirror.wgt").
  Çözüm: paketi boşluksuz ada kopyalayıp öyle yükle.
- Varsayılan Tizen distributor sertifikası bu TV'de (QN90A, developer mode) çalışıyor;
  Samsung hesabı/DUID'li Samsung sertifikasına gerek KALMADI.
- Tizen web-cli kurucusu (`--accept-license C:\tizen-studio`) kendini ayrı sürece
  kopyalayıp hemen 0 döner — asıl kurulumun bitmesini süreci bekleyerek anla (Wait-Process).
- TV'de sdb portu 26101; developer mode açıkken `sdb connect IP:26101`.
- **Viewport sabit olmalı**: `<meta name="viewport" content="width=1920, height=1080,
  user-scalable=no">` — yoksa uygulama TV'de sol üstte küçük pencere olarak çizilir.
- **Kumanda gezinmesi elle kodlanır**: ok tuşları form alanları arasında kendiliğinden
  gezmez. keyCode ile (13=OK, 38=yukarı, 40=aşağı) odak yönetimi + `:focus` çerçevesi
  şart (app.js "TV kumandası gezinmesi" bölümü). Eski Tizen Chromium'da `ev.key` güvenilmez.
- **Eski Tizen Chromium'da modern CSS yok (v0.2.3 dersi)**: `inset` (Chrome 87+) ve
  flex `gap` (84+) TANINMIYOR → panel sol üstte küçük kutu oluyordu. Kenarları
  left/top/width/height ile tek tek yaz, aralıkları margin'le ver. Ağ taraması
  TV'de 16'lık dalgalarla (51 paralel istek + 4K çözme = arayüz donması);
  try/finally ile buton kilidi garantili açılır; panel gizlenince activeElement.blur().
- **Sayı/renk/medya tuşları uygulamaya gelmez** — önce `tizen.tvinputdevice.registerKey('0')`
  ile kayıt gerekir (ok/OK/Geri hariç hepsi böyle). try/catch içine al, tarayıcıda tizen yok.
- **Kullanıcının kumandası minimalist Samsung Solar kumanda: fiziksel rakam tuşu YOK.**
  Kısayolları rakamlara bağlama! İstatistik: yayın ekranında **OK tuşu** (keyCode 13),
  tarayıcı/telefonda '0'/'i' veya videoya çift tıklama/dokunma (v0.1.3).
- `netsh advfirewall` kural ekleme yönetici ister; yükseltilmemiş kabukta sessizce
  başarısız olur (exit 1) — kural yerine ilk çalıştırmada Windows izin penceresi kullanılır.

**Performans teşhisi (2026-08-29/30 ölçümleri — YENİDEN KEŞFETME):**
- Belirti: TV ve telefonda aynı ~7-20 fps + telefonda ~1.5 sn jitter tamponu → ağ SUÇSUZ
  (kayıp 0, telefon modem dibinde). Kaynak host tarafıydı.
- DÜZELTİLDİ: RTP zaman damgası bug'ı — kare atlamalı akışta sabit 1/fps duration
  alıcı saatini geri bırakıp tamponu şişiriyordu; artık gerçek kareler-arası süre
  gönderiliyor (session.rs, EncodedFrame.ts_100ns → duration).
- DÜZELTİLDİ: yakalama 165Hz panelde 150+fps koşup GPU'yu boğuyordu (yakalama ↑ =
  kodlama ↓ 6fps'e); artık hedef fps'e sabitlendi (pipeline.rs interval/due).
- DÜZELTİLDİ: encoder olay döngüsünde sleep(1ms) aslında ~15.6ms'dir (Windows timer
  çözünürlüğü) — bloklu GetEvent'e geçildi.
- KALAN DARBOĞAZ (kanıtlı): NVIDIA H.264 MFT'nin ProcessInput'u kare başına ~25ms
  senkron bekliyor (RTX 5070). Denenen ve İŞE YARAMAYANLAR: D3D11_BIND_VIDEO_ENCODER
  bayrağı, VP'ye bağlanmamış temiz teslim havuzu, AVLowLatencyMode kapatma.
  MF_SA_D3D11_AWARE=1 doğrulandı. Sonuç: MF sarmalayıcısının kendisi; çözüm Faz 1.5.
- Teşhis araçları: host 3sn'de bir "Boru hattı: yakalama X fps | kodlama Y fps | Z Mb/s"
  ve "MFT 3sn: ... olay bekleme/kare bekleme/submit/drain" loglar. Claude başlatırsa
  log: %TEMP%\mirror-host.log. TV/tarayıcı istatistiği: OK tuşu / çift dokunuş.

**Diğer:**
- Desktop Duplication RDP oturumunda çalışmaz; DRM içerik siyah olabilir.
- Duplication kareleri imleç İÇERMEZ (bizim için avantaj — ayrı kanal tekniği).
- DPI: `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` şart, yoksa imleç
  koordinatları ölçekli ekranda kayar.
- D3D cihazı 3 taraftan kullanılır → `ID3D11Multithread::SetMultithreadProtected(true)` şart.
- COM işaretçileri Send değil → `unsafe impl Send` sarmalayıcıları (FrameTex, CaptureSide, Encoder).
- axum 0.8: `Message::Text` `Utf8Bytes` alır (`String.into()` / `.as_str()`).
- webrtc 0.12: veri kanalı `RTCDataChannelInit{ negotiated: Some(0), .. }`;
  JS tarafında `{negotiated: true, id: 0}` ile EŞLEŞMELİ.
- tv-app değişikliği: tarayıcı/telefon için derleme İSTEMEZ (diskten servis; sayfayı
  yenile), TV için .wgt kurulumu gerekir. AMA include_str! gömmesi yüzünden bir
  sonraki cargo build host'u yeniden derler (gömülü kopyalar tazelensin diye) —
  normaldir. Exe çalışırken derleme "dosya kilitli" hatası verir; önce kapat.

## Test Durumu

- ✅ Yakalama+dönüşüm+NVENC: bu makinede doğrulandı (1920x1080@60, NVIDIA GPU).
- ✅ Tarayıcıda aynalama + imleç: kullanıcı test etti, çalışıyor (2026-08-29).
- ✅ Tizen uygulaması TV'de uçtan uca ÇALIŞIYOR (QN90A, 2026-08-29, v0.1.2).
- **TV performans tavanı ÖLÇÜLDÜ (2026-08-30)**: TV uygulaması (Tizen tarayıcı
  motoru) — 1080p60: 60fps ✓ | 2K: 20-30fps | 4K: 8-9fps. iPhone 16 Pro her
  çözünürlükte 60fps → darboğaz TV'nin tarayıcı WebRTC çözme/çizme yolu.
- **OPTİMİZASYON 1 — ÇİFT AKIŞ TAMAM (2026-08-30, v0.2.5)**: kaynak >1080p ise
  host İKİNCİ bir 1080p@12Mb/s akış üretir (ffmpeg_engine "lite": aynı kodlayıcı,
  `-vf hwdownload,format=bgra,scale=1920:-2:flags=fast_bilinear,format=bgr0`).
  **fast_bilinear ŞART** (varsayılan bicubic 4K60'ı 38fps'e düşürür; fb ile 55-61fps
  — ölçüldü). Bu gyan ffmpeg d3d11→cuda hwmap köprüsünü DESTEKLEMİYOR ("Function
  not implemented") → GPU scale yolu kapalı, CPU yolu kullanılıyor. İstemci Offer'da
  `profile:"lite"|"full"` bildirir (TV: window.tizen varsa lite; geçersiz kılma:
  localStorage 'mirror.profile'). session.rs video pompası Offer gelene dek
  başlamaz (profil bilinmeli). Doğrulama: 4K ana 59-61fps + 1080p lite 57-61fps
  AYNI ANDA. Kullanıcı TV'de 60fps doğruladı.
- **FAZ 4 ADAYI (araştırıldı 2026-08-30, BEKLEMEDE)** — TV'de gerçek 4K60:
  Samsung **Tizen WASM Player / ElementaryMediaStreamSource (EMSS)**, H.264'ü
  doğrudan TV donanım çözücüsüne verir (kLow/kUltraLow latency kipleri; Moonlight
  Tizen portu bu teknikle, Samsung resmî dokümanında örnek). Gereksinim: istemci
  video katmanı C++→WASM (Samsung Emscripten çatalı + Tizen Studio WASM eklentisi),
  WebRTC yerine host'a ham AU ucu (host kolay), ses/imleç yeniden bağlama.
  Tahmin: 1-2 hafta; risk orta-yüksek. AVPlay ELENDİ (VOD tamponlu, saniyeler).
  Giriş kriteri: kullanıcı TV'de yakın mesafe metin işi yapıyorsa değer; salt
  video/uzak izlemede 1080p+TV upscale yeterli. Kaynaklar: developer.samsung.com
  smarttv WASM game-streaming dokümanları, OneLiberty/moonlight-chrome-tizen.
- **OPTİMİZASYON 2 — WebCodecs yolu + ölçüm araçları (2026-08-30, v0.3.0)**:
  Gecikme analizi bulgusu: LAN'da gecikmenin ~%70'i tarayıcının WebRTC jitter
  tamponu (70-200ms ölçüldü; gethopp makalesi de 102-189ms buldu; MoonlightWeb
  WebCodecs'le <20ms kanıtı). Uygulanan: (a) istemci Offer'da `video_path:
  "webcodecs"|"rtp"` — webcodecs'te host RTP yerine ham AU'ları negotiated id:1
  veri kanalından yollar ([1B bayrak][8B ts_100ns LE][AU] çerçevesi; SCTP tamponu
  3MB'ı aşarsa anahtar kareye atlar), istemci VideoDecoder(avc1.42e01f, Annex-B,
  optimizeForLatency) → canvas#cast'a çizer; hata → localStorage'a rtp yazıp
  yeniden yükleme (tek sefer, sessionStorage korumalı). Video elementi sesi
  çalmaya devam eder (gizli). TV=rtp (Tizen'de WebCodecs yok), telefon/PC=auto
  webcodecs. Geçersiz kılma: localStorage 'mirror.transport'. (b) Ölçüm: cursor
  kanalında ping→pong RTT; RTP modunda rVFC ile "boru" (alım→gösterim); WebCodecs
  modunda alım→çözüm EMA; hepsi istatistik satırında. (c) host /clock sayfası:
  cam-camdan ölçüm için ms sayacı (sanal ekrana koy + iki ekranı tek fotoğrafta
  çek). İstatistik formatı: "WxH | fps | Mb/s | tampon/boru | RTT | [WebCodecs]".
- **WebCodecs SİYAH EKRAN bug'ı (çözüldü, v0.3.1)**: veri kanalında tek mesaj
  ~256KB'ı aşınca kanal SESSİZCE ölüyor (ilk gönderilen mesaj = en büyük olan
  anahtar kare, 1080p'de bile 360KB ölçüldü) → görüntü hiç başlamıyor, imleç
  yaşıyor. Çözüm: AU'lar 16KB parçalara bölünür ([1B bayrak: bit0=key bit1=son]
  [8B ts][veri]); istemci sıralı kanalda birleştirir. AU kanalı negotiated id 2
  (id 1'de tek/çift teorisi denendi, sorun o DEĞİLDİ — boyuttu; id 2 kaldı).
  Uçtan uca kanıt: headless Edge'de "ilk kare çözüldü 1920x1080" logu.
- **4K WebCodecs siyah ekranı (çözüldü, v0.3.2)**: codec dizesi 'avc1.42e01f'
  (baseline L3.1) 4K'yı KAPSAMAZ → bazı çözücüler 4K karede sessizce boğuluyor.
  'avc1.640034' (High@L5.2) yapıldı; headless'ta 4K "ilk kare çözüldü 3840x2160"
  kanıtlı (693KB anahtar kare parçalanarak geçti). Ayrıca tek seferlik onarım:
  localStorage 'mirror.cv'!=2 ise eski yapışkan rtp fallback'i temizlenir
  (kullanıcının telefonu bug döneminde kalıcı rtp'ye düşmüştü — istatistikte
  "tampon+kayıp" alanları görünüyorsa istemci RTP'dedir; WebCodecs satırı
  "| WebCodecs" ile biter).
- **Kullanıcı ölçümleri (2026-08-30, iPhone 16 Pro, RTP yolu, 5GHz yakın)**:
  1080p: tampon 21ms boru 23ms RTT 3 | 4K: tampon 28-41 boru 38-56 RTT 4-14 |
  2K: tampon 67 boru 59. (RTP'de bile eski 70-200ms tamponlardan iyi.)
- **WebCodecs modu istemci kusurları (çözüldü, v0.3.3)** — telefon onarımla İLK KEZ
  gerçekten WebCodecs'e geçince ortaya çıktı: (1) ses yok: medya elementine kare
  üretmeyen video track eklenince WebKit elementi beklemede tutup sesi başlatmıyor
  → WebCodecs modunda elemente YALNIZ ses track'i verilir; (2) istatistik çift
  dokunuşla açılmıyor: dblclick yalnız video'daydı, görünen element canvas →
  ikisine de bağlandı; (3) ekran kararması: çözücü hatasında location.reload
  yapılıyordu → artık decoder yerinde sıfırlanır (resetDecoder, anahtar kare
  bekler; üst üste 5 hatada rtp'ye düşer). "Ses aygıtı takılı kaldı" şüphesinde:
  `mirror-host --restore-audio` (veya sonraki normal açılış otomatik düzeltir).
- **Headless tarayıcı testi tarifi**: msedge --headless=new --enable-logging=stderr
  --disable-features=WebRtcHideLocalIpsWithMdns ŞART (yoksa mDNS .local adayları
  headless'ta çözülmüyor, ICE hiç kurulmuyor — gerçek cihazlarda sorun yok).
  Konsol logları stderr'e "CONSOLE" satırları olarak düşer. İstemcide kalıcı
  iz-kırıntıları var: "[app] video yolu", "[wc] kanal açıldı/ilk parça/ilk kare".
- **4K WebCodecs telefonda stutter + sekme çökmesi (çözüldü, v0.3.4)**: 4K60
  kareyi 4K canvas'a çizmek WebKit sekme bellek sınırını aşıyor. canvasTarget():
  canvas cihaz ekranıyla (innerWidth*dpr) sınırlanır, drawImage küçülterek çizer;
  istatistik gerçek kaynak çözünürlüğünü (wcSrcW/H) gösterir. Headless 4K kanıtlı.
  v0.3.4 TV'ye kurulamadı (TV kapalıydı) — sonraki fırsatta kur (TV etkilenmez,
  yalnız sürüm eşitleme).
- **KULLANICI SONUÇLARI (v0.3.3, iPhone 16 Pro)**: 1080p ve 2K WebCodecs
  **boru <10ms** — hedef tutturuldu (eski tarayıcı tamponu 70-200ms idi).
  4K: boru 12-14ms ama WiFi'de ara stutter + erken yeniden bağlanma → v0.3.5:
  (a) istemci "disconnected" durumuna 4sn tolerans (çoğu geçici, kendiliğinden
  toparlar; hemen reconnect etme), (b) host SCTP birikme eşiği 3MB→2MB (taze
  kareye daha çabuk atla). KALAN GERÇEK: 35Mb/s 4K'yı WiFi+güvenilir kanalda
  telefona taşımak fizik sınırında — telefonda 4K için --bitrate 25 veya
  localStorage 'mirror.profile'='lite' önerilir (telefon ekranı zaten ~2.6K).
  v0.3.5 TV kurulumu bekliyor (TV kapalı; etkilenmiyor).
- **Ses kararsızlığı ÇÖZÜMLENDİ (teşhis, 2026-08-30)**: host suçsuz — sorun
  TELEFONDA: iOS/WebKit sekmesinin ses oturumu takılıyor; sayfa YENİLEME düzeltmiyor,
  tarayıcıyı tam kapatıp açmak düzeltiyor. Bizim tarafta düzeltilecek şey yok;
  kullanıcıya reçete: ses gelmezse tarayıcıyı tamamen kapat-aç (host'a dokunma).
- **OPTİMİZASYON FAZI KAPANIŞI (2026-08-30)**: 1080p/2K WebCodecs boru <10ms ✓.
  4K WiFi'de "biraz sıkıntılı" kaldı — kullanıcı bilinçli kabul etti ("şuanlık
  problem değil"); reçeteler kayıtlı (bitrate 25 / lite profil / Ethernet).
  İSTENMEDEN 4K-WiFi iyileştirmesine GİRME.
- **TV panel DONMASI çözüldü (v0.2.4)**: 2K/4K çözümü arka planda sürerken panel
  açılınca TV'nin ana iş parçacığı boğuluyor, TÜM tuşlar ölüyordu (kullanıcı fişi
  çekmek zorunda kaldı). Çözüm: panel açıkken video.pause() + video gizle (yayın
  kapanınca devam), tarama dalgaları arası 150ms nefes, Geri tuşu taramayı iptal
  eder, buton disabled tuzağı kaldırıldı (scanning bayrağı).
- Bilinen gözlem: WiFi mesafe/2.4GHz paraziti gecikme yapabiliyor — optimizasyon
  İSTENMEDİKÇE bu konuya girme (kullanıcı bilinçli erteledi; ilk çare 5GHz/Ethernet).

## Durum ve Yol Haritası

- **Faz 1 — Aynalama MVP: TAMAM** (kullanıcı testinden geçti).
- **Faz 1.5 — TAMAM (2026-08-30)**: ffmpeg motoru eklendi (`--engine auto|ffmpeg|mf`,
  varsayılan auto: ffmpeg varsa onu seçer). ffmpeg ddagrab → stdout Annex-B →
  `ffmpeg_engine.rs` AUD'lere (NAL tip 9) bölüp broadcast'e verir. Doğrulandı: **60 fps
  sabit; kullanıcı TV'de 58-64 fps, tampon 70-200 ms ölçtü.** Notlar:
  (1) Kodlayıcı otomatik zincirle seçilir: h264_nvenc → h264_qsv → h264_amf → libx264
  (donanım yoksa ffmpeg hemen ölür, kare sayacıyla anlaşılır, sıradaki denenir).
  AU ayracı evrensel `-bsf:v h264_metadata=aud=insert` ile eklenir (kodlayıcıya özgü
  -aud bayrağına güvenme). (2) **ffmpeg.exe exe'ye gömülü**: `host/assets/ffmpeg.exe`
  varsa build.rs `embedded_ffmpeg` cfg'sini açar, include_bytes! ile gömülür, ilk
  çalıştırmada %LOCALAPPDATA%\mirror-host\ altına çıkarılır → tek exe her makinede
  çalışır. Asset .gitignore'da; yenilemek için gyan.dev essentials build'inden
  ffmpeg.exe'yi assets/ altına koy. Lisans: gyan build'leri GPL — kişisel kullanım
  sorunsuz, ticari dağıtımda GPL yükümlülüklerine dikkat. (3) ffmpeg'e anlık IDR
  zorlatılamaz → GOP 1 sn (`-g fps`); İLK bağlantı ≤1 sn bekler (süreklilik gecikmesi
  DEĞİL — bağlantı sonrası gecikme tampon değeri kadardır). (4) Anahtar karede SPS/PPS
  yoksa önbellekten başa eklenir. (5) MF yolu yedek durur (--engine mf).
  (6) **tv-app istemcisi de exe'ye gömülü** (signaling.rs include_str!): diskte tv-app
  klasörü yoksa bellekten servis edilir → exe TEK BAŞINA tam çalışır (105 MB).
  Doğrulama (2026-08-30): temiz LOCALAPPDATA + klasörsüz çalıştırmada gömülü ffmpeg
  çıkarıldı, NVENC seçildi, 60fps; gömülü istemci HTTP 200 döndü.
- **Faz 2 — TAMAM (2026-08-30)**: (a) **Ses**: cpal WASAPI loopback (çıkış aygıtı girdi
  olarak açılır) → stereo indirgeme + doğrusal yeniden örnekleme (bu makine 192kHz!) →
  Opus 128kb/s 20ms çerçeveler (`audio.rs`) → oturum başına ses track'i (`session.rs`).
  Kapatmak: `--no-audio`. İstemci otomatik oynatma engeline karşı: önce sesli dene,
  engellenirse sessiz başla + ilk tuş/dokunuşta aç (unlockAudio). NOT: loopback yalnızca
  PC'de ses ÇALARKEN veri verir; sessizlikte RTP ses akışı durur (normal).
  (b) **Keşif**: host `/ping` ucu (CORS: *) + istemcide "Ağı Tara" butonu — kayıtlı ağ →
  sayfa ağı → 192.168.0/1.x sırasıyla paralel fetch taraması. mDNS KULLANILMADI (TV
  tarayıcısı yapamaz). (c) **Çoklu monitör imleci**: `cursor::output_rect` DXGI'den
  monitörün masaüstü dikdörtgenini alır; imleç o monitör dışındaysa TV'de gizlenir.
- **DERLEME NOTU**: opus-sys cmake ister; cmake VS BuildTools içinde ama PATH'te değil —
  kullanıcı ortamına `CMAKE` env değişkeni kalıcı yazıldı (yeni terminallerde geçerli).
  DXGI_OUTPUT_DESC için `Win32_Graphics_Gdi` feature'ı gerekir (HMONITOR).
- **Faz 3 — TAMAM (2026-08-30)**: Gerçek genişletme çalışıyor: `--extend` bayrağı
  sanal monitör takar ve onu yayınlar. Sürücü: **parsec-vdd 0.45** (WHQL imzalı,
  kuruldu; kurulum exe: builds.parsec.app/vdd/parsec-vdd-0.45.0.0.exe /S, admin ister).
  Neden parsec-vdd: programatik tak/çıkar (çalışmada admin istemez), monitör yalnız
  yayın sırasında var. Denetim `vdd.rs`: cihaz CM_Get_Device_Interface_ListW ile
  GUID {00b41627-04c4-429e-a26e-0265cf50c8fa} üzerinden bulunur; IOCTL'ler:
  ADD=0x0022e004 REMOVE=0x0022a008 UPDATE=0x0022a00c VERSION=0x0022e010 (overlapped I/O).
  **Sürücü 100ms'de bir UPDATE ister; kesilirse ~1sn'de monitörü kendisi söker** —
  keepalive iş parçacığı 50ms'de bir atar; süreç ölünce monitör otomatik gider
  (kasıtlı güvenlik ağı, hayalet monitör kalmaz). Yeni çıkış "önce/sonra" DXGI
  çıkış listesi karşılaştırmasıyla bulunur (bu makinede adaptör 0, çıkış 1 çıktı —
  adaptörler arası hwdownload yolu yazıldı ama gerekmedi; adapter_index>0 durumunda
  ffmpeg'e -init_hw_device + hwdownload,format=bgr0 eklenir). Doğrulama: 1920x1080
  sanal ekran NVENC ile 57-65 fps yayınlandı. MF motoru --extend desteklemez (bail).
  windows-rs notu: CreateFileW/CreateEventW için "Win32_Security" feature gerekir.
- **--mode WxH@Hz (2026-08-30)**: sanal monitör çözünürlüğü ChangeDisplaySettingsExW
  ile ayarlanır (vdd.rs set_display_mode; mod değişince rect yeniden okunur, cfg.capture_size
  geçirilir). Doğrulandı: 3840x2160@60 → NVENC 60fps @ ~18Mb/s (RTX 5070 rahat).
  TV tarafı gerçeği: H.264 4K120 neredeyse hiçbir çözücüde yok + Tizen uygulama
  katmanı 60Hz kompozit eder (120Hz sadece HDMI/game mode) → TV'de gerçekçi tavan
  4K@60. 4K'da bant ~25-40Mb/s: TV'ye Ethernet öner; Windows ölçeği %150 öner.
- **tv-audio SES BUG'ı çözüldü (2026-08-30)**: Steam Streaming Speakers, Steam yayını
  aktif değilken ses motorunu POMPALAMAZ → loopback ~hiç veri almaz (ölçüldü: 4
  paket/5sn; nominal 249). Çözüm: **VB-CABLE kuruldu** ("C:\Program Files\VB\CABLE\
  VBCABLE_Setup_x64.exe -i -h", UAC ister) → "CABLE Input" tercih 1'de, akış nominal.
  Steam'e düşülürse warn loglanır. Ek düzeltme: engage() hedef aygıtın sesini %100 +
  sessiz-kapalı yapar (IAudioEndpointVolume; loopback aygıt ses seviyesini kopyalar!).
  Teşhis aracı: audio.rs telemetri — 5sn'de bir akış durumu değişince
  "Ses akışı başladı (N paket/5sn)" / "veri gelmiyor" loglar.
- **"İlk açılışta ses yok" bug'ı (2026-08-30, v0.2.2)**: host suçsuzdu (telemetri 250
  paket/5sn gösterdi). Suçlu istemci: elle kurulan MediaStream'e oynatma başladıktan
  SONRA eklenen ses track'i bazı motorlarda seslendirilmiyor. Çözüm: `ev.streams[0]`
  (tarayıcının yönettiği akış — iki track de "mirror" stream id'sinde) kullan;
  elle MediaStream yalnız yedek. v0.2.2 TV'ye kuruldu (2026-08-30).
- Steam Streaming Speakers aygıtı kullanıcı isteğiyle DEVRE DIŞI bırakıldı
  (pnputil /disable-device "ROOT\SteamStreamingSpeakers\0000"; geri almak:
  /enable-device aynı ID — Steam Remote Play sesi için gerekir).
- **VB-CABLE exe'ye GÖMÜLMEZ**: (a) çekirdek sürücüsü, kurulumcu+admin şart;
  (b) VB-Audio lisansı yeniden dağıtımı izne bağlar (ffmpeg GPL'inden farklı).
  Bunun yerine panel "İndir ve kur" butonu sunar (tv-audio açık + CABLE yoksa
  görünür): resmî vb-audio.com zip'i PowerShell alt süreciyle indirilir,
  VBCABLE_Setup_x64.exe -i -h UAC ile koşulur (gui.rs install_cable).
- **--tv-audio (2026-08-30)**: loopback sesin KOPYASINI alır, PC'de de çalar (kullanıcı
  fark etti). Windows'ta ses pencere konumunu DEĞİL varsayılan aygıtı izler → çözüm
  gerçek HDMI TV davranışı: `audio_route.rs` IPolicyConfig (belgesiz COM,
  CLSID 870af99c-171d-4f9e-af0d-e63df40c2bc9, iface f8679f50-...; vtable sırası ABI!)
  ile varsayılan çıkışı sanal aygıta çevirir (tercih: VB-CABLE "CABLE Input" →
  "Steam Streaming Speakers" — bu makinede Steam'inki etkin), loopback onu yakalar,
  PC susar; eCommunications PC'de bırakılır. Çıkışta geri alınır; önceki aygıt
  %LOCALAPPDATA%\mirror-host\audio-restore.id'ye yazılır → çökme sonrası
  `--restore-audio` veya bir sonraki engage otomatik düzeltir.
- **Aynı-PC izleyicide ses kapalı (2026-08-30)**: aynı makineden tarayıcıyla izlerken
  yayın sesi → loopback → yayın... geri besleme döngüsü oluşur (belirti: takılı
  ileri-geri tekrar). session.rs `is_same_machine`: UDP connect hilesiyle peer IP ==
  kendi IP'si tespit edilir, o oturuma ses track'i hiç eklenmez. TV/telefon etkilenmez.
- **TV uygulamasında GERİ tuşu (10009, kayıt gerektirmez) menüyü açar/kapatır**
  (v0.2.1): bağlıyken yayın arkada sürer; Çıkış butonu tizen.application...exit().
  Bağlantı yokken Geri = çık. Esc(27) tarayıcıda aynı işi görür.
- **--cursor-follow (2026-08-30)**: KDE/GNOME tarzı imleç-odağı-izler; Windows'ta
  yerleşik yok → `focus_follow.rs` EVENT_SYSTEM_FOREGROUND kancası (SetWinEventHook,
  "Win32_UI_Accessibility" feature; OUTOFCONTEXT → kanca iş parçacığında mesaj
  döngüsü ŞART). Filtre: yalnız WS_CAPTION'lı pencereler (Alt+Tab anahtarlayıcısı
  ve kenarlıksız oyunlar elenir). Kullanıcı doğrulaması bekliyor.
- **4K iPhone doğrulaması (2026-08-30)**: iPhone 16 Pro Chrome(WebKit) 3840x2160@60
  akışı donanımda çözdü; tampon 100-200ms. 35Mb/s WiFi'de ara ara paket kaybı +
  küçük kırılmalar — normal (NACK + 1sn GOP toparlar); TV için Ethernet önerildi.
- **Pencere yerleşimi (kullanıcı sorusu, kod YOK)**: yeni pencereler imlecin olduğu
  monitörde DEĞİL uygulamanın son hatırlanan monitöründe açılır — GERÇEK çift
  monitörde de böyledir, bizim kusurumuz değil. Çare: pencereyi bir kez sanal ekrana
  taşıyıp orada kapat (Windows hatırlar) ya da PowerToys FancyZones
  "yeni pencereleri etkin monitörde aç". İstenirse SetWinEventHook ile otomatik
  taşıyıcı yazılabilir (deneysel bayrak olarak) — kullanıcı isterse yapılacak.
- **Kodlayıcı denemesi ölçütü**: kare sayısı DEĞİL "süreç 3 sn hayatta mı" —
  masaüstü tamamen hareketsizken ilk kare hiç gelmez, kare beklemek NVENC'i
  yanlışlıkla eler (yaşandı). windows-rs `#[interface]` makrosu üstünde başka
  öznitelik kabul etmez → `#![allow]` modül seviyesine.
