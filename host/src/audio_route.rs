//! "TV'ye özel ses": sanal ses aygıtını varsayılan çıkış yapar.
//!
//! Sorun: loopback yakalama sesin KOPYASINI alır; ses PC hoparlöründe de çalar.
//! Çözüm (gerçek HDMI TV davranışı): yayın sırasında varsayılan çıkışı sessiz bir
//! sanal aygıta (VB-CABLE "CABLE Input" ya da "Steam Streaming Speakers") çevir.
//! Uygulamalar oraya çalar → loopback oradan yakalar → TV'den duyulur, PC susar.
//! Çıkışta eski varsayılan geri getirilir.
//!
//! Varsayılanı değiştirmek için Windows'un belgelenmemiş ama SoundSwitch /
//! EarTrumpet gibi araçların yıllardır kullandığı IPolicyConfig COM arayüzü
//! kullanılır (vtable sırası ABI'dir, değiştirme!).

#![allow(non_snake_case)] // COM vtable metod adları Windows imzalarıyla birebir

use anyhow::{bail, Context, Result};
use tracing::{info, warn};
use windows::core::{interface, IUnknown, IUnknown_Vtbl, GUID, HRESULT, PCWSTR};
use windows::Win32::Media::Audio::{
    eConsole, eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};

// {870af99c-171d-4f9e-af0d-e63df40c2bc9} — CPolicyConfigClient
const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

// PKEY_Device_FriendlyName: {a45c254e-df1c-4efd-8020-67d146a850e0}, pid 14
const PKEY_FRIENDLY_NAME: windows::Win32::Foundation::PROPERTYKEY =
    windows::Win32::Foundation::PROPERTYKEY {
        fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 14,
    };

/// IPolicyConfig — yalnız SetDefaultEndpoint kullanılır; öncekiler vtable
/// hizasını korumak için ham imzalarla yer tutar.
#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    unsafe fn GetMixFormat(&self, dev: PCWSTR, fmt: *mut *mut core::ffi::c_void) -> HRESULT;
    unsafe fn GetDeviceFormat(&self, dev: PCWSTR, default: i32, fmt: *mut *mut core::ffi::c_void) -> HRESULT;
    unsafe fn ResetDeviceFormat(&self, dev: PCWSTR) -> HRESULT;
    unsafe fn SetDeviceFormat(&self, dev: PCWSTR, a: *mut core::ffi::c_void, b: *mut core::ffi::c_void) -> HRESULT;
    unsafe fn GetProcessingPeriod(&self, dev: PCWSTR, default: i32, a: *mut i64, b: *mut i64) -> HRESULT;
    unsafe fn SetProcessingPeriod(&self, dev: PCWSTR, a: *mut i64) -> HRESULT;
    unsafe fn GetShareMode(&self, dev: PCWSTR, mode: *mut core::ffi::c_void) -> HRESULT;
    unsafe fn SetShareMode(&self, dev: PCWSTR, mode: *mut core::ffi::c_void) -> HRESULT;
    unsafe fn GetPropertyValue(&self, dev: PCWSTR, store: i32, key: *const core::ffi::c_void, val: *mut core::ffi::c_void) -> HRESULT;
    unsafe fn SetPropertyValue(&self, dev: PCWSTR, store: i32, key: *const core::ffi::c_void, val: *mut core::ffi::c_void) -> HRESULT;
    unsafe fn SetDefaultEndpoint(&self, dev: PCWSTR, role: u32) -> HRESULT;
    unsafe fn SetEndpointVisibility(&self, dev: PCWSTR, visible: i32) -> HRESULT;
}

pub struct AudioRoute {
    policy: IPolicyConfig,
    previous_id: Vec<u16>, // null sonlu
    pub target_name: String,
}

