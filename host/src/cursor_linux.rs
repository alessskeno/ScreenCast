//! İmleç takibi — Linux tarafı.
//!
//! Windows'ta imleç videoya GÖMÜLMEZ; konumu 125 Hz'de okunup ayrı bir veri
//! kanalından gider ve TV kendi imlecini çizer (video gecikmesinden bağımsız,
//! RDP tekniği). Linux'ta bu mümkün DEĞİL:
//!
//! Wayland'da bir uygulamanın küresel imleç konumunu okuması güvenlik gereği
//! engellidir (`GetCursorInfo` karşılığı yok; XWayland'ın `XQueryPointer`'ı
//! yalnız imleç X pencerelerinin üzerindeyken güncellenir, yani güvenilmez).
//! Bu yüzden Linux'ta imleç, yakalama katmanında videoya gömülür
//! (Mutter `cursor-mode=1`, ximagesrc `show-pointer=true`).
//!
//! Sonuç: kanal yine kurulur ama daima `visible=false` bildirir → istemci kendi
//! imlecini çizmez, çift imleç görünmez. Ödünç: imleç artık video gecikmesine
//! tabi (LAN'da ölçülen boru <10ms olduğu için pratikte fark edilmiyor).

use tokio::sync::watch;

use crate::engine::Rect;
use crate::protocol::CursorState;

/// İmleç videoya gömülü olduğu için hiç güncelleme yayınlanmaz.
///
/// Gönderici hemen düşürülür: oturumdaki imleç pompası `changed()`'den Err alıp
/// sessizce çıkar, istemcideki imleç elemanı da varsayılan `display:none`'da
/// kalır (bkz. tv-app/css/style.css). Veri kanalının kendisi açık kalır —
/// gecikme ölçümündeki ping/pong ayrı bir işleyicidir, etkilenmez.
pub fn spawn(_rect: Rect) -> watch::Receiver<CursorState> {
    let (_tx, rx) = watch::channel(CursorState { x: 0.5, y: 0.5, visible: false });
    rx
}
