//! KDE/GNOME tarzı "imleç odağı izler": Alt+Tab ile BAŞKA monitördeki bir
//! pencereye geçilince imleç o pencerenin ortasına ışınlanır.
//! Windows'ta yerleşik karşılığı yoktur; EVENT_SYSTEM_FOREGROUND kancasıyla yapılır.
//!
//! Filtre: yalnız başlık çubuklu (WS_CAPTION) gerçek uygulama pencereleri —
//! Alt+Tab anahtarlayıcısının kendisi ve araç pencereleri imleci zıplatmasın.
//! (Yan etki: kenarlıksız tam ekran oyunlar da filtrelenir; kabul edilebilir.)

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongW, GetWindowRect, IsWindowVisible,
    SetCursorPos, TranslateMessage, EVENT_SYSTEM_FOREGROUND, GWL_STYLE, MSG,
    WINEVENT_OUTOFCONTEXT, WS_CAPTION,
};

pub fn spawn() {
    std::thread::Builder::new()
        .name("focus-follow".into())
        .spawn(|| unsafe {
            let hook = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(on_foreground),
                0, // tüm süreçler
                0, // tüm iş parçacıkları
                WINEVENT_OUTOFCONTEXT,
            );
            if hook.is_invalid() {
                tracing::warn!("Odak kancası kurulamadı; --cursor-follow devre dışı");
                return;
            }
            tracing::info!("İmleç odak takibi açık: Alt+Tab imleci pencereye ışınlar");
            // Kanca geri çağrıları bu iş parçacığının mesaj döngüsünden gelir.
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .ok();
}

unsafe extern "system" fn on_foreground(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    idobject: i32,
    _idchild: i32,
    _thread: u32,
    _time: u32,
) {
    const OBJID_WINDOW: i32 = 0;
    if idobject != OBJID_WINDOW || hwnd.is_invalid() || !IsWindowVisible(hwnd).as_bool() {
        return;
    }
    let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
    if style & WS_CAPTION.0 != WS_CAPTION.0 {
        return;
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return;
    }
    let mut cursor = POINT::default();
    if GetCursorPos(&mut cursor).is_err() {
        return;
    }
    // İmleç zaten pencerenin monitöründeyse dokunma.
    let window_monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let cursor_monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
    if window_monitor == cursor_monitor {
        return;
    }
    let _ = SetCursorPos((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
}
