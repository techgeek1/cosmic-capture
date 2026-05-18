//! Streaming-texture thumbnail widget.
//!
//! Each tile owns an `Arc<SharedFrame>` — a publish slot for raw RGBA8
//! frames pushed in from a `toplevel_capture::start` mpsc. The widget is
//! an `iced::widget::shader::Shader` whose `Primitive::prepare` reads the
//! shared slot, lazily allocates / resizes a per-tile `wgpu::Texture`,
//! and uploads via `queue.write_texture` only when the version counter
//! has advanced. `draw` reuses iced's render pass — viewport is already
//! the widget bounds, scissor the clip rect — so this is just a textured
//! fullscreen triangle.
//!
//! Why a shared `Arc<SharedFrame>` instead of passing frames through
//! `Msg`: the `image::Handle` route works only by allocating a fresh
//! handle (= fresh wgpu cache key) per frame, which evicts and re-uploads
//! the texture every tick — synchronized across tiles, that reads as a
//! whole-picker flash. Owning the texture in our own pipeline storage,
//! keyed by an id that's stable for the lifetime of the tile, eliminates
//! the eviction.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use cosmic::iced::advanced::widget::{Widget, tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, layout, mouse, renderer};
use cosmic::iced::widget::shader::{self, Viewport};
use cosmic::iced::{ContentFit, Element, Event, Length, Rectangle, Size, wgpu};
use tokio::sync::mpsc as tmpsc;

/// Allocate a fresh id for a `SharedFrame`. Stable for the slot's
/// lifetime; used as the key in `ThumbPipeline.textures` so the per-tile
/// `wgpu::Texture` survives across redraws.
fn next_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Global "wake iced" bus used by the per-tile pump tasks. iced never
/// redraws without an event, so each new frame published to a
/// `SharedFrame` also pings this channel; a subscription on the GUI side
/// forwards the ping as a no-op `Msg`, which triggers redraw.
///
/// One global is fine: there's only one iced application alive at a time,
/// and pings collapse — twenty arriving between frames still cost one
/// redraw.
struct RedrawBus {
    sender: tmpsc::UnboundedSender<()>,
    receiver: Mutex<Option<tmpsc::UnboundedReceiver<()>>>,
}

fn redraw_bus() -> &'static RedrawBus {
    static BUS: OnceLock<RedrawBus> = OnceLock::new();
    BUS.get_or_init(|| {
        let (tx, rx) = tmpsc::unbounded_channel();
        RedrawBus {
            sender: tx,
            receiver: Mutex::new(Some(rx)),
        }
    })
}

/// Wake iced. Called by per-tile pump tasks after publishing a frame.
pub fn nudge_redraw() {
    let _ = redraw_bus().sender.send(());
}

/// Take the redraw receiver. Returns `Some` exactly once per process —
/// the GUI subscription owns it for the lifetime of the app. Subsequent
/// calls get `None` and the subscription should park itself.
pub fn take_redraw_receiver() -> Option<tmpsc::UnboundedReceiver<()>> {
    redraw_bus().receiver.lock().unwrap().take()
}

/// Raw frame contents in cosmic-screencopy native byte order (RGBA8).
/// `stride` may exceed `width * 4` if the compositor padded rows.
pub struct RawFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Publish slot for one tile. Frame producers (the per-toplevel capture
/// task) lock and overwrite `latest` then bump `version`. The widget
/// reads `version` on every `prepare`; if it's advanced past the
/// last-uploaded counter, it pulls `latest` and uploads.
pub struct SharedFrame {
    id: u64,
    version: AtomicU64,
    latest: Mutex<Option<RawFrame>>,
}

impl std::fmt::Debug for SharedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedFrame")
            .field("id", &self.id)
            .field("version", &self.version.load(Ordering::Relaxed))
            .finish()
    }
}

impl SharedFrame {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            id: next_id(),
            version: AtomicU64::new(0),
            latest: Mutex::new(None),
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Publish a new frame. Bumps `version` so the next `prepare` will
    /// upload it.
    pub fn publish(&self, frame: RawFrame) {
        *self.latest.lock().unwrap() = Some(frame);
        self.version.fetch_add(1, Ordering::Release);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    fn take_if_newer(&self, last_seen: u64) -> Option<(RawFrame, u64)> {
        let v = self.version();
        if v == last_seen {
            return None;
        }
        let frame = self.latest.lock().unwrap().take()?;
        Some((frame, v))
    }
}

/// The `shader::Program`. One per tile, holding an `Arc<SharedFrame>`.
#[derive(Clone)]
pub struct StreamingThumb {
    shared: Arc<SharedFrame>,
}

impl StreamingThumb {
    pub fn new(shared: Arc<SharedFrame>) -> Self {
        Self { shared }
    }
}

impl<Message> shader::Program<Message> for StreamingThumb {
    type State = ();
    type Primitive = ThumbPrimitive;

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        ThumbPrimitive {
            shared: self.shared.clone(),
        }
    }
}

