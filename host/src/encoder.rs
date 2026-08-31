//! Media Foundation donanım H.264 kodlayıcısı (asenkron MFT).
//!
//! Performans için kritik seçimler:
//! - Donanım MFT (NVENC / QuickSync / AMD VCN): CPU neredeyse hiç çalışmaz.
//! - Girdi NV12 dokuları GPU'da kalır (IMFDXGIDeviceManager → sıfır kopya).
//! - Düşük gecikme kipi + CBR + B-frame'siz akış: kodlayıcı kare "bekletmez".
//! - Çıktı Annex-B bayt dizisidir; WebRTC H.264 paketleyicisinin beklediği biçim.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use tracing::{debug, info, warn};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main, ICodecAPI, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFTransform, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample, MFStartup, MFTEnumEx,
    MFMediaType_Video, MFSampleExtension_CleanPoint, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, CODECAPI_AVEncCommonLowLatency,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQualityVsSpeed,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MFT_REGISTER_TYPE_INFO, MF_API_VERSION,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_SDK_VERSION, MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::pipeline::FrameTex;

/// Kodlanmış tek bir H.264 erişim birimi (Annex-B).
/// Tanım platformdan bağımsız `engine` modülünde; buradan yeniden dışa verilir
/// ki Windows tarafındaki `crate::encoder::EncodedFrame` kullanımları değişmesin.
pub use crate::engine::EncodedFrame;

// mftransform.h'daki MediaEventType değerleri (ABI sabitleri):
const EV_NEED_INPUT: u32 = 601; // METransformNeedInput
const EV_HAVE_OUTPUT: u32 = 602; // METransformHaveOutput

pub struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    _device_manager: IMFDXGIDeviceManager,
    frame_dur_100ns: i64,
    frame_dur: Duration,
}

