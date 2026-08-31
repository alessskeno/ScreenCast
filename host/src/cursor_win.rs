//! İmleç konumu takibi.
//!
//! İmleç videoya gömülmez: konumu 125 Hz'de okunur ve WebRTC veri kanalından
//! minik JSON mesajları olarak gider. TV kendi tarafında çizer — böylece imleç
//! video kodlama/çözme gecikmesinden bağımsız, anında hareket eder (RDP tekniği).

use std::time::Duration;

use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};

use crate::engine::Rect;
use crate::protocol::CursorState;

/// Yakalanan monitörün sanal masaüstündeki dikdörtgenini bulur.
/// (Çoklu monitörde imleç koordinatları bu dikdörtgene göre normalize edilmeli.)
pub fn output_rect(output_index: u32) -> Option<Rect> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let output = adapter.EnumOutputs(output_index).ok()?;
        let d = output.GetDesc().ok()?.DesktopCoordinates;
        Some(Rect { left: d.left, top: d.top, right: d.right, bottom: d.bottom })
    }
}

/// İmleci 125 Hz'de izler; koordinatları `rect` içine normalize eder.
/// İmleç başka monitördeyse `visible=false` gönderilir (TV'de gizlenir).
pub fn spawn(rect: Rect) -> watch::Receiver<CursorState> {
    let (tx, rx) = watch::channel(CursorState { x: 0.5, y: 0.5, visible: false });
    let w = rect.width() as f32;
    let h = rect.height() as f32;

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(8));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tick.tick().await;

            let mut info = CURSORINFO {
                cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            if unsafe { GetCursorInfo(&mut info) }.is_err() {
                continue;
            }
            let x = (info.ptScreenPos.x - rect.left) as f32 / w;
            let y = (info.ptScreenPos.y - rect.top) as f32 / h;
            let inside = (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y);
            let state = CursorState {
                x: x.clamp(0.0, 1.0),
                y: y.clamp(0.0, 1.0),
                visible: inside && (info.flags.0 & CURSOR_SHOWING.0) != 0,
            };
            // Yalnızca değişince yayınla — kanal boşuna uyandırılmasın.
            tx.send_if_modified(|old| {
                if *old != state {
                    *old = state;
                    true
                } else {
                    false
                }
            });
        }
    });

    rx
}
