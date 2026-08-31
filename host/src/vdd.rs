//! Faz 3: Parsec sanal monitör sürücüsü (parsec-vdd) denetimi.
//!
//! Sürücü kuruluyken bu modül monitörü PROGRAMATİK olarak takar:
//! host başlarken sanal ekran belirir, süreç ölünce (~1 sn içinde) kendiliğinden
//! sökülür — çünkü sürücü ~100 ms'de bir "yaşam sinyali" (UPDATE ioctl) ister.
//! Bu sayede çökme dahil her senaryoda hayalet monitör kalmaz.
//!
//! IOCTL kodları ve arayüz GUID'i nomi-san/parsec-vdd projesinden (MIT).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
};
use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, HANDLE};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, CDS_UPDATEREGISTRY, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};

use crate::engine::Rect;

// parsec-vdd cihaz arayüzü GUID'i: {00b41627-04c4-429e-a26e-0265cf50c8fa}
const VDD_INTERFACE_GUID: GUID = GUID::from_u128(0x00b41627_04c4_429e_a26e_0265cf50c8fa);

const IOCTL_ADD: u32 = 0x0022e004;
const IOCTL_REMOVE: u32 = 0x0022a008;
const IOCTL_UPDATE: u32 = 0x0022a00c;
const IOCTL_VERSION: u32 = 0x0022e010;

pub struct Vdd {
    handle: HANDLE,
}
unsafe impl Send for Vdd {}
unsafe impl Sync for Vdd {}

impl Vdd {
    /// Sürücünün cihaz arayüzünü bul ve aç.
    pub fn open() -> Result<Self> {
        unsafe {
            let mut len = 0u32;
            let cr = CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &VDD_INTERFACE_GUID,
                None,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            );
            if cr != CR_SUCCESS || len <= 1 {
                bail!("parsec-vdd cihazı bulunamadı (sürücü kurulu mu?)");
            }
            let mut buf = vec![0u16; len as usize];
            let cr = CM_Get_Device_Interface_ListW(
                &VDD_INTERFACE_GUID,
                None,
                &mut buf,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            );
            if cr != CR_SUCCESS {
                bail!("parsec-vdd cihaz listesi alınamadı");
            }
            // Liste null ile ayrılır; ilk yolu al.
            let first: Vec<u16> = buf.iter().copied().take_while(|&c| c != 0).collect();
            if first.is_empty() {
                bail!("parsec-vdd cihaz yolu boş");
            }
            let path: Vec<u16> = first.iter().copied().chain(std::iter::once(0)).collect();
            let handle = CreateFileW(
                PCWSTR(path.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_OVERLAPPED.0),
                None,
            )
            .context("parsec-vdd cihazı açılamadı")?;
            Ok(Self { handle })
        }
    }

    fn ioctl(&self, code: u32, input: [u8; 32]) -> Result<u32> {
        unsafe {
            let event = CreateEventW(None, true, false, None)?;
            let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
            let mut out: u32 = 0;
            let mut returned: u32 = 0;
            let result = DeviceIoControl(
                self.handle,
                code,
                Some(input.as_ptr() as _),
                input.len() as u32,
                Some(&mut out as *mut u32 as _),
                4,
                Some(&mut returned),
                Some(&mut ov),
            );
            if let Err(e) = result {
                if e.code() == ERROR_IO_PENDING.to_hresult() {
                    let _ = WaitForSingleObject(event, 5000);
                    let mut transferred = 0u32;
                    let _ = GetOverlappedResult(self.handle, &ov, &mut transferred, false);
                } else {
                    let _ = CloseHandle(event);
                    return Err(e).context("VDD ioctl");
                }
            }
            let _ = CloseHandle(event);
            Ok(out)
        }
    }

    pub fn version(&self) -> Result<u32> {
        self.ioctl(IOCTL_VERSION, [0; 32])
    }

    /// Sanal monitör tak; sürücünün verdiği dizini döndürür.
    pub fn add_display(&self) -> Result<u32> {
        self.ioctl(IOCTL_ADD, [0; 32])
    }

    #[allow(dead_code)]
    pub fn remove_display(&self, index: u32) {
        let mut input = [0u8; 32];
        input[0] = (index >> 8) as u8; // 16-bit big-endian
        input[1] = index as u8;
        let _ = self.ioctl(IOCTL_REMOVE, input);
    }

    fn update(&self) {
        let _ = self.ioctl(IOCTL_UPDATE, [0; 32]);
    }
}

