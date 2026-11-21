//! The wgpu renderer for AntiWebUI scenes.

#[cfg(windows)]
mod adapters;
pub mod canvas;
mod custom_shader;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod dmabuf;
#[cfg(target_os = "linux")]
mod drivers;
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod early;
mod shader_cache;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub use drivers::limit_vulkan_drivers;
mod effects;
#[cfg(target_os = "linux")]
mod gbm;
mod glyph_cache;
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod iosurface;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod linux_sync;
mod path;
mod renderer;
mod video;

use anti_webui_core::scene::Scene;
pub use custom_shader::check_shader;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, RawDisplayHandle, RawWindowHandle,
};
pub use renderer::Renderer;
/// The wgpu crate this one is built on, which a `gpu_canvas` draws with: AntiWebUI is on a fork of
/// it, whose types aren't those of the wgpu on crates.io. Going to another major version of it is a
/// breaking change.
pub use wgpu;

/// What the device is asked to allow: what WebGPU allows by default where the adapter can, so that
/// what a `gpu_canvas` draws can use storage buffers and compute, and what WebGL 2 does elsewhere.
/// Either way, textures can be as big as the adapter has them. See `docs/design/41-gpu-canvas.md`.
pub(crate) fn device_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    let has = adapter.limits();
    let base = wgpu::Limits::default();
    let base = if base.check_limits(&has) {
        base
    } else {
        wgpu::Limits::downlevel_webgl2_defaults()
    };
    base.using_resolution(has)
}

#[derive(Debug)]
pub enum GpuError {
    Surface(wgpu::CreateSurfaceError),
    Adapter(wgpu::RequestAdapterError),
    Device(wgpu::RequestDeviceError),
    /// Our own display buffers are unavailable; use a surface instead.
    Dmabuf(String),
    /// Our own display buffers on macOS are unavailable.
    IoSurface(String),
    /// The platform can't make a GPU surface here, like on a thread other than AppKit's.
    NoSurface,
    /// The adapter offers no format for this surface (yet): nothing can be shown on it. The
    /// caller decides whether to try again later.
    NoFormat,
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuError::Surface(e) => write!(f, "failed to create surface: {e}"),
            GpuError::NoFormat => write!(f, "the adapter offers no format for this surface"),
            GpuError::Adapter(e) => write!(f, "no suitable GPU adapter: {e}"),
            GpuError::Device(e) => write!(f, "failed to create GPU device: {e}"),
            GpuError::Dmabuf(e) => write!(f, "dmabuf presentation unavailable: {e}"),
            GpuError::IoSurface(e) => write!(f, "IOSurface presentation unavailable: {e}"),
            GpuError::NoSurface => f.write_str("no GPU surface can be made on this thread"),
        }
    }
}

impl std::error::Error for GpuError {}

/// A display connection handed to wgpu (needed by the GL backend on Wayland).
#[derive(Debug)]
struct Display(RawDisplayHandle);

// SAFETY: the handle is only an identifier; the platform keeps the connection alive for the
// lifetime of the GPU context.
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

impl HasDisplayHandle for Display {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // SAFETY: see above.
        Ok(unsafe { DisplayHandle::borrow_raw(self.0) })
    }
}

/// The instance and adapter of everything headless, for the life of the process. The Vulkan loader
/// crashes when one instance is created while another is destroyed, which tests running side by
/// side would do.
static HEADLESS: std::sync::OnceLock<
    Result<(wgpu::Instance, wgpu::Adapter), wgpu::RequestAdapterError>,
> = std::sync::OnceLock::new();

/// What is good to have for video frames of more than 8 bits, if it is there.
pub(crate) fn video_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    adapter.features() & wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
}

/// Shared GPU objects.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Compiled shaders saved for the next launch, where the backend can keep them.
    shader_cache: Option<std::sync::Arc<shader_cache::ShaderCache>>,
}

