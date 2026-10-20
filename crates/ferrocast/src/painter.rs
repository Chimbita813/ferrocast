//! Minimal egui renderer on D3D11, drawing into a flip-model swap chain.
//!
//! It shares the engine's device, so the live preview is just the
//! compositor's canvas bound as a texture: no copies, no readback.

use std::collections::HashMap;

use egui::epaint::{ImageDelta, Primitive, Vertex};
use egui::{ClippedPrimitive, TextureFilter, TextureId, TexturesDelta};
use ferrocast_win::d3d::{compile_shader, Device};
use windows::core::{Interface, Result, PCSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

/// User texture id under which the preview canvas is drawn.
pub const PREVIEW: TextureId = TextureId::User(1);

const SHADER: &str = r#"
cbuffer C : register(b0) { float2 screen; float2 pad; };
struct VSIn { float2 pos : POSITION; float2 uv : TEXCOORD; float4 color : COLOR; };
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD; float4 color : COLOR; };
VSOut vs_main(VSIn i) {
    VSOut o;
    o.pos = float4(i.pos.x / screen.x * 2 - 1, 1 - i.pos.y / screen.y * 2, 0, 1);
    o.uv = i.uv;
    o.color = i.color;
    return o;
}
Texture2D t : register(t0);
SamplerState s : register(s0);
float4 ps_main(VSOut i) : SV_Target { return i.color * t.Sample(s, i.uv); }
float4 ps_opaque(VSOut i) : SV_Target { return i.color * float4(t.Sample(s, i.uv).rgb, 1); }
"#;

struct Texture {
    texture: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    nearest: bool,
}

pub struct Painter {
    device: Device,
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    size: (u32, u32),
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    ps_opaque: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    cb: ID3D11Buffer,
    linear: ID3D11SamplerState,
    nearest: ID3D11SamplerState,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    vb: Option<(ID3D11Buffer, usize)>,
    ib: Option<(ID3D11Buffer, usize)>,
    textures: HashMap<TextureId, Texture>,
}

fn sampler(device: &ID3D11Device, filter: D3D11_FILTER) -> Result<ID3D11SamplerState> {
    let mut s = None;
    unsafe {
        device.CreateSamplerState(
            &D3D11_SAMPLER_DESC {
                Filter: filter,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            },
            Some(&mut s),
        )?
    };
    Ok(s.unwrap())
}

