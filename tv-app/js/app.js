'use strict';

/* PC Mirror — TV istemcisi.
   Akış: WebSocket sinyalleşme → WebRTC H.264 video + "cursor" veri kanalı.
   Aynı dosya TV'de (Tizen web uygulaması) ve normal tarayıcıda çalışır. */

const video = document.getElementById('screen');
const cast = document.getElementById('cast');
const castCtx = cast.getContext('2d');
const cursorEl = document.getElementById('cursor');

/* Tek seferlik onarım: eski sürümün yapışkan RTP geri-dönüşünü temizle
   (WebCodecs siyah ekran bug'ı v0.3.1'de çözüldü; cihazlar tekrar denesin). */
if (localStorage.getItem('mirror.cv') !== '2') {
  localStorage.setItem('mirror.cv', '2');
  localStorage.removeItem('mirror.transport');
}

/* Video yolu seçimi:
   - webcodecs: ham H.264 veri kanalından gelir, VideoDecoder donanımda çözer,
     canvas'a anında çizilir → tarayıcının 70-200ms jitter tamponu DEVRE DIŞI.
   - rtp: klasik WebRTC yolu (TV'nin eski motoru WebCodecs bilmez).
   Geçersiz kılma: localStorage.setItem('mirror.transport', 'rtp'|'webcodecs') */
const TRANSPORT = localStorage.getItem('mirror.transport')
  || ((!window.tizen && window.VideoDecoder) ? 'webcodecs' : 'rtp');
const USE_WEBCODECS = TRANSPORT === 'webcodecs';

function activeScreen() { return USE_WEBCODECS ? cast : video; }
function sourceDims() {
  return USE_WEBCODECS
    ? { w: cast.width, h: cast.height }
    : { w: video.videoWidth, h: video.videoHeight };
}
const statusEl = document.getElementById('status');
const statsEl = document.getElementById('stats');
const setupEl = document.getElementById('setup');
const hostInput = document.getElementById('host-input');

let pc = null;
let ws = null;
let reconnectDelay = 1000;
let reconnecting = false;
let statsTimer = null;
let pingTimer = null;
let rttMs = 0;
let discTimer = null; // "disconnected" tolerans sayacı

/* --- WebCodecs çözücü durumu --- */
let decoder = null;
let gotKey = false;
let wcFrames = 0;
let wcBytes = 0;
let wcPipeMs = 0; // veri kanalından ekrana: alım→çözüm gecikmesi (EMA)
let wcSrcW = 0, wcSrcH = 0; // akışın gerçek çözünürlüğü (istatistik için)
const recvTimes = new Map(); // chunk ts (µs) → performance.now()

/* Canvas'ı CİHAZ EKRANINA sınırla: telefonda 4K canvas'a çizmek bellek/dolgu
   israfı — WebKit sekme sınırını aşıp ÇÖKÜYOR (yaşandı). Küçültmeyi drawImage
   sırasında GPU yapar; telefon ekranında fark gözle görülmez. */
function canvasTarget(frame) {
  const dpr = window.devicePixelRatio || 1;
  const maxW = Math.round((window.innerWidth || 1920) * dpr);
  const maxH = Math.round((window.innerHeight || 1080) * dpr);
  const scale = Math.min(1, maxW / frame.displayWidth, maxH / frame.displayHeight);
  return {
    w: Math.max(2, Math.round(frame.displayWidth * scale)),
    h: Math.max(2, Math.round(frame.displayHeight * scale)),
  };
}

function fallbackToRtp(err) {
  console.warn('WebCodecs başarısız, RTP yoluna dönülüyor:', err);
  if (!sessionStorage.getItem('wcFailed')) {
    sessionStorage.setItem('wcFailed', '1');
    localStorage.setItem('mirror.transport', 'rtp');
    location.reload();
  }
}

/* Çözücü hatasında sayfayı YENİLEME (ekran kararıyordu): çözücüyü yerinde
   sıfırla, sonraki anahtar kareyle devam et. Üst üste 5 hata olursa RTP'ye dön. */