impl Drop for Vdd {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Yaşam sinyali: 50 ms'de bir UPDATE. Süreç ölürse sinyal kesilir ve sürücü
/// ~1 sn içinde tüm sanal monitörleri kendisi söker (kasıtlı güvenlik ağı).
pub fn spawn_keepalive(vdd: Arc<Vdd>, stop: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("vdd-keepalive".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                vdd.update();
                std::thread::sleep(Duration::from_millis(50));
            }
            debug!("VDD yaşam sinyali durdu; monitör ~1 sn içinde sökülecek");
        })
        .ok();
}

/// Sistemdeki tüm görüntü çıkışları (tüm adaptörler).
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub adapter_index: u32,
    pub output_index: u32,
    pub device_name: String,
    /// Masaüstündeki dikdörtgen (platformdan bağımsız tip — Linux tarafı da aynısını kullanır).
    pub rect: Rect,
}

pub fn all_outputs() -> Vec<OutputInfo> {
    let mut found = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return found };
        for a in 0..8u32 {
            let Ok(adapter) = factory.EnumAdapters1(a) else { break };
            for o in 0..8u32 {
                let Ok(output) = adapter.EnumOutputs(o) else { break };
                if let Ok(desc) = output.GetDesc() {
                    let name = String::from_utf16_lossy(
                        &desc.DeviceName[..desc
                            .DeviceName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(desc.DeviceName.len())],
                    );
                    // AttachedToDesktop=false çıkışları atla (bağlı değil).
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    found.push(OutputInfo {
                        adapter_index: a,
                        output_index: o,
                        device_name: name,
                        rect: {
                            let d = desc.DesktopCoordinates;
                            Rect { left: d.left, top: d.top, right: d.right, bottom: d.bottom }
                        },
                    });
                }
            }
        }
    }
    found
}

/// Belirtilen ekranın çözünürlük/tazeleme modunu değiştirir (örn. 4K@60).
/// `device_name`: "\\.\DISPLAYx" biçiminde DXGI cihaz adı.
pub fn set_display_mode(device_name: &str, w: u32, h: u32, hz: u32) -> Result<()> {
    unsafe {
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        dm.dmPelsWidth = w;
        dm.dmPelsHeight = h;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        if hz > 0 {
            dm.dmDisplayFrequency = hz;
            dm.dmFields |= DM_DISPLAYFREQUENCY;
        }
        let name: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
        let r = ChangeDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            Some(&dm),
            None,
            CDS_UPDATEREGISTRY,
            None,
        );
        if r != DISP_CHANGE_SUCCESSFUL {
            bail!("{w}x{h}@{hz} modu kabul edilmedi (kod {r:?}) — sürücünün desteklediği bir mod deneyin");
        }
    }
    Ok(())
}

/// Cihaz adına göre çıkışı yeniden bul (mod değişince konum/boyut tazelenir).
pub fn find_output_by_name(name: &str) -> Option<OutputInfo> {
    all_outputs().into_iter().find(|o| o.device_name == name)
}

/// Sürücü kurulu mu? (Panelde "indir ve kur" uyarısını göstermek için.)
/// Cihaz arayüzünü açıp hemen kapatır; yan etkisi yoktur.
pub fn is_installed() -> bool {
    Vdd::open().is_ok()
}

/// Sanal monitörü takar ve yeni beliren çıkışı bulur.
/// Dönen bilgi hem yakalama hedefi hem imleç normalizasyonu için kullanılır.
pub fn attach_virtual_display(stop: Arc<AtomicBool>) -> Result<OutputInfo> {
    let before: Vec<String> = all_outputs().into_iter().map(|o| o.device_name).collect();

    let vdd = Arc::new(Vdd::open().context(
        "parsec-vdd sürücüsü açılamadı. Kurulum: https://builds.parsec.app/vdd/parsec-vdd-0.45.0.0.exe ( /S ile sessiz kurulur )",
    )?);
    if let Ok(v) = vdd.version() {
        info!("parsec-vdd sürüm: {v}");
    }
    vdd.add_display().context("sanal monitör takılamadı")?;
    spawn_keepalive(vdd, stop);

    // Windows'un monitörü tanıyıp masaüstüne eklemesini bekle.
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(new_out) = all_outputs()
            .into_iter()
            .find(|o| !before.contains(&o.device_name))
        {
            info!(
                "Sanal monitör hazır: {} (adaptör {}, çıkış {}, {}x{} @ ({},{}))",
                new_out.device_name,
                new_out.adapter_index,
                new_out.output_index,
                new_out.rect.right - new_out.rect.left,
                new_out.rect.bottom - new_out.rect.top,
                new_out.rect.left,
                new_out.rect.top,
            );
            return Ok(new_out);
        }
    }
    warn!("Yeni çıkış görünmedi; monitör listesi: {:?}", all_outputs());
    bail!("sanal monitör 10 sn içinde masaüstüne eklenmedi")
}
