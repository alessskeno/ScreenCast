//! Host ile TV istemcisi arasındaki mesaj biçimleri (JSON).
//! `tv-app/js/app.js` bu biçimlerin aynısını kullanır.

use serde::{Deserialize, Serialize};

/// WebSocket üzerinden sinyalleşme. Trickle ICE kullanılmaz:
/// LAN'da adaylar anında toplandığı için SDP'ler adaylarla birlikte tam gönderilir.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    Offer {
        sdp: String,
        /// İstemci profili: "lite" = 1080p hafif akış (TV), yoksa tam akış.
        #[serde(default)]
        profile: Option<String>,
        /// Video yolu: "webcodecs" = ham AU'lar veri kanalından (tarayıcı jitter
        /// tamponu devre dışı, en düşük gecikme); yoksa standart RTP.
        #[serde(default)]
        video_path: Option<String>,
    },
    Answer { sdp: String },
}

/// İmleç durumu; koordinatlar yakalanan ekrana göre 0..1 aralığında normalize.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CursorState {
    pub x: f32,
    pub y: f32,
    pub visible: bool,
}