let wcResets = 0;
function resetDecoder(err) {
  console.warn('[wc] çözücü sıfırlanıyor:', err && err.message);
  try { if (decoder) decoder.close(); } catch (e) {}
  decoder = null;
  gotKey = false;
  asmTs = null;
  asmParts = [];
  wcResets++;
  if (wcResets > 5) fallbackToRtp(err);
}

function initDecoder() {
  decoder = new VideoDecoder({
    output: (frame) => {
      if (wcFrames === 0) console.log('[wc] ilk kare çözüldü', frame.displayWidth + 'x' + frame.displayHeight);
      const t0 = recvTimes.get(frame.timestamp);
      if (t0 !== undefined) {
        wcPipeMs = wcPipeMs * 0.9 + (performance.now() - t0) * 0.1;
        recvTimes.delete(frame.timestamp);
      }
      wcSrcW = frame.displayWidth;
      wcSrcH = frame.displayHeight;
      const t = canvasTarget(frame);
      if (cast.width !== t.w || cast.height !== t.h) {
        cast.width = t.w;
        cast.height = t.h;
      }
      castCtx.drawImage(frame, 0, 0, t.w, t.h);
      frame.close();
      wcFrames++;
    },
    error: (e) => { console.error('[wc] çözücü hatası:', e && e.message); resetDecoder(e); },
  });
  // Annex-B H.264: description vermeyince tarayıcı Annex-B bekler.
  // Codec dizesi High@L5.2: 4K60'ı kapsar — baseline L3.1 (42e01f) 4K'da bazı
  // çözücüleri sessizce boğuyordu (PC'de 4K siyah ekran bug'ı).
  decoder.configure({
    codec: 'avc1.640034',
    optimizeForLatency: true,
    hardwareAcceleration: 'prefer-hardware',
  });
  console.log('[wc] çözücü kuruldu');
}

/* Host, AU'ları 16KB parçalar halinde yollar (büyük tek mesaj kanalı öldürür):
   [1B bayrak: bit0=anahtar, bit1=son parça][8B ts][veri]. Kanal sıralı olduğundan
   parçalar sırayla gelir; "son" bayrağında birleştirip çözücüye veririz. */
let wcChunks = 0;
let asmTs = null;
let asmKey = false;
let asmParts = [];
let asmLen = 0;

function onVideoChunk(ev) {
  try {
    const dv = new DataView(ev.data);
    const flags = dv.getUint8(0);
    const isKey = (flags & 1) === 1;
    const isLast = (flags & 2) === 2;
    const tsUs = Math.round(Number(dv.getBigUint64(1, true)) / 10); // 100ns → µs
    wcBytes += ev.data.byteLength;
    if (wcChunks++ === 0) console.log('[wc] ilk parça geldi', ev.data.byteLength, 'bayt');

    if (asmTs !== tsUs) { // yeni AU başladı
      asmTs = tsUs;
      asmKey = isKey;
      asmParts = [];
      asmLen = 0;
    }
    asmParts.push(new Uint8Array(ev.data, 9));
    asmLen += ev.data.byteLength - 9;
    if (!isLast) return;

    // AU tamam — birleştir.
    const data = new Uint8Array(asmLen);
    let off = 0;
    for (let i = 0; i < asmParts.length; i++) {
      data.set(asmParts[i], off);
      off += asmParts[i].length;
    }
    const key = asmKey;
    asmTs = null;
    asmParts = [];

    if (!decoder) initDecoder();
    if (!gotKey) {
      if (!key) return;
      gotKey = true;
    }
    // Çözücü geride kaldıysa ara kareleri at, anahtar karede toparla.
    if (decoder.decodeQueueSize > 4) {
      if (!key) { gotKey = false; return; }
    }
    if (recvTimes.size > 240) recvTimes.clear();
    recvTimes.set(tsUs, performance.now());
    decoder.decode(new EncodedVideoChunk({
      type: key ? 'key' : 'delta',
      timestamp: tsUs,
      data: data,
    }));
  } catch (e) {
    resetDecoder(e);
  }
}