/// What a window's instance is made with. `filter` is the DX12 adapters to look at (Windows).
fn instance_desc(
    display: RawDisplayHandle,
    backends: wgpu::Backends,
    explicit: bool,
    #[cfg(windows)] filter: wgpu::Dx12AdapterFilter,
    #[cfg(not(windows))] _filter: (),
) -> wgpu::InstanceDescriptor {
    let mut desc =
        wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(Display(display)));
    if !explicit {
        desc.backends = backends;
    }
    // Nothing is drawn indirectly; checking indirect draws makes compute pipelines when the device
    // is made (about 25 ms on DX12, where FXC compiles them).
    if std::env::var_os("WGPU_VALIDATION_INDIRECT_CALL").is_none() {
        desc.flags
            .remove(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);
    }
    #[cfg(windows)]
    {
        desc.backend_options.dx12.adapter_filter = filter;
        // A swapchain on a DirectComposition visual has alpha, which popups need; one on the
        // window's handle is opaque (see `docs/design/39-platform-windows.md`).
        if std::env::var_os("WGPU_DX12_PRESENTATION_SYSTEM").is_none() {
            desc.backend_options.dx12.presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;
        }
    }
    desc
}

/// A window surface and its renderer.
#[doc(hidden)]
pub struct WindowRenderer {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    /// What frames are drawn into on Windows: the size of the window, and kept, so that only what
    /// changed is drawn again. Each frame is copied to the swapchain, which is bigger than the
    /// window while it is resized. `None` where the swapchain takes no copies.
    #[cfg(windows)]
    retained: Option<Option<Offscreen>>,
    /// Our own swapchain, which tells DWM what changed; then `surface` is left unconfigured.
    #[cfg(windows)]
    dxgi: Option<dxgi::Swapchain>,
}

