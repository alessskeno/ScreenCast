//! DXGI Desktop Duplication ile ekran yakalama.
//!
//! Kareler GPU'da BGRA doku olarak alınır ve GPU'da kalır — CPU'ya hiç
//! indirilmez (sıfır kopya yolu). İmleç bu karelere dahil DEĞİLDİR;
//! ayrı kanaldan gönderilir (bkz. cursor.rs).

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
};

pub struct Capturer {
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    pub width: u32,
    pub height: u32,
}

pub enum Grab {
    /// Yeni görüntü var, hedef dokuya kopyalandı.
    New,
    /// Ekranda değişiklik yok (bit hızından tasarruf: kodlamayı atla).
    NoChange,
    /// Süre doldu, yeni kare yok.
    Timeout,
    /// Erişim koptu (çözünürlük/mod değişimi, güvenli masaüstü vb.) — yeniden kur.
    Lost,
}

impl Capturer {
    pub fn new(device: &ID3D11Device, output_index: u32) -> Result<Self> {
        unsafe {
            let dxgi_device: IDXGIDevice = device.cast()?;
            let adapter = dxgi_device.GetAdapter()?;
            let output = adapter
                .EnumOutputs(output_index)
                .with_context(|| format!("monitör {output_index} bulunamadı"))?;
            let output1: IDXGIOutput1 = output.cast()?;
            let duplication = output1
                .DuplicateOutput(device)
                .context("DuplicateOutput başarısız (not: RDP oturumunda çalışmaz)")?;

            let desc: DXGI_OUTDUPL_DESC = duplication.GetDesc();

            Ok(Self {
                context: device.GetImmediateContext()?,
                duplication,
                width: desc.ModeDesc.Width,
                height: desc.ModeDesc.Height,
            })
        }
    }

    /// Yeni kare varsa ve `copy` isteniyorsa `dst`e (aynı boyutta BGRA doku) kopyalar.
    /// `copy=false` ile sadece kuyruk boşaltılır (165Hz panelde hedef fps'in üzerindeki
    /// kareler GPU'ya İŞ YAPTIRMADAN atlanır — yoksa kodlayıcı GPU kilidi kıtlığından boğulur).
    pub fn grab_into(&mut self, dst: &ID3D11Texture2D, timeout_ms: u32, copy: bool) -> Result<Grab> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match self.duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {
                    // LastPresentTime == 0 → sadece imleç kıpırdadı, görüntü aynı.
                    let has_new_image = info.LastPresentTime != 0 && copy;
                    if has_new_image {
                        let tex: ID3D11Texture2D =
                            resource.as_ref().context("kare kaynağı boş")?.cast()?;
                        self.context.CopyResource(dst, &tex);
                    }
                    let _ = self.duplication.ReleaseFrame();
                    Ok(if has_new_image { Grab::New } else { Grab::NoChange })
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(Grab::Timeout),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => Ok(Grab::Lost),
                Err(e) => Err(e).context("AcquireNextFrame"),
            }
        }
    }
}