function setStatus(msg) {
  statusEl.textContent = msg;
  statusEl.style.display = msg ? 'block' : 'none';
}

/* PC'nin adresi: sayfa PC'den (http) servis edildiyse otomatik;
   Tizen paketi (file://) olarak açıldıysa bir kez sorulur, hatırlanır. */
function serverAddress() {
  if (location.protocol.indexOf('http') === 0 && location.host) return location.host;
  return localStorage.getItem('mirror.server');
}

function panelOpen() {
  return !setupEl.classList.contains('hidden');
}

/* Panel aç/kapa — TV'de KRİTİK: 2K/4K çözümü arka planda TV'nin tüm gücünü yer,
   panel tuşlara yanıt veremez hale gelir (yaşandı: güç kesmeden çıkılamadı).
   Panel açıkken video DURDURULUP GİZLENİR; kapanınca kaldığı yerden sürer. */
function setPanelVisible(show) {
  if (show) {
    setStatus('');
    setupEl.classList.remove('hidden');
    hostInput.value = localStorage.getItem('mirror.server') || '192.168.1.';
    if (pc) {
      try { video.pause(); } catch (e) {}
      activeScreen().classList.add('hidden');
    }
    connectBtn.focus();
  } else {
    setupEl.classList.add('hidden');
    try { if (document.activeElement) document.activeElement.blur(); } catch (e) {}
    activeScreen().classList.remove('hidden');
    if (pc) video.play().catch(function () {});
  }
}

function showSetup() {
  setPanelVisible(true);
  hostInput.focus();
}

const connectBtn = document.getElementById('connect-btn');
const scanBtn = document.getElementById('scan-btn');
const exitBtn = document.getElementById('exit-btn');
const hintEl = document.getElementById('setup-hint');

/* Mevcut bağlantıyı sessizce kapat (yeniden bağlanma zinciri tetiklenmeden). */
function closeCurrent() {
  stopStats();
  if (ws) { ws.onclose = null; try { ws.close(); } catch (e) {} ws = null; }
  if (pc) { try { pc.close(); } catch (e) {} pc = null; }
}

function exitApp() {
  closeCurrent();
  try { tizen.application.getCurrentApplication().exit(); } catch (e) { window.close(); }
}
exitBtn.addEventListener('click', exitApp);

function doConnect() {
  const addr = hostInput.value.trim();
  if (!addr) return;
  localStorage.setItem('mirror.server', addr.indexOf(':') >= 0 ? addr : addr + ':47000');
  closeCurrent();
  setPanelVisible(false);
  connect();
}

connectBtn.addEventListener('click', doConnect);

/* --- Ağ tarama: /ping ucuna cevap veren bilgisayarı bul --- */
function probe(ip) {
  return new Promise((resolve) => {
    const ctl = new AbortController();
    const timer = setTimeout(() => { ctl.abort(); resolve(null); }, 700);
    fetch('http://' + ip + ':47000/ping', { signal: ctl.signal })
      .then((r) => r.json())
      .then((j) => { clearTimeout(timer); resolve(j && j.app === 'mirror-host' ? ip : null); })
      .catch(() => { clearTimeout(timer); resolve(null); });
  });
}

let scanning = false;
let scanCancel = false;