impl Gpu {
    /// Creates the GPU context together with the first window's surface.
    ///
    /// # Safety
    /// The display and window handles must stay valid until the returned objects are dropped.
    pub unsafe fn with_window(
        display: RawDisplayHandle,
        window: RawWindowHandle,
        width: u32,
        height: u32,
    ) -> Result<(Gpu, WindowRenderer), GpuError> {
        // Try the native APIs first and fall back to GL only if they have no adapter: GL drivers
        // are much heavier to load (Mesa's GL pulls in LLVM, for example).
        let explicit = std::env::var_os("WGPU_BACKEND").is_some();
        // On Windows DX12 comes first: only its swapchain on a DirectComposition visual has alpha
        // (Vulkan's on a window is opaque, which shows popups' shadows black), and it resizes with
        // the window without being made again (see `docs/design/39-platform-windows.md`).
        let attempts: &[wgpu::Backends] = if explicit {
            &[wgpu::Backends::all()]
        } else if cfg!(windows) {
            &[
                wgpu::Backends::DX12,
                wgpu::Backends::PRIMARY,
                wgpu::Backends::GL,
            ]
        } else {
            &[wgpu::Backends::PRIMARY, wgpu::Backends::GL]
        };
        // Only the low-power GPU: wgpu makes a device on each adapter it lists, which wakes a
        // sleeping dGPU for up to seconds. If that one has no adapter, the next tries have all.
        #[cfg(windows)]
        let mut adapters = adapters::preferred();
        // The GPU made ahead on another thread, if it is that one.
        #[cfg(windows)]
        if let Some(gpu) = early::take()
            && adapters::luid_of(&gpu.adapter).is_some_and(|l| adapters.allows(l))
        {
            // SAFETY: forwarded to the caller.
            let surface = unsafe {
                gpu.instance
                    .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: Some(display),
                        raw_window_handle: window,
                    })
            };
            if let Ok(surface) = surface
                && gpu.adapter.is_surface_supported(&surface)
            {
                let window = gpu.configure_window(surface, window, width, height)?;
                return Ok((gpu, window));
            }
        }
        let mut last_error = None;
        let mut found = None;
        for &backends in attempts {
            #[cfg(windows)]
            let filter = std::mem::take(&mut adapters);
            #[cfg(not(windows))]
            let filter = ();
            let instance = wgpu::Instance::new(instance_desc(display, backends, explicit, filter));
            // SAFETY: forwarded to the caller.
            let surface = unsafe {
                instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(display),
                    raw_window_handle: window,
                })
            };
            let surface = match surface {
                Ok(s) => s,
                Err(e) => {
                    last_error = Some(GpuError::Surface(e));
                    continue;
                }
            };
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                    apply_limit_buckets: false,
                }));
            match adapter {
                Ok(adapter) => {
                    found = Some((instance, surface, adapter));
                    break;
                }
                Err(e) => last_error = Some(GpuError::Adapter(e)),
            }
        }
        let Some((instance, surface, adapter)) = found else {
            return Err(last_error.expect("at least one backend was tried"));
        };
        let gpu = Gpu::open(instance, adapter)?;
        let window = gpu.configure_window(surface, window, width, height)?;
        Ok((gpu, window))
    }

    /// Makes the device on the adapter, with the cache of compiled shaders where there is one.
    fn open(instance: wgpu::Instance, adapter: wgpu::Adapter) -> Result<Gpu, GpuError> {
        let cache_feature = if shader_cache::ShaderCache::wanted(&adapter) {
            wgpu::Features::PIPELINE_CACHE
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("anti-webui"),
            required_features: video_features(&adapter) | cache_feature,
            required_limits: crate::device_limits(&adapter),
            ..Default::default()
        }))
        .map_err(GpuError::Device)?;
        let shader_cache = shader_cache::ShaderCache::load(&device).map(std::sync::Arc::new);
        Ok(Gpu {
            instance,
            adapter,
            device,
            queue,
            shader_cache,
        })
    }

    /// Starts making the GPU on another thread, so that it is (nearly) ready when the first window
    /// asks for it in [`Gpu::with_window`]: making the window and having it activated (IME and
    /// all) takes about as long. Windows only; elsewhere it does nothing.
    pub fn start_early() {
        #[cfg(windows)]
        early::start();
    }

    /// Creates a surface for another window, like a popup, with a renderer of its own.
    ///
    /// # Safety
    /// The display and window handles must stay valid until the returned object is dropped.
    pub unsafe fn add_window(
        &self,
        display: RawDisplayHandle,
        window: RawWindowHandle,
        width: u32,
        height: u32,
    ) -> Result<WindowRenderer, GpuError> {
        // SAFETY: forwarded to the caller.
        let surface = unsafe {
            self.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(display),
                    raw_window_handle: window,
                })
        }
        .map_err(GpuError::Surface)?;
        self.configure_window(surface, window, width, height)
    }

    /// Creates a GPU context without a window, for offscreen rendering.
    pub fn headless() -> Result<Gpu, GpuError> {
        let (instance, adapter) = HEADLESS
            .get_or_init(|| {
                let instance = wgpu::Instance::new(
                    wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
                );
                let adapter = pollster::block_on(
                    instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
                )?;
                Ok((instance, adapter))
            })
            .clone()
            .map_err(GpuError::Adapter)?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_features: video_features(&adapter),
            ..Default::default()
        }))
        .map_err(GpuError::Device)?;
        Ok(Gpu {
            instance,
            adapter,
            device,
            queue,
            shader_cache: None,
        })
    }

    fn configure_window(
        &self,
        surface: wgpu::Surface<'static>,
        window: RawWindowHandle,
        width: u32,
        height: u32,
    ) -> Result<WindowRenderer, GpuError> {
        let caps = surface.get_capabilities(&self.adapter);
        // None: the adapter cannot present on this surface, at least not yet (seen with a
        // software adapter on a surface just made). There is no format to configure it with;
        // the caller gets an error instead of a panic on an empty list.
        let Some(&first) = caps.formats.first() else {
            return Err(GpuError::NoFormat);
        };
        // Non-sRGB formats blend in gamma space, like browsers do.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| {
                matches!(
                    f,
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                )
            })
            .unwrap_or(first);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        // Popups have transparent corners and shadows; the window says it is opaque by other means.
        // Metal has no `PreMultiplied`, and `Opaque` makes the layer opaque. Core Animation takes
        // the pixels as premultiplied either way, so `PostMultiplied` only lets the layer have alpha.
        let alpha_mode = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            #[cfg(target_os = "macos")]
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::Opaque,
        ]
        .into_iter()
        .find(|m| caps.alpha_modes.contains(m))
        .or_else(|| caps.alpha_modes.first().copied())
        .unwrap_or(wgpu::CompositeAlphaMode::Auto);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
            color_space: Default::default(),
        };
        #[cfg(windows)]
        let (config, retained, dxgi) = {
            let mut config = config;
            let dxgi = self.own_swapchain(window, &config);
            // Made again with every new size, a swapchain waits for the GPU to finish everything
            // (25 ms on a Radeon 880M): it is given room to grow (see "提示" in
            // `docs/design/39-platform-windows.md`).
            let copies = caps.usages.contains(wgpu::TextureUsages::COPY_DST);
            if dxgi.is_none() {
                if copies {
                    config.usage |= wgpu::TextureUsages::COPY_DST;
                    config.width = swapchain_side(config.width, 0);
                    config.height = swapchain_side(config.height, 0);
                }
                surface.configure(&self.device, &config);
            }
            (config, (copies || dxgi.is_some()).then_some(None), dxgi)
        };
        #[cfg(not(windows))]
        {
            let _ = window;
            surface.configure(&self.device, &config);
        }
        Ok(WindowRenderer {
            surface,
            config,
            renderer: Renderer::with_cache(&self.device, format, self.shader_cache.clone()),
            #[cfg(windows)]
            retained,
            #[cfg(windows)]
            dxgi,
        })
    }

    /// Our own swapchain for the window, if wgpu draws with DX12 and it can be made: it tells DWM
    /// what changed in each frame (see "提示" in `docs/design/39-platform-windows.md`).
    /// `ANTIWEBUI_PRESENT=surface` uses wgpu's surface instead.
    #[cfg(windows)]
    fn own_swapchain(
        &self,
        window: RawWindowHandle,
        config: &wgpu::SurfaceConfiguration,
    ) -> Option<dxgi::Swapchain> {
        if std::env::var("ANTIWEBUI_PRESENT").is_ok_and(|v| v == "surface")
            || self.adapter.get_info().backend != wgpu::Backend::Dx12
        {
            return None;
        }
        let RawWindowHandle::Win32(handle) = window else {
            return None;
        };
        let hwnd = windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _);
        let size = (
            swapchain_side(config.width, 0),
            swapchain_side(config.height, 0),
        );
        let premultiplied = config.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied;
        // SAFETY: the caller of `with_window` or `add_window` keeps the window.
        match unsafe { dxgi::Swapchain::new(self, hwnd, size, config.format, premultiplied) } {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("[anti-webui] no DXGI swapchain of our own ({e}); using wgpu's surface");
                None
            }
        }
    }

    /// Renders a scene offscreen and returns tightly packed RGBA8 pixels.
    pub fn render_to_rgba(&self, renderer: &mut Renderer, scene: &Scene) -> Vec<u8> {
        let target = Offscreen::new(self, renderer.format(), scene.width, scene.height);
        let mut encoder = self.device.create_command_encoder(&Default::default());
        renderer.render(&self.device, &self.queue, &mut encoder, &target.view, scene);
        self.queue.submit([encoder.finish()]);
        target.read_rgba(self)
    }
}

