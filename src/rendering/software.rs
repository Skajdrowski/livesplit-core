//! Provides a software renderer that can be used without a GPU. The renderer is
//! surprisingly fast and can be considered the default rendering backend.

use super::{
    FillShader, FontKind, Scene, SceneManager, SharedOwnership, Transform,
    consts::SHADOW_OFFSET,
    default_text_engine::{Font, Label, TextEngine},
    entity::Entity,
    resource::{self, ResourceAllocator},
};
use crate::{
    layout::LayoutState, platform::prelude::*, rendering::Background, settings,
    settings::ImageCache,
};
use alloc::rc::Rc;
use core::{mem, ops::Deref};
use tiny_skia::{
    BlendMode, Color, FillRule, FilterQuality, GradientStop, LinearGradient, Paint, Path,
    PathBuilder, Pattern, Pixmap, PixmapMut, Point, Rect, Shader, SpreadMode, Stroke,
};
use tiny_skia_path::NormalizedF32;

#[cfg(feature = "image")]
use crate::settings::{BLUR_FACTOR, BackgroundImage};
#[cfg(feature = "image")]
use image::{ImageBuffer, imageops::FilterType};
#[cfg(feature = "image")]
use tiny_skia_path::IntSize;

#[cfg(feature = "image")]
pub use image::{self, RgbaImage};

struct SkiaBuilder(PathBuilder);

type SkiaPath = Option<UnsafeRc<Path>>;
type SkiaImage = UnsafeRc<Image>;
type SkiaFont = Font;
type SkiaLabel = Label<SkiaPath>;

struct Image {
    pixmap: Pixmap,
    aspect_ratio: f32,
}

impl resource::Image for SkiaImage {
    fn aspect_ratio(&self) -> f32 {
        self.aspect_ratio
    }
}

impl resource::PathBuilder for SkiaBuilder {
    type Path = SkiaPath;

    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y)
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y)
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y)
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y)
    }

    fn close(&mut self) {
        self.0.close()
    }

    fn finish(self) -> Self::Path {
        self.0.finish().map(UnsafeRc::new)
    }
}

fn convert_color(&[r, g, b, a]: &[f32; 4]) -> Color {
    Color::from_rgba(r, g, b, a).unwrap()
}

fn convert_transform(transform: &Transform) -> tiny_skia::Transform {
    tiny_skia::Transform::from_row(
        transform.scale_x,
        0.0,
        0.0,
        transform.scale_y,
        transform.x,
        transform.y,
    )
}

struct SkiaAllocator {
    text_engine: TextEngine<SkiaPath>,
}

impl ResourceAllocator for SkiaAllocator {
    type PathBuilder = SkiaBuilder;
    type Path = SkiaPath;
    type Image = SkiaImage;
    type Font = SkiaFont;
    type Label = SkiaLabel;

    fn path_builder(&mut self) -> Self::PathBuilder {
        path_builder()
    }

    fn create_image(&mut self, _data: &[u8]) -> Option<Self::Image> {
        #[cfg(feature = "image")]
        {
            let mut buf = image::load_from_memory(_data).ok()?.to_rgba8();

            // Premultiplication
            for [r, g, b, a] in bytemuck::cast_slice_mut::<u8, [u8; 4]>(&mut buf) {
                // If it's opaque we can skip the entire pixel. However this
                // hurts vectorization, so we want to avoid it if the compiler
                // can vectorize the loop. WASM, PowerPC, and MIPS are
                // unaffected at the moment.
                #[cfg(not(any(target_feature = "avx2", target_feature = "neon")))]
                if *a == 0xFF {
                    continue;
                }
                let a = *a as u16;
                *r = ((*r as u16 * a) / 255) as u8;
                *g = ((*g as u16 * a) / 255) as u8;
                *b = ((*b as u16 * a) / 255) as u8;
            }

            let (width, height) = (buf.width(), buf.height());

            let pixmap = Pixmap::from_vec(buf.into_raw(), IntSize::from_wh(width, height)?)?;

            Some(UnsafeRc::new(Image {
                pixmap,
                aspect_ratio: width as f32 / height as f32,
            }))
        }
        #[cfg(not(feature = "image"))]
        {
            None
        }
    }