async function scanNetwork() {
  if (scanning) return;
  scanning = true;
  scanCancel = false;
  scanBtn.textContent = 'Taranıyor…';
  // Aday alt ağlar: kayıtlı adresin ağı → sayfanın geldiği ağ → yaygın ev ağları.
  const bases = [];
  const saved = localStorage.getItem('mirror.server');
  if (saved) bases.push(saved.split(':')[0].split('.').slice(0, 3).join('.') + '.');
  if (location.hostname && location.hostname.indexOf('.') > 0) {
    bases.push(location.hostname.split('.').slice(0, 3).join('.') + '.');
  }
  bases.push('192.168.0.', '192.168.1.');
  const uniq = bases.filter((b, i) => bases.indexOf(b) === i);

  try {
    for (const base of uniq) {
      for (let start = 1; start <= 254; start += 16) {
        if (scanCancel) {
          hintEl.textContent = 'Tarama iptal edildi (Geri tuşu).';
          return;
        }
        hintEl.textContent = 'Taranıyor: ' + base + start + '…';
        const batch = [];
        for (let i = start; i < Math.min(start + 16, 255); i++) batch.push(probe(base + i));
        const found = (await Promise.all(batch)).find((r) => r);
        if (found) {
          hintEl.textContent = 'Bulundu: ' + found;
          hostInput.value = found + ':47000';
          doConnect();
          return;
        }
        // Arayüze nefes: TV'nin motoru tuş olaylarını işleyebilsin.
        await new Promise((r) => setTimeout(r, 150));
      }
    }
    hintEl.textContent = 'Bulunamadı — bilgisayarda mirror-host açık mı? IP\'yi elle de girebilirsiniz.';
  } catch (e) {
    hintEl.textContent = 'Tarama hatası: ' + e;
  } finally {
    scanning = false;
    scanBtn.textContent = 'Ağı Tara';
  }
}

scanBtn.addEventListener('click', scanNetwork);

/* TV kumandası gezinmesi: tarayıcı ok tuşlarıyla alanlar arasında kendiliğinden
   gezmez. Oklarla input ↔ butonlar arasında gez; OK/Enter etkinleştirir.
   (keyCode kullanılır; eski Tizen Chromium'da ev.key güvenilir değil.) */
hostInput.addEventListener('keydown', (ev) => {
  if (ev.keyCode === 13) { ev.preventDefault(); doConnect(); }        // OK/Enter
  else if (ev.keyCode === 40) { ev.preventDefault(); connectBtn.focus(); } // aşağı
});
connectBtn.addEventListener('keydown', (ev) => {
  if (ev.keyCode === 13) { ev.preventDefault(); doConnect(); }
  else if (ev.keyCode === 38) { ev.preventDefault(); hostInput.focus(); } // yukarı
  else if (ev.keyCode === 39) { ev.preventDefault(); scanBtn.focus(); }   // sağ
});
scanBtn.addEventListener('keydown', (ev) => {
  if (ev.keyCode === 13) { ev.preventDefault(); scanNetwork(); }
  else if (ev.keyCode === 37) { ev.preventDefault(); connectBtn.focus(); } // sol
  else if (ev.keyCode === 39) { ev.preventDefault(); exitBtn.focus(); }    // sağ
  else if (ev.keyCode === 38) { ev.preventDefault(); hostInput.focus(); }  // yukarı
});
exitBtn.addEventListener('keydown', (ev) => {
  if (ev.keyCode === 13) { ev.preventDefault(); exitApp(); }
  else if (ev.keyCode === 37) { ev.preventDefault(); scanBtn.focus(); }   // sol
  else if (ev.keyCode === 38) { ev.preventDefault(); hostInput.focus(); } // yukarı
});

/* Kumandada GERİ (10009) / klavyede Esc: menüyü aç-kapat.
   Tarama sürüyorsa önce onu iptal eder — panelden HER ZAMAN çıkış vardır. */
document.addEventListener('keydown', (ev) => {
  if (ev.keyCode === 10009 || ev.keyCode === 27) {
    ev.preventDefault();
    if (scanning) {
      scanCancel = true;
      return;
    }
    if (!panelOpen()) {
      setPanelVisible(true);
    } else if (pc) {
      setPanelVisible(false); // bağlıyken menüden yayına dön
    } else {
      exitApp(); // bağlantı yokken Geri = uygulamadan çık
    }
  }
});