#[derive(Debug)]
pub struct ThumbPrimitive {
    shared: Arc<SharedFrame>,
}

impl shader::Primitive for ThumbPrimitive {
    type Pipeline = ThumbPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        pipeline.prepare_tile(device, queue, &self.shared);
    }

    fn draw(
        &self,
        pipeline: &Self::Pipeline,
        render_pass: &mut wgpu::RenderPass<'_>,
    ) -> bool {
        pipeline.draw_tile(render_pass, self.shared.id);
        true
    }
}

/// Per-tile GPU resources cached inside the shared `Pipeline`.
struct TextureEntry {
    /// Weak handle to the producer side. Used by `trim` to detect tiles
    /// whose `SharedFrame` has been dropped (widget gone, capture
    /// session torn down) and free the GPU memory.
    weak: Weak<SharedFrame>,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    last_version: u64,
}

pub struct ThumbPipeline {
    render_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    textures: HashMap<u64, TextureEntry>,
}

impl std::fmt::Debug for ThumbPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThumbPipeline")
            .field("tiles", &self.textures.len())
            .finish()
    }
}

const SHADER_SRC: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) ix: u32) -> VsOut {
    // Fullscreen triangle. Iced sets the render pass viewport to the
    // widget bounds, so this triangle is automatically clipped/scaled
    // to the tile rect — no transform uniform needed.
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let p = positions[ix];
    var out: VsOut;
    out.pos = vec4<f32>(p, 0.0, 1.0);
    // UV: (0,0) at top-left. wgpu NDC has +y up; viewport flips it on
    // its way to framebuffer space, so we invert here.
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

impl shader::Pipeline for ThumbPipeline {
    fn new(
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("streaming_thumb shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("streaming_thumb bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float {
                                filterable: true,
                            },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(
                            wgpu::SamplerBindingType::Filtering,
                        ),
                        count: None,
                    },
                ],
            });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("streaming_thumb pl"),
                bind_group_layouts: &[&bind_group_layout],
                immediate_size: 0,
            });

        let render_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("streaming_thumb rp"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("streaming_thumb sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self {
            render_pipeline,
            bind_group_layout,
            sampler,
            textures: HashMap::new(),
        }
    }

    fn trim(&mut self) {
        // Drop entries whose producer is gone (capture session torn
        // down, widget left the tree). Frees GPU memory without
        // needing the app to call us back.
        self.textures
            .retain(|_, entry| entry.weak.strong_count() > 0);
    }
}