    fn create_font(&mut self, font: Option<&settings::Font>, kind: FontKind) -> Self::Font {
        self.text_engine.create_font(font, kind)
    }

    fn create_label(
        &mut self,
        text: &str,
        font: &mut Self::Font,
        max_width: Option<f32>,
    ) -> Self::Label {
        self.text_engine
            .create_label(path_builder, text, font, max_width)
    }

    fn update_label(
        &mut self,
        label: &mut Self::Label,
        text: &str,
        font: &mut Self::Font,
        max_width: Option<f32>,
    ) {
        self.text_engine
            .update_label(path_builder, label, text, font, max_width)
    }
}

fn path_builder() -> SkiaBuilder {
    SkiaBuilder(PathBuilder::new())
}

/// The software renderer allows rendering layouts entirely on the CPU. This is
/// surprisingly fast and can be considered the default renderer. There are two
/// versions of the software renderer. This version of the software renderer
/// does not own the image to render into. This allows the caller to manage
/// their own image buffer.
pub struct BorrowedRenderer {
    allocator: SkiaAllocator,
    scene_manager: SceneManager<SkiaPath, SkiaImage, SkiaFont, SkiaLabel>,
    #[cfg(feature = "image")]
    blurred_background_image: Option<(BackgroundImage<usize>, Pixmap)>,
    background: Pixmap,
    min_y: f32,
    max_y: f32,
}

struct UnsafeRc<T>(Rc<T>);

impl<T: Send + Sync> Deref for UnsafeRc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> UnsafeRc<T> {
    fn new(val: T) -> Self {
        Self(Rc::new(val))
    }
}

impl<T: Send + Sync> SharedOwnership for UnsafeRc<T> {
    fn share(&self) -> Self {
        Self(self.0.share())
    }
}

// Safety: This is safe because the BorrowedSoftwareRenderer and the
// SceneManager never share any of their resources with anyone. For the
// BorrowedSoftwareRenderer this is trivially true as it doesn't share any its
// fields with anyone, you provide the image to render into yourself. For the
// SceneManager it's harder to prove. However as long as the trait bounds for
// the ResourceAllocator's Image and Path types do not require Sync or Send,
// then the SceneManager simply can't share any of the allocated resources
// across any threads at all. FIXME: However the Send bound may not actually
// hold, because Rc may not actually be allowed to be sent across threads at
// all, as it may for example use a thread local heap allocator. So deallocating
// from a different thread would be unsound. Upstream issue:
// https://github.com/rust-lang/rust/issues/122452
unsafe impl<T: Send + Sync> Send for UnsafeRc<T> {}

// Safety: The BorrowedSoftwareRenderer only has a render method which requires
// exclusive access. The SceneManager could still mess it up. But as long as the
// ResourceAllocator's Image and Path types do not require Sync or Send, it
// can't make use of the Sync bound in any dangerous way anyway.
unsafe impl<T: Send + Sync> Sync for UnsafeRc<T> {}

impl Default for BorrowedRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl BorrowedRenderer {
    /// Creates a new software renderer.
    pub fn new() -> Self {
        let mut allocator = SkiaAllocator {
            text_engine: TextEngine::new(),
        };
        let scene_manager = SceneManager::new(&mut allocator);
        Self {
            allocator,
            scene_manager,
            #[cfg(feature = "image")]
            blurred_background_image: None,
            background: Pixmap::new(1, 1).unwrap(),
            min_y: f32::INFINITY,
            max_y: f32::NEG_INFINITY,
        }
    }