async function connect() {
  const server = serverAddress();
  if (!server) { showSetup(); return; }
  setStatus('Bağlanılıyor: ' + server);

  ws = new WebSocket('ws://' + server + '/ws');

  ws.onopen = async () => {
    reconnectDelay = 1000;
    pc = new RTCPeerConnection({}); // LAN: STUN/TURN gerekmez

    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.addTransceiver('audio', { direction: 'recvonly' }); // PC sistem sesi (Opus)

    // İmleç kanalı: iki uçta da negotiated id 0 (host tarafıyla eşleşmeli).
    const dc = pc.createDataChannel('cursor', {
      negotiated: true, id: 0, ordered: false, maxRetransmits: 0,
    });
    dc.onmessage = (ev) => {
      try {
        const msg = JSON.parse(ev.data);
        if (msg.type === 'pong') { rttMs = performance.now() - msg.t; return; }
        drawCursor(msg);
      } catch (e) {}
    };
    // RTT ölçümü: 2 sn'de bir ping; host aynı mesajı pong yapıp geri yollar.
    dc.onopen = () => {
      if (pingTimer) clearInterval(pingTimer);
      pingTimer = setInterval(() => {
        if (dc.readyState === 'open') {
          dc.send(JSON.stringify({ type: 'ping', t: performance.now() }));
        }
      }, 2000);
    };

    // WebCodecs kanalı: ham AU'lar (host tarafıyla eşleşen negotiated id 2;
    // tek numaralı id DTLS rolüne takılıp açılmıyor — çift numara şart).
    const dcv = pc.createDataChannel('video', { negotiated: true, id: 2 });
    dcv.binaryType = 'arraybuffer';
    if (USE_WEBCODECS) {
      cast.classList.remove('hidden');
      video.classList.add('hidden'); // ses için çalar, görüntüsü kullanılmaz
      dcv.onopen = () => console.log('[wc] video kanalı açıldı');
      dcv.onmessage = onVideoChunk;
    }
    console.log('[app] video yolu:', TRANSPORT);

    const media = new MediaStream(); // yalnız yedek yol için
    pc.ontrack = (ev) => {
      // Gecikme ipuçları: oynatma tamponunu sıfıra çek (destekleyen motorlarda).
      try { ev.receiver.playoutDelayHint = 0; } catch (e) {}
      try { ev.receiver.jitterBufferTarget = 0; } catch (e) {}
      if (USE_WEBCODECS) {
        // Görüntü WebCodecs kanalından geliyor; medya elementine YALNIZ SES ver.
        // Kare üretmeyen video track'i eklersek WebKit elementi "bekleme"de
        // tutup sesi hiç başlatmıyor (telefonda sessizlik bug'ı).
        if (ev.track.kind !== 'audio') return;
        video.srcObject = new MediaStream([ev.track]);
        tryPlay();
        setStatus('');
        startStats();
        return;
      }
      // ÖNEMLİ: tarayıcının kendi yönettiği akışı kullan (ev.streams[0]).
      // Elle kurulan MediaStream'e oynatma başladıktan SONRA eklenen ses kanalı
      // bazı motorlarda hiç seslendirilmiyordu (ilk açılışta ses yok bug'ı).
      let stream;
      if (ev.streams && ev.streams[0]) {
        stream = ev.streams[0];
      } else {
        media.addTrack(ev.track);
        stream = media;
      }
      if (video.srcObject !== stream) video.srcObject = stream;
      tryPlay();
      setStatus('');
      startStats();
    };

    pc.onconnectionstatechange = () => {
      const st = pc.connectionState;
      if (st === 'connected') {
        if (discTimer) { clearTimeout(discTimer); discTimer = null; }
        return;
      }
      if (st === 'failed' || st === 'closed') { scheduleReconnect(); return; }
      if (st === 'disconnected') {
        // Çoğu zaman GEÇİCİ (WiFi/yük dalgası) ve kendiliğinden toparlar —
        // hemen kopartma, 4 sn tolere et (4K'da gereksiz yeniden bağlanma bug'ı).
        if (!discTimer) {
          discTimer = setTimeout(() => {
            discTimer = null;
            if (pc && pc.connectionState !== 'connected') scheduleReconnect();
          }, 4000);
        }
      }
    };

    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await iceGatheringComplete(pc);
    // Profil: TV "lite" der → host 1080p hafif akışı gönderir (TV'nin çözücüsü
    // 2K/4K'da boğuluyor; TV zaten 4K panele kendisi büyütüyor). Elle geçersiz
    // kılmak için: localStorage.setItem('mirror.profile', 'full' | 'lite')
    const profile = localStorage.getItem('mirror.profile') || (window.tizen ? 'lite' : 'full');
    ws.send(JSON.stringify({
      type: 'offer',
      sdp: pc.localDescription.sdp,
      profile: profile,
      video_path: TRANSPORT,
    }));
    startFrameProbe();
  };

  ws.onmessage = async (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.type === 'answer' && pc) {
      await pc.setRemoteDescription({ type: 'answer', sdp: msg.sdp });
    }
  };

  ws.onclose = scheduleReconnect;
  ws.onerror = () => {};
}

