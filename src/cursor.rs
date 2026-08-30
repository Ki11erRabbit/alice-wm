//! Software cursor rendering.
//!
//! Neither backend gets a cursor for free: winit runs the compositor inside a
//! plain window with no host cursor guarantees, and the udev/DRM backend has
//! no cursor at all unless the compositor draws one itself. So we draw a
//! small arrow ourselves and composite it as a normal render element on top
//! of everything else, positioned at the pointer's current location. If a
//! client sets its own cursor surface (e.g. a text-edit I-beam) we render
//! that surface instead, honoring its declared hotspot.

use smithay::{
    backend::{allocator::Fourcc, renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{
            AsRenderElements, Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::WaylandSurfaceRenderElement,
        },
    }},
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    utils::{IsAlive, Logical, Physical, Point, Scale, Transform},
    wayland::compositor,
};
use std::sync::Mutex;

const CURSOR_SIZE: i32 = 24;

/// Classic arrow-pointer outline, tip pinned at the origin so it lines up
/// with the hotspot we report (0, 0). Coordinates are in a ~17-unit box;
/// `draw_default_cursor` scales it to fit `CURSOR_SIZE`.
const ARROW_POINTS: &[(f32, f32)] = &[
    (0.0, 0.0),
    (0.0, 16.0),
    (4.0, 12.0),
    (6.0, 17.0),
    (8.0, 16.0),
    (6.0, 11.0),
    (11.0, 11.0),
];

fn point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Draws a simple black arrow with a white outline into a fresh RGBA memory
/// buffer. Self-contained on purpose: no bundled image asset and no
/// dependency on an XCursor theme being installed on the system.
fn draw_default_cursor() -> MemoryRenderBuffer {
    let scale = 1.3_f32;
    let outer: Vec<(f32, f32)> = ARROW_POINTS.iter().map(|(x, y)| (x * scale, y * scale)).collect();

    let mut mem = vec![0u8; (CURSOR_SIZE * CURSOR_SIZE * 4) as usize];
    for y in 0..CURSOR_SIZE {
        for x in 0..CURSOR_SIZE {
            let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Fourcc::Argb8888 uploads as GL_BGRA/UNSIGNED_BYTE, i.e. the
            // in-memory byte order is [B, G, R, A] per pixel.
            let color: [u8; 4] = if point_in_polygon(cx, cy, ARROW_POINTS) {
                [0, 0, 0, 255] // black fill
            } else if point_in_polygon(cx, cy, &outer) {
                [255, 255, 255, 255] // white outline
            } else {
                [0, 0, 0, 0] // transparent
            };
            let idx = ((y * CURSOR_SIZE + x) * 4) as usize;
            mem[idx..idx + 4].copy_from_slice(&color);
        }
    }

    MemoryRenderBuffer::from_slice(
        &mem,
        Fourcc::Argb8888,
        (CURSOR_SIZE, CURSOR_SIZE),
        1,
        Transform::Normal,
        None,
    )
}

/// The hotspot (in surface-local logical coordinates) that should sit
/// exactly on the pointer's location.
pub fn cursor_hotspot(status: &CursorImageStatus) -> Point<i32, Logical> {
    if let CursorImageStatus::Surface(surface) = status {
        compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<Mutex<CursorImageAttributes>>()
                .map(|attrs| attrs.lock().unwrap().hotspot)
                .unwrap_or_default()
        })
    } else {
        (0, 0).into()
    }
}

/// If the client that owns the current cursor surface has disconnected,
/// fall back to drawing the default arrow again.
pub fn reset_cursor_if_dead(status: &mut CursorImageStatus) {
    if let CursorImageStatus::Surface(surface) = status {
        if !surface.alive() {
            *status = CursorImageStatus::default_named();
        }
    }
}

/// Per-seat cursor state: caches the drawn default-arrow buffer (drawing it
/// once is enough, it never changes) and the client-requested status.
pub struct PointerElement {
    buffer: MemoryRenderBuffer,
    status: CursorImageStatus,
}

impl Default for PointerElement {
    fn default() -> Self {
        Self {
            buffer: draw_default_cursor(),
            status: CursorImageStatus::default_named(),
        }
    }
}

impl PointerElement {
    pub fn set_status(&mut self, status: CursorImageStatus) {
        self.status = status;
    }
}

smithay::backend::renderer::element::render_elements! {
    pub PointerRenderElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Memory=MemoryRenderBufferRenderElement<R>,
}

impl<R: Renderer> std::fmt::Debug for PointerRenderElement<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Surface(arg0) => f.debug_tuple("Surface").field(arg0).finish(),
            Self::Memory(arg0) => f.debug_tuple("Memory").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

impl<T: Texture + Clone + Send + 'static, R> AsRenderElements<R> for PointerElement
where
    R: Renderer<TextureId = T> + ImportAll + ImportMem,
{
    type RenderElement = PointerRenderElement<R>;

    fn render_elements<E>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<E>
    where
        E: From<PointerRenderElement<R>>,
    {
        match &self.status {
            CursorImageStatus::Hidden => vec![],
            CursorImageStatus::Named(_) => {
                vec![
                    E::from(PointerRenderElement::<R>::from(
                        MemoryRenderBufferRenderElement::from_buffer(
                            renderer,
                            location.to_f64(),
                            &self.buffer,
                            None,
                            None,
                            None,
                            Kind::Cursor,
                        )
                        .expect("failed to import cursor buffer"),
                    )),
                ]
            }
            CursorImageStatus::Surface(surface) => {
                let elements: Vec<PointerRenderElement<R>> =
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        renderer,
                        surface,
                        location,
                        scale,
                        alpha,
                        Kind::Cursor,
                    );
                elements.into_iter().map(E::from).collect()
            }
        }
    }
}

/// Convenience used by both backends: given the pointer's location and
/// hotspot in a space already offset to the output's origin, produce the
/// render elements for the cursor at the correct physical position.
pub fn cursor_render_elements<T, R, E>(
    pointer_element: &mut PointerElement,
    status: &CursorImageStatus,
    renderer: &mut R,
    cursor_pos_in_output: Point<f64, Logical>,
    scale: Scale<f64>,
) -> Vec<E>
where
    T: Texture + Clone + Send + 'static,
    R: Renderer<TextureId = T> + ImportAll + ImportMem,
    E: From<PointerRenderElement<R>>,
{
    pointer_element.set_status(status.clone());
    let hotspot = cursor_hotspot(status);
    let location = (cursor_pos_in_output - hotspot.to_f64())
        .to_physical(scale)
        .to_i32_round();
    AsRenderElements::<R>::render_elements(pointer_element, renderer, location, scale, 1.0)
}