/// Etkin ses çıkışlarını (id, ad) listeler.
fn render_endpoints() -> Result<Vec<(Vec<u16>, String)>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let list = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..list.GetCount()? {
            let dev = list.Item(i)?;
            let id_pw = dev.GetId()?;
            let mut id: Vec<u16> = Vec::new();
            let mut p = id_pw.0;
            while *p != 0 {
                id.push(*p);
                p = p.add(1);
            }
            id.push(0);
            CoTaskMemFree(Some(id_pw.0 as _));

            let store = dev.OpenPropertyStore(STGM_READ)?;
            let value = store.GetValue(&PKEY_FRIENDLY_NAME)?;
            let name = value
                .Anonymous
                .Anonymous
                .Anonymous
                .pwszVal
                .to_string()
                .unwrap_or_default();
            out.push((id, name));
        }
        Ok(out)
    }
}

fn default_endpoint_id() -> Result<Vec<u16>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
        let id_pw = dev.GetId()?;
        let mut id: Vec<u16> = Vec::new();
        let mut p = id_pw.0;
        while *p != 0 {
            id.push(*p);
            p = p.add(1);
        }
        id.push(0);
        CoTaskMemFree(Some(id_pw.0 as _));
        Ok(id)
    }
}

fn is_virtual_name(name: &str) -> bool {
    name.contains("CABLE Input") || name.contains("Steam Streaming")
}

/// Çökme durumunda geri dönebilmek için önceki varsayılanın kimliği diske yazılır.
fn restore_file() -> Option<std::path::PathBuf> {
    Some(
        std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("mirror-host")
            .join("audio-restore.id"),
    )
}

/// GUI için: güvenilir bir sanal ses aygıtı (VB-CABLE) kurulu mu?
pub fn virtual_sink_available() -> bool {
    render_endpoints()
        .map(|list| list.iter().any(|(_, n)| n.contains("CABLE Input")))
        .unwrap_or(false)
}

/// Sanal aygıtı bul, mevcut varsayılanı kaydet, sanalı varsayılan yap.
/// ÖNEMLİ: ses yakalama (audio::start) bundan SONRA başlamalı ki loopback
/// yeni varsayılanı (sanal aygıtı) dinlesin.
pub fn engage() -> Result<AudioRoute> {
    let endpoints = render_endpoints().context("ses çıkışları listelenemedi")?;
    let names: Vec<&str> = endpoints.iter().map(|(_, n)| n.as_str()).collect();
    info!("Etkin ses çıkışları: {names:?}");

    // Tercih sırası: VB-CABLE → Steam Streaming Speakers.
    let target = endpoints
        .iter()
        .find(|(_, n)| n.contains("CABLE Input"))
        .or_else(|| endpoints.iter().find(|(_, n)| n.contains("Steam Streaming")));
    let Some((target_id, target_name)) = target else {
        bail!(
            "Sanal ses aygıtı yok. VB-CABLE kurun (vb-audio.com/Cable) ya da --tv-audio kullanmayın. Mevcutlar: {names:?}"
        );
    };
    if target_name.contains("Steam Streaming") {
        // Ölçüldü: Steam aygıtı, Steam yayını aktif değilken ses motorunu
        // pompalamıyor → loopback'e neredeyse hiç veri gelmiyor (4 paket/5sn).
        warn!("Yedek aygıt Steam Streaming Speakers seçildi — güvenilir ses için VB-CABLE kurun (vb-audio.com/Cable)");
    }

    // Önceki varsayılan: mevcut varsayılan SANAL aygıtsa (önceki çalıştırma
    // çökmüş demektir) diskteki kayda ya da ilk gerçek aygıta düş.
    let mut previous_id = default_endpoint_id().context("mevcut varsayılan alınamadı")?;
    let current_name = endpoints
        .iter()
        .find(|(id, _)| *id == previous_id)
        .map(|(_, n)| n.clone())
        .unwrap_or_default();
    if is_virtual_name(&current_name) {
        let from_file = restore_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| {
                let mut v: Vec<u16> = s.trim().encode_utf16().collect();
                v.push(0);
                v
            });
        let fallback = endpoints
            .iter()
            .find(|(_, n)| !is_virtual_name(n))
            .map(|(id, _)| id.clone());
        if let Some(id) = from_file.or(fallback) {
            warn!("Varsayılan zaten sanal aygıttaydı (önceki çalıştırma düzgün kapanmamış); gerçek aygıt kayıttan alındı");
            previous_id = id;
        }
    }
    if let Some(p) = restore_file() {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(&p, String::from_utf16_lossy(&previous_id[..previous_id.len() - 1]));
    }

    let policy: IPolicyConfig = unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)? };
    unsafe {
        // eConsole(0) + eMultimedia(1); eCommunications(2) PC'de kalır (aramalar).
        policy
            .SetDefaultEndpoint(PCWSTR(target_id.as_ptr()), eConsole.0 as u32)
            .ok()
            .context("varsayılan ses aygıtı değiştirilemedi")?;
        let _ = policy.SetDefaultEndpoint(PCWSTR(target_id.as_ptr()), eMultimedia.0 as u32);

        // KRİTİK: loopback, aygıtın kendi ses seviyesini de kopyalar. Sanal aygıt
        // kısık/sessizse yayın da sessiz olur — sesi %100'e çek, sessizi kaldır.
        if let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        {
            if let Ok(dev) = enumerator.GetDevice(PCWSTR(target_id.as_ptr())) {
                if let Ok(vol) = dev.Activate::<windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume>(CLSCTX_ALL, None)
                {
                    let _ = vol.SetMute(false, std::ptr::null());
                    let _ = vol.SetMasterVolumeLevelScalar(1.0, std::ptr::null());
                    info!("Sanal aygıt sesi %100 + sessiz kapalı yapıldı");
                }
            }
        }
    }
    info!("Ses artık TV'ye yönlendi (PC çıkışı: {target_name} → sessiz sanal aygıt)");
    Ok(AudioRoute {
        policy,
        previous_id,
        target_name: target_name.clone(),
    })
}