/* --- Ses açma ---
   Tarayıcılar kullanıcı etkileşimi olmadan sesli otomatik oynatmayı engelleyebilir.
   Önce sesli dene; engellenirse sessiz başlat, ilk tuş/dokunuşta sesi aç. */
let audioUnlocked = false;
function tryPlay() {
  video.muted = false;
  video.play().then(() => { audioUnlocked = true; }).catch(() => {
    video.muted = true;
    video.play().catch(() => {});
  });
}
function unlockAudio() {
  if (audioUnlocked) return;
  video.muted = false;
  video.play().then(() => { audioUnlocked = true; }).catch(() => { video.muted = true; });
}
document.addEventListener('keydown', unlockAudio);
document.addEventListener('click', unlockAudio);
document.addEventListener('touchstart', unlockAudio);

/* Trickle ICE kullanmıyoruz: LAN'da adaylar anında toplanır,
   SDP'yi komple göndermek hem basit hem hızlı. */
function iceGatheringComplete(conn) {
  if (conn.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    const check = () => {
      if (conn.iceGatheringState === 'complete') {
        conn.removeEventListener('icegatheringstatechange', check);
        resolve();
      }
    };
    conn.addEventListener('icegatheringstatechange', check);
    setTimeout(resolve, 2000); // güvenlik ağı
  });
}

function scheduleReconnect() {
  if (reconnecting) return;
  reconnecting = true;
  stopStats();
  cursorEl.style.display = 'none';
  try { if (pc) pc.close(); } catch (e) {}
  pc = null;
  setStatus('Bağlantı koptu, yeniden deneniyor…');
  setTimeout(() => { reconnecting = false; connect(); }, reconnectDelay);
  reconnectDelay = Math.min(reconnectDelay * 2, 5000);
}

/* --- İmleç ---
   Host 0..1 aralığında normalize koordinat gönderir. object-fit: contain
   nedeniyle videonun ekranda kapladığı gerçek dikdörtgeni hesaplayıp
   imleci oraya yerleştiririz. */
function drawCursor(c) {
  if (!c.visible) { cursorEl.style.display = 'none'; return; }
  const d = sourceDims();
  const vw = d.w, vh = d.h;
  if (!vw || !vh) return;
  const el = activeScreen();
  const rw = el.clientWidth, rh = el.clientHeight;
  const scale = Math.min(rw / vw, rh / vh);
  const dw = vw * scale, dh = vh * scale;
  const ox = (rw - dw) / 2, oy = (rh - dh) / 2;
  cursorEl.style.display = 'block';
  cursorEl.style.transform =
    'translate3d(' + (ox + c.x * dw) + 'px, ' + (oy + c.y * dh) + 'px, 0)';
}

/* --- İstatistik kaplaması: kumandada/klavyede '0' veya 'i' --- */
let prevBytes = 0, prevFrames = 0, prevTs = 0;
let prevWcFrames = 0, prevWcBytes = 0;
let rtpPipeMs = 0; // RTP modunda alım→gösterim süresi (rVFC destekliyorsa)

/* Kare bazlı boru ölçümü (yalnız RTP modu, Chrome 83+ / Safari 15.4+):
   gösterilen her kare için "ağdan geldi → ekrana çizildi" süresi ölçülür. */
function startFrameProbe() {
  if (USE_WEBCODECS || !video.requestVideoFrameCallback) return;
  const cb = (now, meta) => {
    if (meta.receiveTime && meta.expectedDisplayTime) {
      rtpPipeMs = rtpPipeMs * 0.9 + (meta.expectedDisplayTime - meta.receiveTime) * 0.1;
    }
    video.requestVideoFrameCallback(cb);
  };
  video.requestVideoFrameCallback(cb);
}

