//! GPU'da BGRA → NV12 renk dönüşümü (D3D11 Video Processor).
//!
//! Donanım H.264 kodlayıcıları girdi olarak NV12 ister. Dönüşümü GPU'nun
//! sabit-işlev video biriminde yapmak hem CPU'yu sıfır kullanır hem de
//! kareyi hiç sistem belleğine indirmez.

use std::mem::ManuallyDrop;

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VIDEO_ENCODER, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

/// Havuz derinliği: kodlayıcı bir NV12 dokusunu okurken biz sıradakine yazarız.
/// Geniş tutulur ki kodlayıcının hâlâ okuduğu dokunun üstüne yazıp GPU'ya
/// senkron beklemesi ekletmeyelim.
const POOL: usize = 8;

pub fn create_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.context("doku oluşturulamadı")
}

pub struct Converter {
    ctx: ID3D11VideoContext,
    base_ctx: ID3D11DeviceContext,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    pool: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    /// Kodlayıcıya verilen "temiz" kopyalar: VP'nin hiç bağlanmadığı, yalnızca
    /// VIDEO_ENCODER bind'lı dokular — NVENC'in senkron yavaş yolunu tetiklememesi için.
    submit_pool: Vec<ID3D11Texture2D>,
    next: usize,
}

impl Converter {
    /// `src_bgra`: yakalama dokusu. `src`/`dst`: kaynak ve hedef boyutlar
    /// (NV12 çift sayı boyut ister; gerekiyorsa işlemci ölçekler).
    pub fn new(
        device: &ID3D11Device,
        src_bgra: &ID3D11Texture2D,
        src: (u32, u32),
        dst: (u32, u32),
        fps: u32,
    ) -> Result<Self> {
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast()?;
            let ctx: ID3D11VideoContext = device.GetImmediateContext()?.cast()?;

            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                InputWidth: src.0,
                InputHeight: src.1,
                OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                OutputWidth: dst.0,
                OutputHeight: dst.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0)?;

            // Renk uzayı: giriş tam-aralık RGB (masaüstü), çıkış BT.709 stüdyo-aralık YUV.
            // _bitfield düzeni: Usage:1 | RGB_Range:1 | YCbCr_Matrix:1 | YCbCr_xvYCC:1 | Nominal_Range:2
            let cs_in = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let cs_out = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (1 << 4), // 709 matrisi + 16-235 aralık
            };
            ctx.VideoProcessorSetStreamColorSpace(&processor, 0, &cs_in);
            ctx.VideoProcessorSetOutputColorSpace(&processor, &cs_out);

            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut iv = None;
            vdev.CreateVideoProcessorInputView(src_bgra, &enumerator, &in_desc, Some(&mut iv))?;
            let input_view = iv.context("giriş görünümü oluşturulamadı")?;

            let mut pool = Vec::with_capacity(POOL);
            let mut submit_pool = Vec::with_capacity(POOL);
            for _ in 0..POOL {
                let sdesc = D3D11_TEXTURE2D_DESC {
                    Width: dst.0,
                    Height: dst.1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_VIDEO_ENCODER.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut st = None;
                device.CreateTexture2D(&sdesc, None, Some(&mut st))?;
                submit_pool.push(st.context("teslim dokusu oluşturulamadı")?);
                // VIDEO_ENCODER bayrağı kritik: onsuz NVENC MFT ProcessInput'ta
                // ~20ms'lik senkron kopya yapıyor (ölçüldü) ve kodlama ~28fps'te
                // kalıyor. Bu bayrakla sürücü dokuyu doğrudan (sıfır kopya) alır.
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: dst.0,
                    Height: dst.1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                device.CreateTexture2D(&desc, None, Some(&mut t))?;
                let tex = t.context("NV12 doku oluşturulamadı")?;
                let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut ov = None;
                vdev.CreateVideoProcessorOutputView(&tex, &enumerator, &ov_desc, Some(&mut ov))?;
                pool.push((tex, ov.context("çıkış görünümü oluşturulamadı")?));
            }

            let base_ctx: ID3D11DeviceContext = device.GetImmediateContext()?;
            Ok(Self { ctx, base_ctx, processor, input_view, pool, submit_pool, next: 0 })
        }
    }

    /// BGRA kaynağı havuzdaki sıradaki NV12 dokusuna dönüştürür ve o dokuyu döndürür.
    pub fn convert(&mut self) -> Result<ID3D11Texture2D> {
        let (tex, view) = &self.pool[self.next];
        self.next = (self.next + 1) % self.pool.len();

        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        }];
        let result = unsafe { self.ctx.VideoProcessorBlt(&self.processor, view, 0, &streams) };
        // ManuallyDrop içindeki klonu elle bırak — yoksa her karede referans sızar.
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result?;
        // Kodlayıcıya VP'ye hiç bağlanmamış temiz kopya ver (GPU'da ~0.2ms).
        let submit = &self.submit_pool[(self.next + self.pool.len() - 1) % self.pool.len()];
        unsafe { self.base_ctx.CopyResource(submit, tex) };
        Ok(submit.clone())
    }
}