    /// Renders the layout state provided into the image buffer provided. The
    /// image has to be an array of `RGBA8` encoded pixels (red, green, blue,
    /// alpha with each channel being an u8). Some frameworks may over allocate
    /// an image's dimensions. So an image with dimensions `100x50` may be over
    /// allocated as `128x64`. In that case you provide the real dimensions of
    /// `100x50` as the width and height, but a stride of `128` pixels as that
    /// correlates with the real width of the underlying buffer. It may detect
    /// that the layout got resized. In that case it returns the new ideal size.
    /// This is just a hint and can be ignored entirely. The image is always
    /// rendered with the resolution provided. By default the renderer will try
    /// not to redraw parts of the image that haven't changed. You can force a
    /// redraw in case the image provided or its contents have changed.
    pub fn render(
        &mut self,
        state: &LayoutState,
        image_cache: &ImageCache,
        image: &mut [u8],
        [width, height]: [u32; 2],
        stride: u32,
        force_redraw: bool,
    ) -> Option<[f32; 2]> {
        let mut frame_buffer = PixmapMut::from_bytes(image, stride, height).unwrap();

        if stride != self.background.width() || height != self.background.height() {
            self.background = Pixmap::new(stride, height).unwrap();
        }

        let new_resolution = self.scene_manager.update_scene(
            &mut self.allocator,
            [width as _, height as _],
            state,
            image_cache,
        );

        let scene = self.scene_manager.scene();
        let rectangle = scene.rectangle();
        let rectangle = rectangle.as_deref().unwrap();

        let bottom_layer_changed = scene.bottom_layer_changed();

        let mut background = self.background.as_mut();

        if bottom_layer_changed {
            fill_background(
                scene,
                #[cfg(feature = "image")]
                &mut self.blurred_background_image,
                &mut background,
                width,
                height,
                rectangle,
            );
            render_layer(&mut background, scene.bottom_layer(), rectangle);
        }

        let top_layer = scene.top_layer();

        let [min_y, max_y] = calculate_bounds(top_layer);
        let min_y = mem::replace(&mut self.min_y, min_y).min(min_y);
        let max_y = mem::replace(&mut self.max_y, max_y).max(max_y);

        if force_redraw || bottom_layer_changed {
            frame_buffer
                .data_mut()
                .copy_from_slice(background.data_mut());
        } else if min_y <= max_y {
            let stride = 4 * stride as usize;
            let min_y = stride * (min_y - 1.0) as usize;
            let max_y = stride * ((max_y + 2.0) as usize).min(height as usize);

            frame_buffer.data_mut()[min_y..max_y]
                .copy_from_slice(&background.data_mut()[min_y..max_y]);
        }

        render_layer(&mut frame_buffer, top_layer, rectangle);

        new_resolution
    }
}

/// The software renderer allows rendering layouts entirely on the CPU. This is
/// surprisingly fast and can be considered the default renderer. There are two
/// versions of the software renderer. This version of the software renderer
/// owns the image it renders into.
pub struct Renderer {
    renderer: BorrowedRenderer,
    frame_buffer: Pixmap,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    /// Creates a new software renderer.
    pub fn new() -> Self {
        Self {
            renderer: BorrowedRenderer::new(),
            frame_buffer: Pixmap::new(1, 1).unwrap(),
        }
    }

    /// Renders the layout state provided with the chosen resolution. It may
    /// detect that the layout got resized. In that case it returns the new
    /// ideal size. This is just a hint and can be ignored entirely. The image
    /// is always rendered with the resolution provided.
    pub fn render(
        &mut self,
        state: &LayoutState,
        image_cache: &ImageCache,
        [width, height]: [u32; 2],
    ) -> Option<[f32; 2]> {
        if width != self.frame_buffer.width() || height != self.frame_buffer.height() {
            self.frame_buffer = Pixmap::new(width, height).unwrap();
        }

        self.renderer.render(
            state,
            image_cache,
            self.frame_buffer.data_mut(),
            [width, height],
            width,
            false,
        )
    }

    /// Accesses the image as a byte slice of RGBA8 encoded pixels (red, green,
    /// blue, alpha with each channel being an u8).
    pub fn image_data(&self) -> &[u8] {
        self.frame_buffer.data()
    }