/// An offscreen render target that keeps its pixels between frames.
pub struct Offscreen {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
}

impl Offscreen {
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat, width: u32, height: u32) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Self { texture, view }
    }

    /// Reads the pixels back as tightly packed RGBA8.
    pub fn read_rgba(&self, gpu: &Gpu) -> Vec<u8> {
        let (w, h) = (self.texture.width(), self.texture.height());
        let padded = (w * 4).div_ceil(256) * 256;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
        let data = slice.get_mapped_range().expect("readback buffer is mapped");
        let bgra = self.texture.format() == wgpu::TextureFormat::Bgra8Unorm;
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h {
            let start = (row * padded) as usize;
            let row = &data[start..start + (w * 4) as usize];
            if bgra {
                for px in row.chunks_exact(4) {
                    out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            } else {
                out.extend_from_slice(row);
            }
        }
        out
    }
}

/// The outcome of presenting a frame.
#[derive(Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum FrameResult {
    Presented,
    /// Nothing was presented; try again on the next frame.
    Skipped,
}

/// The side of a swapchain that shows `need` pixels, given what it has: room to grow by a step,
/// and smaller only once it is much too big.
#[cfg(windows)]
fn swapchain_side(need: u32, have: u32) -> u32 {
    const STEP: u32 = 256;
    let need = need.max(1);
    if need > have || need + 2 * STEP < have {
        (need + STEP / 2).div_ceil(STEP) * STEP
    } else {
        have
    }
}

