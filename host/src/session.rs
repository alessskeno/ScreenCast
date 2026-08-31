//! Her izleyici (TV / tarayıcı) için bir WebRTC oturumu.
//!
//! Neden WebRTC? UDP tabanlı (TCP kafa-kuyruk beklemesi yok), DTLS-SRTP ile
//! şifreli, paket kaybını NACK/PLI ile toparlıyor ve TV tarayıcısında alıcı
//! tarafı hazır — donanım çözücüyü tarayıcı kendisi kullanıyor.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use tokio::sync::broadcast;
use tracing::{info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::udp_network::{EphemeralUDP, UDPNetwork};
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use crate::protocol::SignalMessage;
use crate::signaling::AppState;

pub async fn run(socket: WebSocket, state: AppState, peer: std::net::SocketAddr) {
    if let Err(e) = drive(socket, state, peer).await {
        warn!("Oturum hatayla bitti: {e:#}");
    }
}

/// İzleyici bu makinenin kendisi mi? (Aynı PC'de tarayıcıyla izleme durumu.)
/// Kendi IP'mize giden yolun kaynak IP'si yine kendisiyse bağlantı yereldir.
/// ICE'nin kullanacağı UDP port aralığı (dahil). Güvenlik duvarında yalnız
/// bunlar + HTTP portu açılır. Aynı anda birkaç izleyici için fazlasıyla yeterli.
pub const ICE_PORT_MIN: u16 = 47100;
pub const ICE_PORT_MAX: u16 = 47120;

fn is_same_machine(peer: &std::net::SocketAddr) -> bool {
    if peer.ip().is_loopback() {
        return true;
    }
    let bind_addr = if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    std::net::UdpSocket::bind(bind_addr)
        .and_then(|s| {
            s.connect((peer.ip(), 9))?;
            s.local_addr()
        })
        .map(|local| local.ip() == peer.ip())
        .unwrap_or(false)
}

async fn drive(mut ws: WebSocket, state: AppState, peer: std::net::SocketAddr) -> Result<()> {
    // Aynı makineden izleyiciye ses GÖNDERİLMEZ: tarayıcı yayının sesini çalar,
    // loopback onu tekrar yakalar → sonsuz geri besleme döngüsü (yaşandı: takılı
    // ileri-geri tekrar). TV/telefon gibi başka cihazlar etkilenmez.
    let local_viewer = is_same_machine(&peer);
    if local_viewer {
        info!("Yeni izleyici bağlandı ({peer} — bu makine; geri besleme önlemek için ses kapalı)");
    } else {
        info!("Yeni izleyici bağlandı ({peer})");
    }

    // Yalnızca H.264 sun: TV'nin donanım çözücüsü bu; VP8/VP9 pazarlığına girme.
    let mut media = MediaEngine::default();
    media.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 102,
            ..Default::default()
        },
        RTPCodecType::Video,
    )?;
    // Ses: Opus 48 kHz stereo (WebRTC'nin standart ses kodeği).
    media.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media)?;

    // ICE için DAR ve SABİT bir UDP port aralığı kullan.
    //
    // Varsayılanda webrtc her oturumda çekirdeğin geçici port havuzundan
    // (Linux'ta 32768-60999) rastgele portlar seçer. Güvenlik duvarı olan bir
    // makinede bu, "video hiç gelmiyor" demektir: HTTP portu (47000) açılsa
    // bile sayfa yüklenir ama medya bağlanamaz — ya da 28 bin portu birden
    // açmak gerekir. Sabit aralıkla kullanıcı yalnız birkaç portu açar.
    let mut settings = SettingEngine::default();
    settings.set_udp_network(UDPNetwork::Ephemeral(EphemeralUDP::new(
        ICE_PORT_MIN,
        ICE_PORT_MAX,
    )?));

    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .build();

    // LAN içi bağlantı: STUN/TURN gerekmez, yerel adaylar yeter.
    let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);

    let track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability { mime_type: MIME_TYPE_H264.to_owned(), ..Default::default() },
        "video".to_owned(),
        "mirror".to_owned(),
    ));
    let sender = pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // İzleyici "kare kaybettim" (PLI) derse kodlayıcıya anahtar kare zorlat.
    let pli_task = {
        let keyframe = state.keyframe_request.clone();
        tokio::spawn(async move {
            loop {
                match sender.read_rtcp().await {
                    Ok((packets, _)) => {
                        if packets
                            .iter()
                            .any(|p| p.as_any().downcast_ref::<PictureLossIndication>().is_some())
                        {
                            keyframe.store(true, Ordering::SeqCst);
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    };

    // Ses track'i (ses yakalama açıksa ve izleyici başka cihazsa).
    let mut audio_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    if let Some(audio_tx) = state.audio_tx.as_ref().filter(|_| !local_viewer) {
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability { mime_type: MIME_TYPE_OPUS.to_owned(), ..Default::default() },
            "audio".to_owned(),
            "mirror".to_owned(),
        ));
        let audio_sender = pc
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        // RTCP'yi boşalt (kesici katmanın çalışması için okunması gerekir).
        audio_tasks.push(tokio::spawn(async move {
            while audio_sender.read_rtcp().await.is_ok() {}
        }));
        let mut audio_rx = audio_tx.subscribe();
        audio_tasks.push(tokio::spawn(async move {
            loop {
                match audio_rx.recv().await {
                    Ok(frame) => {
                        let sample = Sample {
                            data: frame.data.clone(),
                            duration: frame.duration,
                            ..Default::default()
                        };
                        if audio_track.write_sample(&sample).await.is_err() {
                            break;
                        }
                    }
                    // Ses karesi kaçtıysa sorun değil; Opus bir sonraki paketle toparlar.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }));
    }

    // İmleç kanalı: iki uçta da "negotiated, id 0" açılır → SDP pazarlığı beklemez.
    // Sırasız + yeniden gönderimsiz: geç kalan konumun değeri yok, en tazesi kazanır.
    let dc = pc
        .create_data_channel(
            "cursor",
            Some(RTCDataChannelInit {
                ordered: Some(false),
                max_retransmits: Some(0),
                negotiated: Some(0),
                ..Default::default()
            }),
        )
        .await?;
    {
        let dc_send = Arc::clone(&dc);
        let mut cursor_rx = state.cursor_rx.clone();
        dc.on_open(Box::new(move || {
            Box::pin(async move {
                while cursor_rx.changed().await.is_ok() {
                    let msg = {
                        let c = *cursor_rx.borrow();
                        serde_json::to_string(&c).unwrap_or_default()
                    };
                    if dc_send.send_text(msg).await.is_err() {
                        break;
                    }
                }
            })
        }));
    }
    // Gecikme ölçümü: istemcinin ping'ini aynen pong'a çevirip geri yolla (RTT).
    {
        let dc_reply = Arc::clone(&dc);
        dc.on_message(Box::new(move |msg| {
            let dc_reply = Arc::clone(&dc_reply);
            Box::pin(async move {
                if msg.is_string {
                    if let Ok(text) = std::str::from_utf8(&msg.data) {
                        if text.contains("\"ping\"") {
                            let _ = dc_reply.send_text(text.replace("\"ping\"", "\"pong\"")).await;
                        }
                    }
                }
            })
        }));
    }

    // WebCodecs yolu için ham AU kanalı (iki uçta da negotiated, id 2 —
    // ÇİFT numara ŞART: tek numaralı akış DTLS rol paylaşımına takılıp hiç
    // açılmıyor; id 1 ile yaşandı). Pompa on_open'da kurulur.
    let dc_video = pc
        .create_data_channel(
            "video",
            Some(RTCDataChannelInit { negotiated: Some(2), ..Default::default() }),
        )
        .await?;

    // Video pompası, istemcinin profili (tam / hafif 1080p) Offer'la öğrenilince
    // başlatılır — aşağıdaki sinyalleşme döngüsünde.
    let mut video_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut video_configured = false;

    pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        info!("WebRTC durumu: {s}");
        Box::pin(async {})
    }));

    // Sinyalleşme: TV offer yollar, biz answer döneriz.
    // Trickle yok — ICE adayları toplandıktan sonra SDP komple gider (LAN'da anlık).
    while let Some(msg) = ws.recv().await {
        let Ok(msg) = msg else { break };
        if let Message::Text(text) = msg {
            match serde_json::from_str::<SignalMessage>(text.as_str()) {
                Ok(SignalMessage::Offer { sdp, profile, video_path }) => {
                    // Profile göre akış seç: TV "lite" der ve 1080p alır (varsa).
                    if !video_configured {
                        video_configured = true;
                        let want_lite = profile.as_deref() == Some("lite");
                        let tx = match (&state.lite_tx, want_lite) {
                            (Some(lite), true) => {
                                info!("İzleyici hafif (1080p) akışa bağlandı");
                                lite.clone()
                            }
                            _ => state.encoded_tx.clone(),
                        };
                        if video_path.as_deref() == Some("webcodecs") {
                            // WebCodecs yolu: RTP yerine ham AU'lar veri kanalından.
                            // Tarayıcının 70-200ms'lik jitter tamponu devreden çıkar.
                            info!("İzleyici WebCodecs yolunu seçti (düşük gecikme)");
                            let dcv_outer = Arc::clone(&dc_video);
                            let keyframe = state.keyframe_request.clone();
                            dc_video.on_open(Box::new(move || {
                                let dcv = Arc::clone(&dcv_outer);
                                let mut rx = tx.subscribe();
                                let keyframe = keyframe.clone();
                                Box::pin(async move {
                                    info!("WebCodecs AU pompası başladı");
                                    keyframe.store(true, Ordering::SeqCst);
                                    let mut sent = 0u64;
                                    let mut wait_key = true;
                                    loop {
                                        match rx.recv().await {
                                            Ok(frame) => {
                                                if wait_key {
                                                    if !frame.is_keyframe {
                                                        continue;
                                                    }
                                                    wait_key = false;
                                                }
                                                // SCTP tamponu şiştiyse (yavaş istemci/ağ):
                                                // birikmiş gecikme = stutter; 2MB'ta (≈4K'da
                                                // ~2-3 karelik birikme) taze anahtar kareye atla.
                                                if dcv.buffered_amount().await > 2_000_000 {
                                                    wait_key = true;
                                                    keyframe.store(true, Ordering::SeqCst);
                                                    continue;
                                                }
                                                // KRİTİK: veri kanalında büyük tek mesaj GÖNDERME —
                                                // 4K anahtar karesi 500KB+ olabiliyor ve tarayıcının
                                                // mesaj sınırını aşınca kanal sessizce ölüyor (yaşandı).
                                                // AU'yu 16KB parçalara böl; kanal sıralı+güvenilir
                                                // olduğundan istemci sırayla birleştirir.
                                                // Parça: [1B bayrak: bit0=anahtar, bit1=son][8B ts][veri]
                                                const CHUNK: usize = 16 * 1024;
                                                let ts = (frame.ts_100ns as u64).to_le_bytes();
                                                let total = frame.data.len();
                                                let mut off = 0usize;
                                                let mut fail = false;
                                                while off < total {
                                                    let end = (off + CHUNK).min(total);
                                                    let last = end == total;
                                                    let mut buf =
                                                        Vec::with_capacity(9 + (end - off));
                                                    buf.push(
                                                        (frame.is_keyframe as u8)
                                                            | ((last as u8) << 1),
                                                    );
                                                    buf.extend_from_slice(&ts);
                                                    buf.extend_from_slice(&frame.data[off..end]);
                                                    if let Err(e) =
                                                        dcv.send(&bytes::Bytes::from(buf)).await
                                                    {
                                                        warn!("AU parçası gönderilemedi: {e}");
                                                        fail = true;
                                                        break;
                                                    }
                                                    off = end;
                                                }
                                                if fail {
                                                    break;
                                                }
                                                sent += 1;
                                                if sent == 1 {
                                                    info!("İlk AU gönderildi ({total} bayt)");
                                                }
                                            }
                                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                                wait_key = true;
                                                keyframe.store(true, Ordering::SeqCst);
                                            }
                                            Err(broadcast::error::RecvError::Closed) => break,
                                        }
                                    }
                                })
                            }));
                        } else {
                            video_task = Some(spawn_video_pump(
                                Arc::clone(&track),
                                tx,
                                state.keyframe_request.clone(),
                            ));
                        }
                    }
                    // Teşhis: istemcinin ICE adaylarını say. Chrome/Edge varsayılanda
                    // yerel IP'leri gizler ve "<uuid>.local" (mDNS) adayları yollar;
                    // bunları çözebilmek için UDP 5353'ün güvenlik duvarında AÇIK
                    // olması şart. Çözülemezse adaylar düşer, uzak aday sayısı sıfıra
                    // iner ve ICE "pingAllCandidates ... no candidate pairs" ile takılır
                    // (yaşandı: ufw açıkken Windows/Chrome'dan bağlanan izleyici).
                    let cands: Vec<&str> =
                        sdp.lines().filter(|l| l.starts_with("a=candidate:")).collect();
                    let mdns = cands.iter().filter(|c| c.contains(".local")).count();
                    info!("İstemci ICE adayları: {} (mDNS/.local: {mdns})", cands.len());
                    if mdns > 0 && mdns == cands.len() {
                        warn!(
                            "Adayların tamamı mDNS — çözülemezse bağlantı kurulamaz. \
                             Güvenlik duvarında UDP 5353 açık olmalı \
                             (ufw: sudo ufw allow from <ağ>/24 to any port 5353 proto udp)"
                        );
                    }

                    let offer = RTCSessionDescription::offer(sdp)?;
                    pc.set_remote_description(offer).await?;
                    let answer = pc.create_answer(None).await?;
                    let mut gathered = pc.gathering_complete_promise().await;
                    pc.set_local_description(answer).await?;
                    let _ = tokio::time::timeout(Duration::from_secs(3), gathered.recv()).await;
                    let local = pc.local_description().await.context("yerel SDP yok")?;
                    let reply = serde_json::to_string(&SignalMessage::Answer { sdp: local.sdp })?;
                    ws.send(Message::Text(reply.into())).await?;
                }
                Ok(_) => {}
                Err(e) => warn!("Anlaşılmayan sinyal mesajı: {e}"),
            }
        }
    }

    info!("İzleyici ayrıldı");
    if let Some(t) = video_task {
        t.abort();
    }
    pli_task.abort();
    for t in audio_tasks {
        t.abort();
    }
    let _ = pc.close().await;
    Ok(())
}

/// Seçilen yayın kanalını WebRTC track'ine pompalar. Yayına anahtar kareyle
/// başlanır; kanal taşarsa (Lagged) çözücü bozulmasın diye yine anahtar kare beklenir.
fn spawn_video_pump(
    track: Arc<TrackLocalStaticSample>,
    tx: broadcast::Sender<Arc<crate::engine::EncodedFrame>>,
    keyframe: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let mut rx = tx.subscribe();
    keyframe.store(true, Ordering::SeqCst);
    tokio::spawn(async move {
        let mut wait_key = true;
        let mut last_ts: Option<i64> = None;
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if wait_key {
                        if !frame.is_keyframe {
                            continue;
                        }
                        wait_key = false;
                    }
                    // RTP zamanı GERÇEK geçen süreyle ilerlemeli. Sabit 1/fps
                    // kullanılırsa (kare atlamalı akışta) alıcı saati geri kalır,
                    // jitter tamponu saniyelerce şişer (ölçülen ~1.5 sn bug'ı).
                    let duration = match last_ts {
                        Some(prev) if frame.ts_100ns > prev => Duration::from_nanos(
                            (frame.ts_100ns - prev).min(100_000_000) as u64 * 100,
                        ),
                        _ => frame.duration,
                    };
                    last_ts = Some(frame.ts_100ns);
                    let sample = Sample {
                        data: frame.data.clone(),
                        duration,
                        ..Default::default()
                    };
                    if track.write_sample(&sample).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("İzleyici {n} kare geride kaldı; anahtar kareyle toparlanıyor");
                    wait_key = true;
                    keyframe.store(true, Ordering::SeqCst);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}