// COM işaretçileri iş parçacığına taşınır; MF nesneleri serbest iş parçacıklıdır
// ve D3D cihazı çok iş parçacıklı korumada kullanılır.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        gop_frames: u32,
    ) -> Result<Self> {
        unsafe {
            MFStartup((MF_SDK_VERSION << 16) | MF_API_VERSION, MFSTARTUP_FULL)?;

            // GPU dokularını doğrudan kodlayıcıya vermek için cihaz yöneticisi.
            let mut reset_token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
            let manager = manager.context("DXGI cihaz yöneticisi oluşturulamadı")?;
            manager.ResetDevice(device, reset_token)?;

            // Donanım H.264 kodlayıcısını bul (SORTANDFILTER en iyisini öne koyar).
            let out_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&out_info),
                &mut activates,
                &mut count,
            )?;
            if count == 0 || activates.is_null() {
                bail!("Donanım H.264 kodlayıcı bulunamadı (GPU sürücüsünü kontrol edin)");
            }
            let mut list: Vec<Option<IMFActivate>> = Vec::with_capacity(count as usize);
            for i in 0..count as usize {
                list.push(std::ptr::read(activates.add(i)));
            }
            CoTaskMemFree(Some(activates as _));
            let activate = list.remove(0).context("kodlayıcı etkinleştiricisi boş")?;
            // (list düşerken diğer etkinleştiriciler serbest kalır)

            let mut name = PWSTR::null();
            let mut name_len = 0u32;
            if activate
                .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut name_len)
                .is_ok()
            {
                info!("Donanım kodlayıcı: {}", name.to_string().unwrap_or_default());
                CoTaskMemFree(Some(name.as_ptr() as _));
            }

            let transform: IMFTransform = activate.ActivateObject()?;

            // Teşhis: MFT gerçekten D3D11 dokularını doğrudan alabiliyor mu?
            let attrs = transform.GetAttributes()?;
            let d3d_aware = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0);
            info!("MFT D3D11 farkındalığı: {d3d_aware}");

            // Donanım MFT'leri asenkrondur; olay tabanlı kullanım için kilidi aç.
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .context("ASYNC_UNLOCK")?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("SET_D3D_MANAGER")?;

            // Önce çıkış türü (H.264), sonra giriş türü (NV12) — MF'nin istediği sıra.
            let out_ty = MFCreateMediaType()?;
            out_ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_ty.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
            out_ty.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            out_ty.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
            out_ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            out_ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_ty.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            transform.SetOutputType(0, &out_ty, 0).context("SetOutputType(H264)")?;

            // Giriş türünü sıfırdan kurmayı bazı sürücüler (örn. NVENC) reddediyor.
            // Kodlayıcının kendi önerdiği türlerden NV12'yi seçip boyut/hızı üzerine yaz.
            let mut in_ty = None;
            for i in 0u32..64 {
                match transform.GetInputAvailableType(0, i) {
                    Ok(t) => {
                        let sub = t.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
                        info!("MFT giriş türü #{i}: {sub:?}");
                        if sub == MFVideoFormat_NV12 && in_ty.is_none() {
                            in_ty = Some(t);
                        }
                    }
                    Err(_) => break,
                }
            }
            let in_ty = in_ty.context("kodlayıcı NV12 girişini önermiyor")?;
            in_ty.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            in_ty.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
            transform.SetInputType(0, &in_ty, 0).context("SetInputType(NV12)")?;

            // Düşük gecikme ayarları — desteklemeyen sürücüde uyarı verip devam et.
            let codec_api: ICodecAPI = transform.cast()?;
            // Ölçüldü (2026-08-29): AVLowLatencyMode'un çıkış hızına etkisi yok
            // (darboğaz ProcessInput'un kendisi); iç tamponu azalttığı için açık kalır.
            set_opt(&codec_api, &CODECAPI_AVLowLatencyMode, variant_bool(true), "AVLowLatencyMode");
            set_opt(&codec_api, &CODECAPI_AVEncCommonLowLatency, variant_bool(true), "CommonLowLatency");
            set_opt(
                &codec_api,
                &CODECAPI_AVEncCommonRateControlMode,
                variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
                "CBR",
            );
            set_opt(&codec_api, &CODECAPI_AVEncCommonMeanBitRate, variant_u32(bitrate_bps), "bit hızı");
            set_opt(&codec_api, &CODECAPI_AVEncMPVGOPSize, variant_u32(gop_frames), "GOP");
            set_opt(&codec_api, &CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0), "B-frame=0");
            set_opt(&codec_api, &CODECAPI_AVEncCommonQualityVsSpeed, variant_u32(33), "hız önceliği");

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let events: IMFMediaEventGenerator = transform.cast()?;
            let frame_dur_100ns = 10_000_000i64 / fps.max(1) as i64;

            Ok(Self {
                transform,
                events,
                codec_api,
                _device_manager: manager,
                frame_dur_100ns,
                frame_dur: Duration::from_nanos(frame_dur_100ns as u64 * 100),
            })
        }
    }

    /// Kodlayıcı olay döngüsü. `next_frame` None dönene ya da `stop` işaretlenene
    /// kadar çalışır; kodlanan her kareyi `on_encoded`e verir.
    pub fn drive(
        mut self,
        mut next_frame: impl FnMut() -> Option<FrameTex>,
        mut on_encoded: impl FnMut(EncodedFrame),
        keyframe_request: &AtomicBool,
        stop: &AtomicBool,
    ) -> Result<()> {
        let mut first_logged = false;
        // Aşama kronometreleri (teşhis): 3 sn'de bir dökülür.
        let mut acc_evwait = Duration::ZERO; // olay bekleme (NVENC'in temposu)
        let mut acc_frame = Duration::ZERO; // yakalamadan kare bekleme
        let mut acc_submit = Duration::ZERO; // ProcessInput tarafı
        let mut acc_drain = Duration::ZERO; // ProcessOutput + kopya
        let mut n_need = 0u32;
        let mut n_have = 0u32;
        let mut last_log = std::time::Instant::now();

        while !stop.load(Ordering::Relaxed) {
            // Bloklu GetEvent: olay gelince anında uyanır. Uyuyarak yoklama (sleep 1ms)
            // KULLANMA — Windows'ta sleep(1ms) gerçekte ~15.6ms'dir. Not: durdurma,
            // süreç çıkışıyla olur; bloklanan iş parçacığını ayrıca uyandırmayız.
            let t = std::time::Instant::now();
            let event = match unsafe {
                self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
            } {
                Ok(e) => e,
                Err(e) => return Err(e).context("MFT olay döngüsü"),
            };
            acc_evwait += t.elapsed();

            match unsafe { event.GetType()? } {
                EV_NEED_INPUT => {
                    n_need += 1;
                    let t = std::time::Instant::now();
                    let Some(frame) = next_frame() else { break };
                    acc_frame += t.elapsed();
                    let t = std::time::Instant::now();
                    self.submit(frame, keyframe_request)?;
                    acc_submit += t.elapsed();
                }
                EV_HAVE_OUTPUT => {
                    n_have += 1;
                    let t = std::time::Instant::now();
                    let out = self.drain_output()?;
                    acc_drain += t.elapsed();
                    if let Some(f) = out {
                        if !first_logged {
                            info!(
                                "İlk kare kodlandı ({} bayt, anahtar kare: {})",
                                f.data.len(),
                                f.is_keyframe
                            );
                            first_logged = true;
                        }
                        on_encoded(f);
                    }
                }
                other => debug!("MFT olayı: {other}"),
            }

            if last_log.elapsed() >= Duration::from_secs(3) {
                info!(
                    "MFT 3sn: need={n_need} have={n_have} | olay bekleme {:?} | kare bekleme {:?} | submit {:?} | drain {:?}",
                    acc_evwait, acc_frame, acc_submit, acc_drain
                );
                acc_evwait = Duration::ZERO;
                acc_frame = Duration::ZERO;
                acc_submit = Duration::ZERO;
                acc_drain = Duration::ZERO;
                n_need = 0;
                n_have = 0;
                last_log = std::time::Instant::now();
            }
        }
        Ok(())
    }

    fn submit(&mut self, frame: FrameTex, keyframe_request: &AtomicBool) -> Result<()> {
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &frame.tex, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            // Gerçek yakalama zamanı kullanılır: değişmeyen kareler atlandığı için
            // kareler düzensiz gelir; kodlayıcının hız denetimi gerçek zamanı bilmeli.
            sample.SetSampleTime(frame.ts_100ns)?;
            sample.SetSampleDuration(self.frame_dur_100ns)?;

            if keyframe_request.swap(false, Ordering::SeqCst) {
                let _ = self
                    .codec_api
                    .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
            }
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        Ok(())
    }

    fn drain_output(&mut self) -> Result<Option<EncodedFrame>> {
        unsafe {
            loop {
                let mut out = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(None),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let result = self.transform.ProcessOutput(0, &mut out, &mut status);
                let sample = ManuallyDrop::take(&mut out[0].pSample);
                drop(ManuallyDrop::take(&mut out[0].pEvents));

                match result {
                    Ok(()) => {
                        let Some(sample) = sample else { return Ok(None) };
                        let buf = sample.ConvertToContiguousBuffer()?;
                        let mut ptr = std::ptr::null_mut();
                        let mut len = 0u32;
                        buf.Lock(&mut ptr, None, Some(&mut len))?;
                        let data =
                            Bytes::copy_from_slice(std::slice::from_raw_parts(ptr, len as usize));
                        let _ = buf.Unlock();
                        let is_keyframe =
                            sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
                        // MF, girişte verdiğimiz yakalama zamanını çıkışa taşır.
                        let ts_100ns = sample.GetSampleTime().unwrap_or(0);
                        return Ok(Some(EncodedFrame {
                            data,
                            is_keyframe,
                            duration: self.frame_dur,
                            ts_100ns,
                        }));
                    }
                    // Kodlayıcı çıkış biçimini tazeledi (ilk kare öncesi olağan).
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        let new_ty = self.transform.GetOutputAvailableType(0, 0)?;
                        self.transform.SetOutputType(0, &new_ty, 0)?;
                        debug!("Kodlayıcı çıkış türü yenilendi");
                        continue;
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                    Err(e) => return Err(e).context("ProcessOutput"),
                }
            }
        }
    }
}

fn set_opt(api: &ICodecAPI, guid: &GUID, value: VARIANT, label: &str) {
    if let Err(e) = unsafe { api.SetValue(guid, &value) } {
        warn!("Kodlayıcı ayarı desteklenmedi: {label} ({e})");
    }
}

// windows 0.61'de VARIANT ham C birleşimi; sayı/bool için elle kurulur.
// (VT_UI4/VT_BOOL bellekte temizlik gerektirmez, Drop derdi yoktur.)

fn variant_u32(value: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

fn variant_bool(value: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: if value { VARIANT_TRUE } else { VARIANT_FALSE },
                },
            }),
        },
    }
}