impl Painter {
    pub fn new(device: &Device, hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        let d = &device.device;
        unsafe {
            let dxgi: IDXGIDevice = d.cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swapchain = factory.CreateSwapChainForHwnd(d, hwnd, &desc, None, None)?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);

            let vs_code = compile_shader(SHADER, "vs_main", "vs_5_0")?;
            let mut vs = None;
            d.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
            let mut ps = None;
            d.CreatePixelShader(
                &compile_shader(SHADER, "ps_main", "ps_5_0")?,
                None,
                Some(&mut ps),
            )?;
            let mut ps_opaque = None;
            d.CreatePixelShader(
                &compile_shader(SHADER, "ps_opaque", "ps_5_0")?,
                None,
                Some(&mut ps_opaque),
            )?;
            let element = |name: &'static [u8], format, offset| D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr()),
                SemanticIndex: 0,
                Format: format,
                InputSlot: 0,
                AlignedByteOffset: offset,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            };
            let elements = [
                element(b"POSITION\0", DXGI_FORMAT_R32G32_FLOAT, 0),
                element(b"TEXCOORD\0", DXGI_FORMAT_R32G32_FLOAT, 8),
                element(b"COLOR\0", DXGI_FORMAT_R8G8B8A8_UNORM, 16),
            ];
            let mut layout = None;
            d.CreateInputLayout(&elements, &vs_code, Some(&mut layout))?;
            let mut cb = None;
            d.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: 16,
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut cb),
            )?;
            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_INV_DEST_ALPHA,
                DestBlendAlpha: D3D11_BLEND_ONE,
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
                    ScissorEnable: true.into(),
                    ..Default::default()
                },
                Some(&mut raster),
            )?;
            let mut painter = Self {
                device: device.clone(),
                swapchain,
                rtv: None,
                size: (width.max(1), height.max(1)),
                vs: vs.unwrap(),
                ps: ps.unwrap(),
                ps_opaque: ps_opaque.unwrap(),
                layout: layout.unwrap(),
                cb: cb.unwrap(),
                linear: sampler(d, D3D11_FILTER_MIN_MAG_MIP_LINEAR)?,
                nearest: sampler(d, D3D11_FILTER_MIN_MAG_MIP_POINT)?,
                blend: blend.unwrap(),
                raster: raster.unwrap(),
                vb: None,
                ib: None,
                textures: HashMap::new(),
            };
            painter.create_rtv()?;
            Ok(painter)
        }
    }

    fn create_rtv(&mut self) -> Result<()> {
        let back: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0)? };
        self.rtv = Some(self.device.rtv(&back)?);
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || (width, height) == self.size {
            return Ok(());
        }
        self.rtv = None;
        unsafe {
            let _lock = self.device.lock();
            self.device.context.OMSetRenderTargets(None, None);
            self.device.context.Flush();
            self.swapchain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }
        self.size = (width, height);
        self.create_rtv()
    }

    fn update_texture(&mut self, id: TextureId, delta: &ImageDelta) -> Result<()> {
        let egui::ImageData::Color(image) = &delta.image;
        let [w, h] = image.size;
        if w == 0 || h == 0 {
            return Ok(());
        }
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(image.pixels.as_ptr() as *const u8, image.pixels.len() * 4)
        };
        let nearest = delta.options.magnification == TextureFilter::Nearest;
        match (delta.pos, self.textures.get(&id)) {
            (Some([x, y]), Some(tex)) => {
                let region = D3D11_BOX {
                    left: x as u32,
                    top: y as u32,
                    front: 0,
                    right: (x + w) as u32,
                    bottom: (y + h) as u32,
                    back: 1,
                };
                let _lock = self.device.lock();
                unsafe {
                    self.device.context.UpdateSubresource(
                        &tex.texture,
                        0,
                        Some(&region),
                        bytes.as_ptr() as *const _,
                        (w * 4) as u32,
                        0,
                    )
                };
            }
            _ => {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w as u32,
                    Height: h as u32,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let init = D3D11_SUBRESOURCE_DATA {
                    pSysMem: bytes.as_ptr() as *const _,
                    SysMemPitch: (w * 4) as u32,
                    SysMemSlicePitch: 0,
                };
                let mut texture = None;
                unsafe {
                    self.device
                        .device
                        .CreateTexture2D(&desc, Some(&init), Some(&mut texture))?
                };
                let texture = texture.unwrap();
                let srv = self.device.srv(&texture)?;
                self.textures.insert(
                    id,
                    Texture {
                        texture,
                        srv,
                        nearest,
                    },
                );
            }
        }
        Ok(())
    }

    fn ensure_buffer(
        &self,
        slot: &mut Option<(ID3D11Buffer, usize)>,
        bytes: usize,
        bind: D3D11_BIND_FLAG,
    ) -> Result<ID3D11Buffer> {
        if let Some((b, cap)) = slot {
            if *cap >= bytes {
                return Ok(b.clone());
            }
        }
        let cap = bytes.next_power_of_two().max(64 * 1024);
        let mut buf = None;
        unsafe {
            self.device.device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: cap as u32,
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: bind.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut buf),
            )?
        };
        let buf = buf.unwrap();
        *slot = Some((buf.clone(), cap));
        Ok(buf)
    }

    /// Draws one egui frame and presents it.
    pub fn paint(
        &mut self,
        primitives: &[ClippedPrimitive],
        textures: &TexturesDelta,
        pixels_per_point: f32,
        preview: Option<&ID3D11ShaderResourceView>,
    ) -> Result<()> {
        for (id, deltas) in &textures.set {
            for delta in deltas {
                self.update_texture(*id, delta)?;
            }
        }
        let meshes: Vec<(&egui::Rect, &egui::Mesh)> = primitives
            .iter()
            .filter_map(|p| match &p.primitive {
                Primitive::Mesh(m) if !m.indices.is_empty() => Some((&p.clip_rect, m)),
                _ => None,
            })
            .collect();
        let vcount: usize = meshes.iter().map(|(_, m)| m.vertices.len()).sum();
        let icount: usize = meshes.iter().map(|(_, m)| m.indices.len()).sum();
        let mut vb_slot = self.vb.take();
        let mut ib_slot = self.ib.take();
        let vb = self.ensure_buffer(
            &mut vb_slot,
            vcount.max(1) * std::mem::size_of::<Vertex>(),
            D3D11_BIND_VERTEX_BUFFER,
        )?;
        let ib = self.ensure_buffer(&mut ib_slot, icount.max(1) * 4, D3D11_BIND_INDEX_BUFFER)?;
        self.vb = vb_slot;
        self.ib = ib_slot;

        let (w, h) = self.size;
        let ctx = &self.device.context;
        {
            let _lock = self.device.lock();
            unsafe {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&vb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
                let mut vptr = mapped.pData as *mut Vertex;
                for (_, m) in &meshes {
                    std::ptr::copy_nonoverlapping(m.vertices.as_ptr(), vptr, m.vertices.len());
                    vptr = vptr.add(m.vertices.len());
                }
                ctx.Unmap(&vb, 0);
                ctx.Map(&ib, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
                let mut iptr = mapped.pData as *mut u32;
                for (_, m) in &meshes {
                    std::ptr::copy_nonoverlapping(m.indices.as_ptr(), iptr, m.indices.len());
                    iptr = iptr.add(m.indices.len());
                }
                ctx.Unmap(&ib, 0);
                ctx.Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
                let screen = [
                    w as f32 / pixels_per_point,
                    h as f32 / pixels_per_point,
                    0.0,
                    0.0,
                ];
                std::ptr::write(mapped.pData as *mut [f32; 4], screen);
                ctx.Unmap(&self.cb, 0);

                let rtv = self.rtv.clone().unwrap();
                ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
                ctx.ClearRenderTargetView(&rtv, &[0.06, 0.06, 0.07, 1.0]);
                ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: w as f32,
                    Height: h as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]));
                ctx.RSSetState(&self.raster);
                ctx.OMSetBlendState(&self.blend, None, 0xffff_ffff);
                ctx.IASetInputLayout(&self.layout);
                ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                let stride = std::mem::size_of::<Vertex>() as u32;
                let offset = 0u32;
                ctx.IASetVertexBuffers(0, 1, Some(&Some(vb.clone())), Some(&stride), Some(&offset));
                ctx.IASetIndexBuffer(&ib, DXGI_FORMAT_R32_UINT, 0);
                ctx.VSSetShader(&self.vs, None);
                ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));

                let (mut base_vertex, mut first_index) = (0i32, 0u32);
                for (clip, m) in &meshes {
                    let (srv, nearest, opaque) = if m.texture_id == PREVIEW {
                        (preview.cloned(), false, true)
                    } else {
                        match self.textures.get(&m.texture_id) {
                            Some(t) => (Some(t.srv.clone()), t.nearest, false),
                            None => (None, false, false),
                        }
                    };
                    let x0 = (clip.min.x * pixels_per_point).round().clamp(0.0, w as f32) as i32;
                    let y0 = (clip.min.y * pixels_per_point).round().clamp(0.0, h as f32) as i32;
                    let x1 = (clip.max.x * pixels_per_point).round().clamp(0.0, w as f32) as i32;
                    let y1 = (clip.max.y * pixels_per_point).round().clamp(0.0, h as f32) as i32;
                    if srv.is_some() && x1 > x0 && y1 > y0 {
                        ctx.RSSetScissorRects(Some(&[RECT {
                            left: x0,
                            top: y0,
                            right: x1,
                            bottom: y1,
                        }]));
                        ctx.PSSetShader(if opaque { &self.ps_opaque } else { &self.ps }, None);
                        ctx.PSSetShaderResources(0, Some(&[srv]));
                        ctx.PSSetSamplers(
                            0,
                            Some(&[Some(if nearest {
                                self.nearest.clone()
                            } else {
                                self.linear.clone()
                            })]),
                        );
                        ctx.DrawIndexed(m.indices.len() as u32, first_index, base_vertex);
                    }
                    base_vertex += m.vertices.len() as i32;
                    first_index += m.indices.len() as u32;
                }
                ctx.PSSetShaderResources(0, Some(&[None]));
                ctx.OMSetRenderTargets(None, None);
            }
        }
        unsafe {
            let _ = self.swapchain.Present(1, DXGI_PRESENT(0));
        }
        for id in &textures.free {
            self.textures.remove(id);
        }
        Ok(())
    }
}