    /// Turns the whole renderer into the underlying image buffer of RGBA8
    /// encoded pixels (red, green, blue, alpha with each channel being an u8).
    pub fn into_image_data(self) -> Vec<u8> {
        self.frame_buffer.take()
    }

    /// Accesses the image.
    #[cfg(feature = "image")]
    pub fn image(&self) -> ImageBuffer<image::Rgba<u8>, &[u8]> {
        ImageBuffer::from_raw(
            self.frame_buffer.width(),
            self.frame_buffer.height(),
            self.frame_buffer.data(),
        )
        .unwrap()
    }

    /// Turns the whole renderer into the underlying image.
    #[cfg(feature = "image")]
    pub fn into_image(self) -> RgbaImage {
        RgbaImage::from_raw(
            self.frame_buffer.width(),
            self.frame_buffer.height(),
            self.frame_buffer.take(),
        )
        .unwrap()
    }
}

fn render_layer(
    canvas: &mut PixmapMut<'_>,
    layer: &[Entity<SkiaPath, SkiaImage, SkiaLabel>],
    rectangle: &Path,
) {
    for entity in layer {
        match entity {
            Entity::FillPath(path, shader, transform) => {
                if let Some(path) = path.as_deref() {
                    let paint = convert_shader(
                        shader,
                        path,
                        |path| {
                            let bounds = path.bounds();
                            [bounds.top(), bounds.bottom()]
                        },
                        |path| {
                            let bounds = path.bounds();
                            [bounds.left(), bounds.right()]
                        },
                    );

                    canvas.fill_path(
                        path,
                        &paint,
                        FillRule::Winding,
                        convert_transform(transform),
                        None,
                    );
                }
            }
            Entity::StrokePath(path, stroke_width, color, transform) => {
                if let Some(path) = path.as_deref() {
                    canvas.stroke_path(
                        path,
                        &Paint {
                            shader: Shader::SolidColor(convert_color(color)),
                            anti_alias: true,
                            ..Default::default()
                        },
                        &Stroke {
                            width: *stroke_width,
                            ..Default::default()
                        },
                        convert_transform(transform),
                        None,
                    );
                }
            }
            Entity::Image(image, transform) => {
                canvas.fill_path(
                    rectangle,
                    &Paint {
                        shader: Pattern::new(
                            image.pixmap.as_ref(),
                            SpreadMode::Pad,
                            FilterQuality::Bilinear,
                            1.0,
                            tiny_skia::Transform::from_scale(
                                1.0 / image.pixmap.width() as f32,
                                1.0 / image.pixmap.height() as f32,
                            ),
                        ),
                        anti_alias: true,
                        ..Default::default()
                    },
                    FillRule::Winding,
                    convert_transform(transform),
                    None,
                );
            }
            Entity::Label(label, shader, text_shadow, transform) => {
                let label = &*label.read().unwrap();

                let paint = convert_shader(
                    shader,
                    label,
                    |label| {
                        let (mut top, mut bottom) = (f32::INFINITY, f32::NEG_INFINITY);
                        for glyph in label.glyphs() {
                            if let Some(path) = &glyph.path {
                                let bounds = path.bounds();
                                top = top.min(bounds.top());
                                bottom = bottom.max(bounds.bottom());
                            }
                        }
                        if bottom < top {
                            [0.0, 0.0]
                        } else {
                            [top, bottom]
                        }
                    },
                    |label| {
                        let (mut left, mut right) = (f32::INFINITY, f32::NEG_INFINITY);
                        for glyph in label.glyphs() {
                            if let Some(path) = &glyph.path {
                                let bounds = path.bounds();
                                left = left.min(bounds.left());
                                right = right.max(bounds.right());
                            }
                        }
                        if right < left {
                            [0.0, 0.0]
                        } else {
                            [left, right]
                        }
                    },
                );

                if let Some(text_shadow) = text_shadow {
                    let mut color = convert_color(text_shadow);
                    let alpha = match shader {
                        FillShader::SolidColor([.., a]) => *a,
                        FillShader::VerticalGradient([.., a1], [.., a2])
                        | FillShader::HorizontalGradient([.., a1], [.., a2]) => 0.5 * (a1 + a2),
                    };
                    color.apply_opacity(alpha);
                    let transform = transform.pre_translate(SHADOW_OFFSET, SHADOW_OFFSET);

                    for glyph in label.glyphs() {
                        if let Some(path) = &glyph.path {
                            let transform = transform
                                .pre_translate(glyph.x, glyph.y)
                                .pre_scale(glyph.scale, glyph.scale);

                            canvas.fill_path(
                                path,
                                &Paint {
                                    shader: Shader::SolidColor(color),
                                    ..paint
                                },
                                FillRule::Winding,
                                convert_transform(&transform),
                                None,
                            );
                        }
                    }
                }

                for glyph in label.glyphs() {
                    if let Some(path) = &glyph.path {
                        let transform = transform
                            .pre_translate(glyph.x, glyph.y)
                            .pre_scale(glyph.scale, glyph.scale);

                        let paint = if let Some(color) = &glyph.color {
                            &Paint {
                                shader: Shader::SolidColor(convert_color(color)),
                                ..paint
                            }
                        } else {
                            &paint
                        };

                        canvas.fill_path(
                            path,
                            paint,
                            FillRule::Winding,
                            convert_transform(&transform),
                            None,
                        );
                    }
                }
            }
        }
    }
}

