//! D3D11 scene compositor: draws each visible source as a textured quad onto
//! the canvas. Double-buffered so the preview never shows a half-drawn frame.

use std::sync::atomic::{AtomicUsize, Ordering};

use ferrocast_core::Rect;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

use crate::d3d::{compile_shader, Device};
use crate::Result;

const SHADER: &str = r#"
cbuffer Params : register(b0) {
    float4 rect;   // top-left (x, y) and size (w, h) in clip space
    float4 color;  // premultiplied color for solid fills
    float4 uv;     // u0, v0, u1, v1
    uint mode;     // 0 = texture, 1 = solid, 2 = opaque texture
    uint3 pad;
};
Texture2D tex : register(t0);
SamplerState samp : register(s0);

struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VSOut vs_main(uint id : SV_VertexID) {
    float2 c = float2(id & 1, id >> 1);
    VSOut o;
    o.pos = float4(rect.xy + c * rect.zw, 0, 1);
    o.uv = lerp(uv.xy, uv.zw, c);
    return o;
}

float4 ps_main(VSOut i) : SV_Target {
    if (mode == 1) return color;
    float4 c = tex.Sample(samp, i.uv);
    if (mode == 2) c.a = 1;
    return c;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    rect: [f32; 4],
    color: [f32; 4],
    uv: [f32; 4],
    mode: u32,
    pad: [u32; 3],
}

/// What to draw for one scene item.
pub enum Fill {
    /// A texture with premultiplied alpha (images).
    Texture(ID3D11ShaderResourceView),
    /// A texture whose alpha channel is meaningless (screens, windows, webcams).
    Opaque(ID3D11ShaderResourceView),
    /// Straight-alpha RGBA, 0..=1.
    Solid([f32; 4]),
}

pub struct DrawItem {
    pub fill: Fill,
    pub rect: Rect,
}

#[derive(Clone)]
pub struct Canvas {
    pub texture: ID3D11Texture2D,
    pub rtv: ID3D11RenderTargetView,
    pub srv: ID3D11ShaderResourceView,
}

unsafe impl Send for Canvas {}
unsafe impl Sync for Canvas {}

/// Shared quad-drawing pipeline (also used by the preview).
pub struct QuadPipeline {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
}

unsafe impl Send for QuadPipeline {}
unsafe impl Sync for QuadPipeline {}

impl QuadPipeline {
    pub fn new(device: &Device) -> Result<Self> {
        let d = &device.device;
        let vs_code = compile_shader(SHADER, "vs_main", "vs_5_0")?;
        let ps_code = compile_shader(SHADER, "ps_main", "ps_5_0")?;
        unsafe {
            let mut vs = None;
            d.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
            let mut ps = None;
            d.CreatePixelShader(&ps_code, None, Some(&mut ps))?;
            let mut cb = None;
            d.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: std::mem::size_of::<Params>() as u32,
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut cb),
            )?;
            let mut sampler = None;
            d.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            d.CreateBlendState(&blend_desc, Some(&mut blend))?;
            let mut raster = None;
            d.CreateRasterizerState(
                &D3D11_RASTERIZER_DESC {
                    FillMode: D3D11_FILL_SOLID,
                    CullMode: D3D11_CULL_NONE,
                    ..Default::default()
                },
                Some(&mut raster),
            )?;
            Ok(Self {
                vs: vs.unwrap(),
                ps: ps.unwrap(),
                cb: cb.unwrap(),
                sampler: sampler.unwrap(),
                blend: blend.unwrap(),
                raster: raster.unwrap(),
            })
        }
    }

    /// Binds the pipeline and a render target. Call with the device locked.
    pub fn begin(
        &self,
        ctx: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        width: u32,
        height: u32,
    ) {
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.RSSetState(&self.raster);
            ctx.OMSetBlendState(&self.blend, None, 0xffff_ffff);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        }
    }

    /// Draws one item at `rect` (pixels) on a `width`x`height` target.
    pub fn draw(
        &self,
        ctx: &ID3D11DeviceContext,
        item: &DrawItem,
        width: f32,
        height: f32,
    ) -> Result<()> {
        let r = item.rect;
        let mut p = Params {
            rect: [
                r.x / width * 2.0 - 1.0,
                1.0 - r.y / height * 2.0,
                r.w / width * 2.0,
                -r.h / height * 2.0,
            ],
            uv: [0.0, 0.0, 1.0, 1.0],
            ..Default::default()
        };
        let srv = match &item.fill {
            Fill::Texture(srv) => Some(srv.clone()),
            Fill::Opaque(srv) => {
                p.mode = 2;
                Some(srv.clone())
            }
            Fill::Solid(c) => {
                p.mode = 1;
                p.color = [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]];
                None
            }
        };
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            std::ptr::write(mapped.pData as *mut Params, p);
            ctx.Unmap(&self.cb, 0);
            ctx.PSSetShaderResources(0, Some(&[srv]));
            ctx.Draw(4, 0);
        }
        Ok(())
    }
}

pub struct Compositor {
    device: Device,
    pipeline: QuadPipeline,
    canvases: [Canvas; 2],
    front: AtomicUsize,
    pub width: u32,
    pub height: u32,
}

impl Compositor {
    pub fn new(device: &Device, width: u32, height: u32) -> Result<Self> {
        let make = || -> Result<Canvas> {
            let texture = device.texture(
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_FLAG(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0),
            )?;
            Ok(Canvas {
                rtv: device.rtv(&texture)?,
                srv: device.srv(&texture)?,
                texture,
            })
        };
        Ok(Self {
            device: device.clone(),
            pipeline: QuadPipeline::new(device)?,
            canvases: [make()?, make()?],
            front: AtomicUsize::new(0),
            width,
            height,
        })
    }

    /// Renders a frame into the back canvas and makes it the front one.
    pub fn render(&self, items: &[DrawItem]) -> Result<Canvas> {
        let back = 1 - self.front.load(Ordering::Acquire);
        let canvas = &self.canvases[back];
        {
            let _lock = self.device.lock();
            let ctx = &self.device.context;
            unsafe { ctx.ClearRenderTargetView(&canvas.rtv, &[0.0, 0.0, 0.0, 1.0]) };
            self.pipeline
                .begin(ctx, &canvas.rtv, self.width, self.height);
            for item in items {
                self.pipeline
                    .draw(ctx, item, self.width as f32, self.height as f32)?;
            }
            unsafe {
                ctx.PSSetShaderResources(0, Some(&[None]));
                ctx.OMSetRenderTargets(None, None);
            }
        }
        self.front.store(back, Ordering::Release);
        Ok(canvas.clone())
    }

    /// The most recently completed frame.
    pub fn front(&self) -> Canvas {
        self.canvases[self.front.load(Ordering::Acquire)].clone()
    }
}