impl AudioRoute {
    /// Eski varsayılan ses aygıtını geri getir.
    pub fn restore(&self) {
        unsafe {
            let r1 = self
                .policy
                .SetDefaultEndpoint(PCWSTR(self.previous_id.as_ptr()), eConsole.0 as u32);
            let _ = self
                .policy
                .SetDefaultEndpoint(PCWSTR(self.previous_id.as_ptr()), eMultimedia.0 as u32);
            if r1.is_ok() {
                info!("PC ses aygıtı eski haline döndü ({} bırakıldı)", self.target_name);
                if let Some(p) = restore_file() {
                    let _ = std::fs::remove_file(p);
                }
            } else {
                warn!("Ses aygıtı geri alınamadı; Ayarlar → Ses'ten elle seçin");
            }
        }
    }
}

/// `--restore-audio`: takılı kalmış ses yönlendirmesini düzelt ve çık.
/// Kayıt dosyası varsa oradaki aygıta, yoksa ilk gerçek (sanal olmayan) aygıta döner.
pub fn restore_cli() -> Result<()> {
    let endpoints = render_endpoints()?;
    let id = restore_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| {
            let mut v: Vec<u16> = s.trim().encode_utf16().collect();
            v.push(0);
            v
        })
        .or_else(|| {
            endpoints
                .iter()
                .find(|(_, n)| !is_virtual_name(n))
                .map(|(id, _)| id.clone())
        })
        .context("dönülecek gerçek ses aygıtı bulunamadı")?;
    let policy: IPolicyConfig = unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)? };
    unsafe {
        policy.SetDefaultEndpoint(PCWSTR(id.as_ptr()), eConsole.0 as u32).ok()?;
        let _ = policy.SetDefaultEndpoint(PCWSTR(id.as_ptr()), eMultimedia.0 as u32);
    }
    if let Some(p) = restore_file() {
        let _ = std::fs::remove_file(p);
    }
    info!("Ses aygıtı geri yüklendi");
    Ok(())
}