fn convert_shader<T>(
    shader: &FillShader,
    has_bounds: &T,
    calculate_top_bottom: impl FnOnce(&T) -> [f32; 2],
    calculate_left_right: impl FnOnce(&T) -> [f32; 2],
) -> Paint<'static> {
    let shader = match shader {
        FillShader::SolidColor(col) => Shader::SolidColor(convert_color(col)),
        FillShader::VerticalGradient(top, bottom) => {
            let [bound_top, bound_bottom] = calculate_top_bottom(has_bounds);
            LinearGradient::new(
                Point::from_xy(0.0, bound_top),
                Point::from_xy(0.0, bound_bottom),
                vec![
                    GradientStop::new(0.0, convert_color(top)),
                    GradientStop::new(1.0, convert_color(bottom)),
                ],
                SpreadMode::Pad,
                tiny_skia::Transform::identity(),
            )
            .unwrap()
        }
        FillShader::HorizontalGradient(left, right) => {
            let [bound_left, bound_right] = calculate_left_right(has_bounds);
            LinearGradient::new(
                Point::from_xy(bound_left, 0.0),
                Point::from_xy(bound_right, 0.0),
                vec![
                    GradientStop::new(0.0, convert_color(left)),
                    GradientStop::new(1.0, convert_color(right)),
                ],
                SpreadMode::Pad,
                tiny_skia::Transform::identity(),
            )
            .unwrap()
        }
    };

    Paint {
        shader,
        anti_alias: true,
        ..Default::default()
    }
}