impl WindowRenderer {
    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        // Our own swapchain is fitted to each frame.
        #[cfg(windows)]
        if self.dxgi.is_some() {
            return;
        }
        let (w, h) = (width.max(1), height.max(1));
        if self.config.width != w || self.config.height != h {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&gpu.device, &self.config);
        }
    }

    /// How frames get to the screen, for reports.
    pub fn path(&self) -> &'static str {
        #[cfg(windows)]
        if self.dxgi.is_some() {
            return "DXGI swapchain";
        }
        "wgpu surface"
    }

    /// How many frames old the buffer the last frame went to was, where that is known.
    pub fn last_buffer_age(&self) -> Option<u64> {
        #[cfg(windows)]
        if let Some(d) = &self.dxgi {
            return d.last_age();
        }
        None
    }

    pub fn size(&self) -> (u32, u32) {
        #[cfg(windows)]
        if let Some(d) = &self.dxgi {
            return d.size();
        }
        (self.config.width, self.config.height)
    }

    pub fn renderer(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Renders and presents a scene, which differs from the last presented in `damage`. Returns how
    /// many pixels were drawn.
    pub fn present_damaged(
        &mut self,
        gpu: &Gpu,
        scene: &Scene,
        damage: &anti_webui_core::damage::Damage,
    ) -> (FrameResult, i64) {
        #[cfg(windows)]
        if self.retained.is_some() {
            return self.present_retained(gpu, scene, damage);
        }
        let _ = damage;
        let full = scene.width as i64 * scene.height as i64;
        (self.present(gpu, scene), full)
    }

    /// Draws what changed into the frame kept, and copies it to the top left of the swapchain.
    #[cfg(windows)]
    fn present_retained(
        &mut self,
        gpu: &Gpu,
        scene: &Scene,
        damage: &anti_webui_core::damage::Damage,
    ) -> (FrameResult, i64) {
        let (w, h) = (scene.width.max(1), scene.height.max(1));
        let frame = if let Some(dxgi) = &mut self.dxgi {
            let have = dxgi.size();
            let side = (swapchain_side(w, have.0), swapchain_side(h, have.1));
            if side != have
                && let Err(e) = dxgi.resize(gpu, side)
            {
                eprintln!("[anti-webui] resizing the swapchain failed: {e}");
                return (FrameResult::Skipped, 0);
            }
            None
        } else {
            let side = (
                swapchain_side(w, self.config.width),
                swapchain_side(h, self.config.height),
            );
            if side != (self.config.width, self.config.height) {
                self.config.width = side.0;
                self.config.height = side.1;
                self.surface.configure(&gpu.device, &self.config);
            }
            match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    self.surface.configure(&gpu.device, &self.config);
                    return (FrameResult::Skipped, 0);
                }
                _ => return (FrameResult::Skipped, 0),
            }
        };
        let slot = self.retained.as_mut().expect("retained frames");
        let fresh = slot
            .as_ref()
            .is_none_or(|o| (o.texture.width(), o.texture.height()) != (w, h));
        if fresh {
            *slot = Some(Offscreen::new(gpu, self.renderer.format(), w, h));
        }
        let kept = slot.as_ref().expect("just made");
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        let full = w as i64 * h as i64;
        let drawn = if fresh {
            self.renderer
                .render(&gpu.device, &gpu.queue, &mut encoder, &kept.view, scene);
            full
        } else {
            self.renderer.render_damaged(
                &gpu.device,
                &gpu.queue,
                &mut encoder,
                &kept.view,
                scene,
                damage,
            );
            damage.area().min(full)
        };
        let Some(frame) = frame else {
            let dxgi = self.dxgi.as_mut().expect("no frame without it");
            return match dxgi.present(gpu, encoder, kept, (w, h), damage, fresh) {
                Ok(()) => (FrameResult::Presented, drawn),
                Err(e) => {
                    eprintln!("[anti-webui] presenting failed: {e}");
                    (FrameResult::Skipped, 0)
                }
            };
        };
        encoder.copy_texture_to_texture(
            kept.texture.as_image_copy(),
            frame.texture.as_image_copy(),
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit([encoder.finish()]);
        gpu.queue.present(frame);
        (FrameResult::Presented, drawn)
    }

    /// Renders and presents a scene.
    pub fn present(&mut self, gpu: &Gpu, scene: &Scene) -> FrameResult {
        #[cfg(windows)]
        if self.retained.is_some() {
            let full = anti_webui_core::damage::Damage::full(scene.width, scene.height);
            return self.present_retained(gpu, scene, &full).0;
        }
        self.resize(gpu, scene.width, scene.height);
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&gpu.device, &self.config);
                return FrameResult::Skipped;
            }
            _ => return FrameResult::Skipped,
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        self.renderer
            .render(&gpu.device, &gpu.queue, &mut encoder, &view, scene);
        gpu.queue.submit([encoder.finish()]);
        gpu.queue.present(frame);
        FrameResult::Presented
    }
}