function rttText() {
  return rttMs ? ' | RTT ' + rttMs.toFixed(0) + ' ms' : '';
}

function startStats() {
  stopStats();
  prevBytes = prevFrames = prevTs = 0;
  prevWcFrames = prevWcBytes = 0;
  statsTimer = setInterval(async () => {
    if (!pc || statsEl.classList.contains('hidden')) return;

    if (USE_WEBCODECS) {
      const fps = wcFrames - prevWcFrames;
      const mbps = ((wcBytes - prevWcBytes) * 8 / 1e6).toFixed(1);
      prevWcFrames = wcFrames;
      prevWcBytes = wcBytes;
      statsEl.textContent =
        (wcSrcW || cast.width) + 'x' + (wcSrcH || cast.height) + ' | ' + fps + ' fps | ' + mbps +
        ' Mb/s | boru ~' + wcPipeMs.toFixed(0) + ' ms' + rttText() + ' | WebCodecs';
      return;
    }

    const stats = await pc.getStats();
    stats.forEach((r) => {
      if (r.type === 'inbound-rtp' && (r.kind === 'video' || r.mediaType === 'video')) {
        if (prevTs) {
          const dt = (r.timestamp - prevTs) / 1000;
          const mbps = ((r.bytesReceived - prevBytes) * 8 / dt / 1e6).toFixed(1);
          const fps = Math.round((r.framesDecoded - prevFrames) / dt);
          const buf = (r.jitterBufferDelay && r.jitterBufferEmittedCount)
            ? (r.jitterBufferDelay / r.jitterBufferEmittedCount * 1000).toFixed(0)
            : '?';
          // Eski Tizen Chromium inbound-rtp'de frameWidth/Height vermez; videodan al.
          const w = r.frameWidth || video.videoWidth || '?';
          const h = r.frameHeight || video.videoHeight || '?';
          const pipe = rtpPipeMs ? ' | boru ~' + rtpPipeMs.toFixed(0) + ' ms' : '';
          statsEl.textContent =
            w + 'x' + h + ' | ' + fps + ' fps | ' +
            mbps + ' Mb/s | tampon ~' + buf + ' ms' + pipe + rttText() +
            ' | kayıp: ' + (r.packetsLost || 0);
        }
        prevBytes = r.bytesReceived;
        prevFrames = r.framesDecoded;
        prevTs = r.timestamp;
      }
    });
  }, 1000);
}

function stopStats() {
  if (statsTimer) { clearInterval(statsTimer); statsTimer = null; }
}

/* Tizen TV'de sayı tuşları uygulamaya ancak kayıt edilirse ulaşır
   (ok/OK/Geri hariç tüm kumanda tuşları için geçerli). Not: yeni minimalist
   Samsung kumandalarında fiziksel rakam tuşu YOK — bu yüzden esas kısayol OK. */
try {
  if (window.tizen && tizen.tvinputdevice) tizen.tvinputdevice.registerKey('0');
} catch (e) {}

function toggleStats() {
  statsEl.classList.toggle('hidden');
}

document.addEventListener('keydown', (ev) => {
  // Kurulum panelindeki Enter zaten "bağlan" demek; onu istatistiğe sayma.
  if (ev.defaultPrevented) return;
  const inSetup = !setupEl.classList.contains('hidden');
  if (ev.keyCode === 48 || ev.key === '0' || ev.keyCode === 73 || ev.key === 'i') {
    toggleStats(); // klavyeli cihazlar: 0 veya i
  } else if (ev.keyCode === 13 && !inSetup) {
    toggleStats(); // TV kumandası: yayın ekranında OK tuşu
  }
});

// Tarayıcı/telefon/tablet: görünen yüzeye çift tıklama ya da çift dokunma
// (RTP modunda video, WebCodecs modunda canvas görünür — ikisine de bağla).
video.addEventListener('dblclick', toggleStats);
cast.addEventListener('dblclick', toggleStats);

connect();