fn fill_background(
    scene: &Scene<SkiaPath, SkiaImage, SkiaLabel>,
    #[cfg(feature = "image")] blurred_background_image: &mut Option<(
        BackgroundImage<usize>,
        Pixmap,
    )>,
    background_layer: &mut PixmapMut<'_>,
    width: u32,
    height: u32,
    rectangle: &Path,
) {
    #[cfg(feature = "image")]
    update_blurred_background_image(scene, blurred_background_image);

    match scene.background() {
        Some(background) => match background {
            Background::Shader(shader) => match shader {
                FillShader::SolidColor(color) => {
                    background_layer
                        .pixels_mut()
                        .fill(convert_color(color).premultiply().to_color_u8());
                }
                FillShader::VerticalGradient(top, bottom) => {
                    background_layer.fill_rect(
                        Rect::from_xywh(0.0, 0.0, width as _, height as _).unwrap(),
                        &Paint {
                            shader: LinearGradient::new(
                                Point::from_xy(0.0, 0.0),
                                Point::from_xy(0.0, height as _),
                                vec![
                                    GradientStop::new(0.0, convert_color(top)),
                                    GradientStop::new(1.0, convert_color(bottom)),
                                ],
                                SpreadMode::Pad,
                                tiny_skia::Transform::identity(),
                            )
                            .unwrap(),
                            blend_mode: BlendMode::Source,
                            ..Default::default()
                        },
                        tiny_skia::Transform::identity(),
                        None,
                    );
                }
                FillShader::HorizontalGradient(left, right) => {
                    background_layer.fill_rect(
                        Rect::from_xywh(0.0, 0.0, width as _, height as _).unwrap(),
                        &Paint {
                            shader: LinearGradient::new(
                                Point::from_xy(0.0, 0.0),
                                Point::from_xy(width as _, 0.0),
                                vec![
                                    GradientStop::new(0.0, convert_color(left)),
                                    GradientStop::new(1.0, convert_color(right)),
                                ],
                                SpreadMode::Pad,
                                tiny_skia::Transform::identity(),
                            )
                            .unwrap(),
                            blend_mode: BlendMode::Source,
                            ..Default::default()
                        },
                        tiny_skia::Transform::identity(),
                        None,
                    );
                }
            },
            Background::Image(image, transform) => {
                #[cfg(feature = "image")]
                let pixmap = if image.blur != 0.0 {
                    blurred_background_image
                        .as_ref()
                        .map(|(_, pixmap)| pixmap)
                        .unwrap()
                } else {
                    &image.image.pixmap
                };
                #[cfg(not(feature = "image"))]
                let pixmap = &image.image.pixmap;

                let transform = convert_transform(transform);
                background_layer.fill_path(
                    rectangle,
                    &Paint {
                        shader: Pattern::new(
                            pixmap.as_ref(),
                            SpreadMode::Pad,
                            FilterQuality::Bilinear,
                            image.opacity,
                            tiny_skia::Transform::from_scale(
                                1.0 / pixmap.width() as f32,
                                1.0 / pixmap.height() as f32,
                            ),
                        ),
                        anti_alias: true,
                        blend_mode: BlendMode::Source,
                        ..Default::default()
                    },
                    FillRule::Winding,
                    transform,
                    None,
                );

                if image.brightness != 1.0 {
                    let brightness = NormalizedF32::new_clamped(image.brightness).get();
                    let color = Color::from_rgba(brightness, brightness, brightness, 1.0).unwrap();
                    background_layer.fill_path(
                        rectangle,
                        &Paint {
                            shader: Shader::SolidColor(color),
                            anti_alias: true,
                            blend_mode: BlendMode::Modulate,
                            ..Default::default()
                        },
                        FillRule::Winding,
                        transform,
                        None,
                    );
                }
            }
        },
        None => background_layer.data_mut().fill(0),
    }
}