impl ThumbPipeline {
    fn prepare_tile(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        shared: &Arc<SharedFrame>,
    ) {
        let id = shared.id;
        let current_version = shared.version();

        // Fast path: nothing new.
        if let Some(entry) = self.textures.get(&id) {
            if entry.last_version == current_version {
                return;
            }
        }

        let Some((frame, version)) = shared.take_if_newer(
            self.textures.get(&id).map(|e| e.last_version).unwrap_or(0),
        ) else {
            return;
        };

        // (Re)create the texture if missing or size changed.
        let needs_realloc = match self.textures.get(&id) {
            None => true,
            Some(e) => e.width != frame.width || e.height != frame.height,
        };

        if needs_realloc {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("streaming_thumb tex"),
                size: wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // `Rgba8Unorm` (no sRGB linearization) is what iced's
                // own image atlas uses when the `web-colors` feature
                // is on (see iced_wgpu::image::atlas.rs +
                // graphics::color::GAMMA_CORRECTION). libcosmic
                // enables `web-colors`, so iced runs the whole pipeline
                // in gamma-encoded values and the surface is non-sRGB.
                // Using `Rgba8UnormSrgb` here would linearize on sample
                // and write linear to that non-sRGB target — pixels
                // come out darker than the source frame.
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("streaming_thumb bg"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.textures.insert(
                id,
                TextureEntry {
                    weak: Arc::downgrade(shared),
                    texture,
                    bind_group,
                    width: frame.width,
                    height: frame.height,
                    last_version: 0,
                },
            );
        }

        let entry = self.textures.get_mut(&id).expect("just inserted");
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &entry.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.stride.max(frame.width * 4)),
                rows_per_image: Some(frame.height),
            },
            wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
        );
        entry.last_version = version;
    }

    fn draw_tile(&self, pass: &mut wgpu::RenderPass<'_>, id: u64) {
        let Some(entry) = self.textures.get(&id) else {
            // No texture yet (first frame hasn't arrived). Leave the
            // bounds transparent — iced will composite whatever's
            // underneath. Cheaper than upload-a-blank.
            return;
        };
        pass.set_pipeline(&self.render_pipeline);
        pass.set_bind_group(0, &entry.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// A `Widget` wrapper around `Shader<StreamingThumb>` that reports an
/// *intrinsic* size — behaving like `iced::widget::image` for layout
/// purposes — instead of the bare `Shader`'s fixed Length defaults. In a
/// `FillPortion` row inside a `button::custom`, the bare Shader either
/// stretches to fill (wrong aspect) or collapses to 0 (Shrink parent
/// can't shrink-around Fill); this wrapper resolves the parent's limit
/// against the source dimensions with `ContentFit::ScaleDown` — the
/// same algorithm `iced::widget::image` uses with its intrinsic texture
/// dimensions.
///
/// All other Widget methods forward to the wrapped `Shader`. The clever
/// bit: by writing `where Shader<...>: Widget<...>` in the impl, the
/// Renderer bound that `Shader` requires (the private
/// `iced_wgpu::primitive::Renderer` trait) is propagated implicitly,
/// avoiding the need to name it from our crate.
pub struct IntrinsicShader<Message> {
    inner: shader::Shader<Message, StreamingThumb>,
    intrinsic: Size<f32>,
}

impl<Message> IntrinsicShader<Message> {
    pub fn new(shared: Arc<SharedFrame>, intrinsic_width: u32, intrinsic_height: u32) -> Self {
        // `Length::Fill` on the inner so when our layout assigns the
        // wrapper a final size, the Shader fills exactly that rect.
        let inner = shader::Shader::new(StreamingThumb::new(shared))
            .width(Length::Fill)
            .height(Length::Fill);
        Self {
            inner,
            intrinsic: Size::new(
                intrinsic_width.max(1) as f32,
                intrinsic_height.max(1) as f32,
            ),
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for IntrinsicShader<Message>
where
    Renderer: cosmic::iced::advanced::Renderer,
    shader::Shader<Message, StreamingThumb>: Widget<Message, Theme, Renderer>,
{
    fn tag(&self) -> tree::Tag {
        Widget::<Message, Theme, Renderer>::tag(&self.inner)
    }

    fn state(&self) -> tree::State {
        Widget::<Message, Theme, Renderer>::state(&self.inner)
    }

    fn children(&self) -> Vec<tree::Tree> {
        Widget::<Message, Theme, Renderer>::children(&self.inner)
    }

    fn diff(&mut self, tree: &mut tree::Tree) {
        Widget::<Message, Theme, Renderer>::diff(&mut self.inner, tree);
    }

    fn size(&self) -> Size<Length> {
        // Shrink/Shrink mirrors `iced::widget::Image::size`, which is
        // what makes the picker's `FillPortion`-column shrink-around
        // behavior work the same way as the original static tiles.
        Size {
            width: Length::Shrink,
            height: Length::Shrink,
        }
    }

    fn layout(
        &mut self,
        tree: &mut tree::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // Mirror `iced::widget::image::layout`: resolve parent limits
        // with the intrinsic size as the Shrink hint, fit via
        // `ScaleDown`, then shrink the final size to the fit.
        let bounds = limits.resolve(Length::Shrink, Length::Shrink, self.intrinsic);
        let fit = ContentFit::ScaleDown.fit(self.intrinsic, bounds);
        let final_size = Size {
            width: bounds.width.min(fit.width),
            height: bounds.height.min(fit.height),
        };
        // Run the Shader's own layout against tight limits so its
        // internal `atomic` layout produces a node sized to our final
        // rect — its draw uses this layout's bounds to derive the
        // wgpu viewport for the primitive.
        let tight = layout::Limits::new(final_size, final_size);
        Widget::<Message, Theme, Renderer>::layout(&mut self.inner, tree, renderer, &tight)
    }

    fn update(
        &mut self,
        tree: &mut tree::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        Widget::<Message, Theme, Renderer>::update(
            &mut self.inner,
            tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &tree::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        Widget::<Message, Theme, Renderer>::mouse_interaction(
            &self.inner,
            tree,
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &tree::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        Widget::<Message, Theme, Renderer>::draw(
            &self.inner,
            tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
}

impl<'a, Message, Theme, Renderer> From<IntrinsicShader<Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: cosmic::iced::advanced::Renderer,
    shader::Shader<Message, StreamingThumb>: Widget<Message, Theme, Renderer> + 'a,
{
    fn from(widget: IntrinsicShader<Message>) -> Self {
        Element::new(widget)
    }
}