#[cfg(feature = "image")]
fn update_blurred_background_image(
    scene: &Scene<SkiaPath, SkiaImage, SkiaLabel>,
    blurred_background_image: &mut Option<(BackgroundImage<usize>, Pixmap)>,
) {
    match scene.background() {
        Some(Background::Image(image, _)) if image.blur != 0.0 => {
            let current_key = image.map(image.image.id);
            if !blurred_background_image
                .as_ref()
                .is_some_and(|(key, _)| &current_key == key)
            {
                let original_image = ImageBuffer::<image::Rgba<u8>, _>::from_raw(
                    image.image.pixmap.width(),
                    image.image.pixmap.height(),
                    image.image.pixmap.data(),
                )
                .unwrap();

                // Formula to calculate the sigma as specified
                let dim = original_image.width().max(original_image.height()) as f32;
                let sigma = BLUR_FACTOR * image.blur * dim;

                // For large blurs the calculation is actually very expensive,
                // but we can get around that because large blurs don't require
                // high resolutions in the first place. So we simply scale down
                // the image based on the sigma to a smaller size and then blur
                // the image. For the scaled down image we always use a sigma of
                // 2.0, so scaling the image by 2.0 / sigma should resulting in
                // the same amount of blur. Of course we never want to scale the
                // image up, so in case the scale factor would end up in >= 1x,
                // we simply don't do any scaling and keep the original sigma.
                const SIGMA_WHEN_SCALED: f32 = 2.0;
                let scale = SIGMA_WHEN_SCALED / sigma;

                let scaled;
                let (image, sigma) = if scale < 1.0 {
                    // The image needs to at least be 1x1, because tiny-skia
                    // doesn't allow images to be smaller than that. A triangle
                    // filter is probably fine, the blur will hide most scaling
                    // artifacts anyway.
                    scaled = image::imageops::resize(
                        &original_image,
                        ((scale * original_image.width() as f32) as u32).max(1),
                        ((scale * original_image.height() as f32) as u32).max(1),
                        FilterType::Triangle,
                    );
                    (
                        ImageBuffer::<image::Rgba<u8>, _>::from_raw(
                            scaled.width(),
                            scaled.height(),
                            &*scaled,
                        )
                        .unwrap(),
                        SIGMA_WHEN_SCALED,
                    )
                } else {
                    (original_image, sigma)
                };

                let image_buffer = image::imageops::blur(&image, sigma);
                let size = IntSize::from_wh(image_buffer.width(), image_buffer.height()).unwrap();
                let pixmap = Pixmap::from_vec(image_buffer.into_raw(), size).unwrap();
                *blurred_background_image = Some((current_key, pixmap));
            }
        }
        _ => {
            *blurred_background_image = None;
        }
    }
}

fn calculate_bounds(layer: &[Entity<SkiaPath, SkiaImage, SkiaLabel>]) -> [f32; 2] {
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for entity in layer.iter() {
        match entity {
            Entity::FillPath(path, _, transform) => {
                if let Some(path) = &**path {
                    let bounds = path.bounds();
                    for y in [bounds.top(), bounds.bottom()] {
                        let transformed_y = transform.transform_y(y);
                        min_y = min_y.min(transformed_y);
                        max_y = max_y.max(transformed_y);
                    }
                }
            }
            Entity::StrokePath(path, radius, _, transform) => {
                if let Some(path) = &**path {
                    let radius = transform.scale_y * radius;
                    let bounds = path.bounds();
                    for y in [bounds.top(), bounds.bottom()] {
                        let transformed_y = transform.transform_y(y);
                        min_y = min_y.min(transformed_y - radius);
                        max_y = max_y.max(transformed_y + radius);
                    }
                }
            }
            Entity::Image(_, transform) => {
                for y in [0.0, 1.0] {
                    let transformed_y = transform.transform_y(y);
                    min_y = min_y.min(transformed_y);
                    max_y = max_y.max(transformed_y);
                }
            }
            Entity::Label(label, _, text_shadow, transform) => {
                let label = &*label.read().unwrap();

                if text_shadow.is_some() {
                    let transform = transform.pre_translate(SHADOW_OFFSET, SHADOW_OFFSET);

                    for glyph in label.glyphs() {
                        if let Some(path) = &glyph.path {
                            let transform = transform
                                .pre_translate(glyph.x, glyph.y)
                                .pre_scale(glyph.scale, glyph.scale);

                            let bounds = path.bounds();
                            for y in [bounds.top(), bounds.bottom()] {
                                let transformed_y = transform.transform_y(y);
                                min_y = min_y.min(transformed_y);
                                max_y = max_y.max(transformed_y);
                            }
                        }
                    }
                }

                for glyph in label.glyphs() {
                    if let Some(path) = &glyph.path {
                        let transform = transform
                            .pre_translate(glyph.x, glyph.y)
                            .pre_scale(glyph.scale, glyph.scale);

                        let bounds = path.bounds();
                        for y in [bounds.top(), bounds.bottom()] {
                            let transformed_y = transform.transform_y(y);
                            min_y = min_y.min(transformed_y);
                            max_y = max_y.max(transformed_y);
                        }
                    }
                }
            }
        }
    }
    [min_y, max_y]
}
