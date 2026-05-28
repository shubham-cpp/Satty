use anyhow::Result;
use glow::HasContext;
use std::{
    cell::{RefCell, RefMut},
    collections::HashSet,
    num::NonZeroU32,
    path::PathBuf,
    rc::Rc,
};

use femtovg::{
    Canvas, FontId, ImageFlags, ImageId, ImageSource, Paint, Path, PixelFormat, Transform2D,
    imgref::{Img, ImgVec},
    renderer,
    rgb::{RGB, RGBA, RGBA8},
};
use fontconfig::Fontconfig;
use gtk::{glib, prelude::*, subclass::prelude::*};
use relm4::gtk::gdk_pixbuf::Pixbuf;
use relm4::{Sender, gtk};
use resource::resource;

use crate::{
    APP_CONFIG,
    configuration::Action,
    math::{Vec2D, rect_ensure_in_bounds, rect_round},
    sketch_board::SketchBoardInput,
    tools::{
        CropTool, Drawable, StyleChange, Tool, Tools,
        edit::{self, EditHandle},
    },
};

use super::{font_stack, set_font_stack};

const TRANSPARENCY_SQUARE_SIZE: usize = 64;

#[derive(Default)]
pub struct FemtoVGArea {
    canvas: RefCell<Option<femtovg::Canvas<femtovg::renderer::OpenGl>>>,
    font: RefCell<Option<FontId>>,
    inner: RefCell<Option<FemtoVgAreaMut>>,
    request_render: RefCell<Option<Vec<Action>>>,
    sender: RefCell<Option<Sender<SketchBoardInput>>>,
}

pub struct FemtoVgAreaMut {
    background_image: Pixbuf,
    background_image_id: Option<femtovg::ImageId>,
    transparent_background_id: Option<femtovg::ImageId>,
    active_tool: Rc<RefCell<dyn Tool>>,
    crop_tool: Rc<RefCell<CropTool>>,
    scale_factor: f32,
    offset: Vec2D,
    drawables: Vec<Box<dyn Drawable>>,
    history: Vec<HistoryAction>,
    redo_history: Vec<HistoryAction>,
    selection_focus: SelectionFocusState,
    transform_session: Option<TransformSession>,
    zoom_scale: f32,
    last_scale: f32,
    pointer_offset: Vec2D,
    last_offset: Vec2D,
    drag_offset: Vec2D,
    is_drag: bool,
    is_reset: bool,
}

enum HistoryAction {
    Add {
        index: usize,
        drawable: Option<Box<dyn Drawable>>,
    },
    Modify {
        index: usize,
        before: Box<dyn Drawable>,
        after: Box<dyn Drawable>,
    },
    Reset {
        drawables: Vec<Box<dyn Drawable>>,
    },
}

enum ObjectEditAction {
    Move,
    Resize(EditHandle),
}

#[derive(Default)]
struct SelectionFocusState {
    selected: Option<usize>,
    focused: Option<usize>,
}

impl SelectionFocusState {
    fn clear_selection(&mut self) {
        self.selected = None;
    }

    fn clear(&mut self) {
        self.selected = None;
        self.focused = None;
    }

    fn set_selected(&mut self, index: Option<usize>) {
        self.selected = index;
        self.focused = index;
    }

    fn focus_without_selection(&mut self, index: usize) {
        self.selected = None;
        self.focused = Some(index);
    }

    fn target(&self) -> Option<usize> {
        self.selected.or(self.focused)
    }
}

#[derive(Clone, Copy)]
enum HitPart {
    Body,
    Handle(EditHandle),
}

#[derive(Clone, Copy)]
struct HitResult {
    index: usize,
    part: HitPart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerCursor {
    Move,
    ResizeNwSe,
    ResizeNeSw,
    ResizeNs,
    ResizeEw,
    ResizeAll,
}

impl PointerCursor {
    pub fn name(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::ResizeNwSe => "nwse-resize",
            Self::ResizeNeSw => "nesw-resize",
            Self::ResizeNs => "ns-resize",
            Self::ResizeEw => "ew-resize",
            Self::ResizeAll => "all-resize",
        }
    }
}

impl HitResult {
    fn action(self) -> ObjectEditAction {
        match self.part {
            HitPart::Body => ObjectEditAction::Move,
            HitPart::Handle(handle) => ObjectEditAction::Resize(handle),
        }
    }
}

struct TransformSession {
    index: usize,
    action: ObjectEditAction,
    before: Box<dyn Drawable>,
}

fn cursor_for_handle(handle: EditHandle) -> PointerCursor {
    match handle {
        EditHandle::TopLeft | EditHandle::BottomRight => PointerCursor::ResizeNwSe,
        EditHandle::TopRight | EditHandle::BottomLeft => PointerCursor::ResizeNeSw,
        EditHandle::Top | EditHandle::Bottom => PointerCursor::ResizeNs,
        EditHandle::Right | EditHandle::Left => PointerCursor::ResizeEw,
        EditHandle::Start | EditHandle::End => PointerCursor::ResizeAll,
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FemtoVGArea {
    const NAME: &'static str = "FemtoVGArea";
    type Type = super::FemtoVGArea;
    type ParentType = gtk::GLArea;
}

impl ObjectImpl for FemtoVGArea {
    fn constructed(&self) {
        self.parent_constructed();
        let area = self.obj();
        area.set_has_stencil_buffer(true);
        area.queue_render();
    }
}

impl WidgetImpl for FemtoVGArea {
    fn realize(&self) {
        self.parent_realize();
    }
    fn unrealize(&self) {
        self.obj().make_current();
        self.canvas.borrow_mut().take();
        self.parent_unrealize();
    }
}

impl GLAreaImpl for FemtoVGArea {
    fn resize(&self, width: i32, height: i32) {
        self.ensure_canvas();

        let mut bc = self.canvas.borrow_mut();
        let canvas = bc.as_mut().unwrap(); // this unwrap is safe as long as we call "ensure_canvas" before

        let w = canvas.width();
        let h = canvas.height();

        canvas.set_size(
            if width == 0 { w } else { width as u32 },
            if height == 0 { h } else { height as u32 },
            self.obj().scale_factor() as f32,
        );

        // update scale factor
        self.inner()
            .as_mut()
            .expect("Did you call init before using FemtoVgArea?")
            .update_transformation(canvas);
    }
    fn render(&self, _context: &gtk::gdk::GLContext) -> glib::Propagation {
        self.ensure_canvas();

        let mut bc = self.canvas.borrow_mut();
        let canvas = bc.as_mut().unwrap(); // this unwrap is safe as long as we call "ensure_canvas" before
        let font = self.font.borrow().unwrap(); // this unwrap is safe as long as we call "ensure_canvas" before
        let mut actions = self.request_render.borrow_mut();

        // if we got requested to render a frame
        if let Some(a) = actions.take() {
            // render image
            let image = match self
                .inner()
                .as_mut()
                .expect("Did you call init before using FemtoVgArea?")
                .render_native_resolution(canvas, font)
            {
                Ok(t) => t,
                Err(e) => {
                    println!("Error while rendering image: {e}");
                    return glib::Propagation::Stop;
                }
            };

            // send result
            self.sender
                .borrow()
                .as_ref()
                .expect("Did you call init before using FemtoVgArea?")
                .emit(SketchBoardInput::RenderResult(image, a));

            // reset request
            *actions = None;
        }
        if let Err(e) = self
            .inner()
            .as_mut()
            .expect("Did you call init before using FemtoVgArea?")
            .render_framebuffer(canvas, font)
        {
            println!("Error rendering to framebuffer: {e}");
        }
        glib::Propagation::Stop
    }
}
impl FemtoVGArea {
    pub fn init(
        &self,
        sender: Sender<SketchBoardInput>,
        crop_tool: Rc<RefCell<CropTool>>,
        active_tool: Rc<RefCell<dyn Tool>>,
        background_image: Pixbuf,
    ) {
        let initial_scale = APP_CONFIG.read().input_scale().unwrap_or(0.0);
        self.inner().replace(FemtoVgAreaMut {
            background_image,
            background_image_id: None,
            transparent_background_id: None,
            active_tool,
            crop_tool,
            scale_factor: 1.0,
            offset: Vec2D::zero(),
            drawables: Vec::new(),
            history: Vec::new(),
            redo_history: Vec::new(),
            selection_focus: SelectionFocusState::default(),
            transform_session: None,
            zoom_scale: initial_scale,
            pointer_offset: Vec2D::zero(),
            last_offset: Vec2D::zero(),
            drag_offset: Vec2D::zero(),
            last_scale: initial_scale,
            is_drag: false,
            is_reset: false,
        });
        self.sender.borrow_mut().replace(sender);
    }
    fn ensure_canvas(&self) {
        if self.canvas.borrow().is_none() {
            let c = self
                .setup_canvas()
                .expect("Cannot setup renderer and canvas");
            self.canvas.borrow_mut().replace(c);
        }

        if self.font.borrow().is_none()
            && let Some(first) = font_stack().first()
        {
            self.font.borrow_mut().replace(*first);
        }
    }

    fn build_text_context(&self) -> Result<(femtovg::TextContext, Vec<FontId>)> {
        let text_context = femtovg::TextContext::default();
        let mut loaded_fonts = Vec::new();
        let mut loaded_paths = HashSet::<(PathBuf, u32)>::new();

        let app_config = APP_CONFIG.read();
        let fontconfig = Fontconfig::new();

        let mut load_font = |family: &str, style: Option<&str>| -> Result<FontId> {
            let font = fontconfig
                .as_ref()
                .and_then(|fc| fc.find(family, style))
                .ok_or_else(|| anyhow::anyhow!("Font family '{}' not found", family))?;

            let face_index = font.index.unwrap_or(0).max(0) as u32;

            if !loaded_paths.insert((font.path.clone(), face_index)) {
                return Err(anyhow::anyhow!("Font '{}' already loaded", family));
            }
            let data = std::fs::read(&font.path)
                .map_err(|e| anyhow::anyhow!("Failed to read font file: {}", e))?;

            text_context
                .add_shared_font_with_index(data, face_index)
                .map_err(|e| anyhow::anyhow!("Failed to load font: {}", e))
        };

        match load_font(
            app_config.font().family().unwrap_or(""),
            app_config.font().style(),
        ) {
            Ok(id) => {
                loaded_fonts.push(id);
            }
            Err(e) => {
                eprintln!("Primary font: {}", e);
            }
        }

        if loaded_fonts.is_empty() {
            let fallback = text_context
                .add_font_mem(&resource!("src/assets/Roboto-Regular.ttf"))
                .expect("Cannot add font");
            loaded_fonts.push(fallback);
        }

        for family in app_config.font().fallback() {
            match load_font(family, None) {
                Ok(id) => {
                    loaded_fonts.push(id);
                }
                Err(e) => {
                    eprintln!("Fallback font: {}", e);
                }
            }
        }

        Ok((text_context, loaded_fonts))
    }

    fn setup_canvas(&self) -> Result<femtovg::Canvas<femtovg::renderer::OpenGl>> {
        let widget = self.obj();
        widget.attach_buffers();

        static LOAD_FN: fn(&str) -> *const std::ffi::c_void =
            |s| epoxy::get_proc_addr(s) as *const _;
        // SAFETY: Need to get the framebuffer id that gtk expects us to draw into, so
        // femtovg knows which framebuffer to bind. This is safe as long as we
        // call attach_buffers beforehand. Also unbind it here just in case,
        // since this can be called outside render.
        let (mut renderer, fbo) = unsafe {
            let renderer =
                renderer::OpenGl::new_from_function(LOAD_FN).expect("Cannot create renderer");
            let ctx = glow::Context::from_loader_function(LOAD_FN);
            let id = NonZeroU32::new(ctx.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) as u32)
                .expect("No GTK provided framebuffer binding");
            ctx.bind_framebuffer(glow::FRAMEBUFFER, None);
            (renderer, glow::NativeFramebuffer(id))
        };
        renderer.set_screen_target(Some(fbo));

        let (text_context, loaded_fonts) = self.build_text_context()?;
        let canvas = Canvas::new_with_text_context(renderer, text_context)?;

        set_font_stack(loaded_fonts.clone());
        if let Some(first) = loaded_fonts.first() {
            self.font.borrow_mut().replace(*first);
        }

        Ok(canvas)
    }

    pub fn inner(&self) -> RefMut<'_, Option<FemtoVgAreaMut>> {
        self.inner.borrow_mut()
    }
    pub fn request_render(&self, actions: &[Action]) {
        self.request_render.borrow_mut().replace(actions.into());
        self.obj().queue_render();
    }
    pub fn set_parent_sender(&self, sender: Sender<SketchBoardInput>) {
        self.sender.borrow_mut().replace(sender);
    }
}

impl FemtoVgAreaMut {
    pub fn commit(&mut self, drawable: Box<dyn Drawable>) {
        self.clear_object_selection();
        let index = self.drawables.len();
        self.drawables.push(drawable);
        self.selection_focus.focused = Some(index);
        self.history.push(HistoryAction::Add {
            index,
            drawable: None,
        });
        self.redo_history.clear();
    }

    pub fn undo(&mut self) -> bool {
        self.clear_object_selection();
        self.selection_focus.focused = None;
        let Some(mut action) = self.history.pop() else {
            return false;
        };

        match &mut action {
            HistoryAction::Add { index, drawable } => {
                if *index >= self.drawables.len() {
                    return false;
                }
                let mut removed = self.drawables.remove(*index);
                removed.handle_undo();
                *drawable = Some(removed);
            }
            HistoryAction::Modify { index, before, .. } => {
                if *index >= self.drawables.len() {
                    return false;
                }
                self.drawables[*index] = before.edit_snapshot();
                self.drawables[*index].invalidate_edit_cache();
            }
            HistoryAction::Reset { drawables } => {
                self.drawables = drawables
                    .iter()
                    .map(|drawable| {
                        let mut drawable = drawable.edit_snapshot();
                        drawable.handle_redo();
                        drawable
                    })
                    .collect();
            }
        }

        self.redo_history.push(action);
        true
    }
    pub fn redo(&mut self) -> bool {
        self.clear_object_selection();
        self.selection_focus.focused = None;
        let Some(mut action) = self.redo_history.pop() else {
            return false;
        };

        match &mut action {
            HistoryAction::Add { index, drawable } => {
                let Some(mut drawable) = drawable.take() else {
                    return false;
                };
                drawable.handle_redo();
                if *index >= self.drawables.len() {
                    self.drawables.push(drawable);
                } else {
                    self.drawables.insert(*index, drawable);
                }
            }
            HistoryAction::Modify { index, after, .. } => {
                if *index >= self.drawables.len() {
                    return false;
                }
                self.drawables[*index] = after.edit_snapshot();
                self.drawables[*index].invalidate_edit_cache();
            }
            HistoryAction::Reset { .. } => {
                for drawable in &mut self.drawables {
                    drawable.handle_undo();
                }
                self.drawables.clear();
            }
        }

        self.history.push(action);
        true
    }
    pub fn reset(&mut self) -> bool {
        self.clear_object_selection();
        self.selection_focus.focused = None;
        if self.drawables.is_empty() {
            return false;
        }

        for drawable in &mut self.drawables {
            drawable.handle_undo();
        }
        let removed = std::mem::take(&mut self.drawables);
        let snapshots = removed
            .iter()
            .map(|drawable| drawable.edit_snapshot())
            .collect();
        self.history.push(HistoryAction::Reset {
            drawables: snapshots,
        });
        self.redo_history.clear();
        true
    }

    pub fn set_active_tool(&mut self, active_tool: Rc<RefCell<dyn Tool>>) {
        if active_tool.borrow().get_tool_type() != Tools::Pointer {
            self.clear_object_selection();
            self.selection_focus.focused = None;
        }
        self.active_tool = active_tool;
    }

    pub fn pointer_click(&mut self, pos: Vec2D) -> bool {
        let old_selection = self.selection_focus.selected;
        self.transform_session = None;
        let selection = self.find_drawable_at(pos);
        self.selection_focus.set_selected(selection);
        old_selection != self.selection_focus.selected
    }

    pub fn take_text_edit_at(&mut self, pos: Vec2D) -> Option<(usize, Box<dyn Drawable>)> {
        self.transform_session = None;
        let index = self.find_drawable_at(pos)?;
        if !self.drawables.get(index)?.supports_text_edit() {
            return None;
        }

        self.selection_focus.clear();
        Some((index, self.drawables.remove(index)))
    }

    pub fn restore_drawable(&mut self, index: usize, drawable: Box<dyn Drawable>) {
        self.clear_object_selection();
        if index >= self.drawables.len() {
            self.drawables.push(drawable);
        } else {
            self.drawables.insert(index, drawable);
        }
        self.selection_focus.focused = Some(index);
    }

    pub fn modify_drawable(
        &mut self,
        index: usize,
        before: Box<dyn Drawable>,
        after: Box<dyn Drawable>,
    ) {
        self.clear_object_selection();
        if index >= self.drawables.len() {
            self.drawables.push(after.edit_snapshot());
        } else {
            self.drawables.insert(index, after.edit_snapshot());
        }
        self.selection_focus.focused = Some(index);
        self.history.push(HistoryAction::Modify {
            index,
            before,
            after,
        });
        self.redo_history.clear();
    }

    pub fn pointer_begin_drag(&mut self, pos: Vec2D) -> bool {
        self.transform_session = None;

        if let Some(hit) = self.hit_selected_handle(pos) {
            self.start_transform_session(hit.index, hit.action());
            return true;
        }

        let old_selection = self.selection_focus.selected;
        let selection = self.hit_body(pos).map(|hit| hit.index);
        self.selection_focus.set_selected(selection);
        if let Some(index) = self.selection_focus.selected {
            self.start_transform_session(index, ObjectEditAction::Move);
        }

        old_selection != self.selection_focus.selected || self.transform_session.is_some()
    }

    pub fn pointer_hover_cursor(&self, pos: Vec2D) -> Option<PointerCursor> {
        let tolerance = self.object_hit_tolerance();

        if let Some(index) = self.selection_focus.selected
            && let Some(drawable) = self.drawables.get(index)
            && let Some(handle) = edit::closest_handle(&drawable.edit_handles(), pos, tolerance)
        {
            return Some(cursor_for_handle(handle));
        }

        self.find_drawable_at(pos).map(|_| PointerCursor::Move)
    }

    pub fn temporary_pointer_click(&mut self, pos: Vec2D) -> bool {
        self.transform_session = None;

        if let Some(index) = self.find_drawable_at(pos) {
            self.selection_focus.focus_without_selection(index);
            return true;
        }

        false
    }

    pub fn temporary_pointer_begin_drag(&mut self, pos: Vec2D) -> bool {
        self.transform_session = None;

        if let Some(hit) = self.hit_any_handle(pos) {
            self.selection_focus.focus_without_selection(hit.index);
            self.start_transform_session(hit.index, hit.action());
            return true;
        }

        if let Some(hit) = self.hit_body(pos) {
            self.selection_focus.focus_without_selection(hit.index);
            self.start_transform_session(hit.index, ObjectEditAction::Move);
            return true;
        }

        false
    }

    pub fn temporary_pointer_hover_cursor(&self, pos: Vec2D) -> Option<PointerCursor> {
        if let Some(hit) = self.hit_any_handle(pos)
            && let HitPart::Handle(handle) = hit.part
        {
            return Some(cursor_for_handle(handle));
        }

        self.find_drawable_at(pos).map(|_| PointerCursor::Move)
    }

    pub fn pointer_edit_active(&self) -> bool {
        self.transform_session.is_some()
    }

    pub fn pointer_update_drag(&mut self, delta: Vec2D) -> bool {
        self.apply_transform_session(delta, true)
    }

    pub fn pointer_end_drag(&mut self, delta: Vec2D) -> bool {
        if !self.apply_transform_session(delta, false) {
            self.transform_session = None;
            return false;
        }

        let Some(edit) = self.transform_session.take() else {
            return false;
        };
        let Some(drawable) = self.drawables.get(edit.index) else {
            return false;
        };

        let after = drawable.edit_snapshot();
        self.history.push(HistoryAction::Modify {
            index: edit.index,
            before: edit.before,
            after,
        });
        self.selection_focus.focused = Some(edit.index);
        self.redo_history.clear();
        true
    }

    pub fn apply_style_change_to_target(&mut self, change: StyleChange) -> bool {
        let Some(index) = self.selection_focus.target() else {
            return false;
        };
        if index >= self.drawables.len() {
            self.selection_focus.focused = None;
            return false;
        }

        let before = self.drawables[index].edit_snapshot();
        if !self.drawables[index].apply_style_change(change) {
            return false;
        }
        self.drawables[index].invalidate_edit_cache();
        let after = self.drawables[index].edit_snapshot();
        self.history.push(HistoryAction::Modify {
            index,
            before,
            after,
        });
        self.redo_history.clear();
        self.selection_focus.focused = Some(index);
        true
    }

    fn apply_transform_session(&mut self, delta: Vec2D, preview: bool) -> bool {
        let Some(edit) = &self.transform_session else {
            return false;
        };
        if edit.index >= self.drawables.len() {
            return false;
        }

        self.drawables[edit.index] = edit.before.edit_snapshot();
        if preview {
            self.drawables[edit.index].begin_edit_session();
        }
        let changed = match edit.action {
            ObjectEditAction::Move => self.drawables[edit.index].move_by(delta),
            ObjectEditAction::Resize(handle) => self.drawables[edit.index].resize(handle, delta),
        };
        if changed && preview {
            self.drawables[edit.index].invalidate_edit_cache();
        }
        if changed && !preview {
            self.drawables[edit.index].end_edit_session();
        }
        changed
    }

    fn hit_selected_handle(&self, pos: Vec2D) -> Option<HitResult> {
        let tolerance = self.object_hit_tolerance();
        let index = self.selection_focus.selected?;
        let drawable = self.drawables.get(index)?;
        let handle = edit::closest_handle(&drawable.edit_handles(), pos, tolerance)?;
        Some(HitResult {
            index,
            part: HitPart::Handle(handle),
        })
    }

    fn hit_any_handle(&self, pos: Vec2D) -> Option<HitResult> {
        let tolerance = self.object_hit_tolerance();
        self.drawables
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, drawable)| {
                edit::closest_handle(&drawable.edit_handles(), pos, tolerance).map(|handle| {
                    HitResult {
                        index,
                        part: HitPart::Handle(handle),
                    }
                })
            })
    }

    fn start_transform_session(&mut self, index: usize, action: ObjectEditAction) -> bool {
        let Some(drawable) = self.drawables.get(index) else {
            return false;
        };

        self.transform_session = Some(TransformSession {
            index,
            action,
            before: drawable.edit_snapshot(),
        });
        true
    }

    fn find_drawable_at(&self, pos: Vec2D) -> Option<usize> {
        self.hit_body(pos).map(|hit| hit.index)
    }

    fn hit_body(&self, pos: Vec2D) -> Option<HitResult> {
        let tolerance = self.object_hit_tolerance();
        self.drawables
            .iter()
            .enumerate()
            .rev()
            .find(|(_, drawable)| {
                drawable.edit_bounds().is_some() && drawable.hit_test(pos, tolerance)
            })
            .map(|(index, _)| HitResult {
                index,
                part: HitPart::Body,
            })
    }

    fn object_hit_tolerance(&self) -> f32 {
        10.0 / self.scale_factor.max(0.01)
    }

    fn clear_object_selection(&mut self) {
        self.selection_focus.clear_selection();
        self.transform_session = None;
    }

    pub fn render_native_resolution(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        font: FontId,
    ) -> anyhow::Result<ImgVec<RGBA8>> {
        let bounds = (
            Vec2D::zero(),
            Vec2D::new(
                self.background_image.width() as f32,
                self.background_image.height() as f32,
            ),
        );
        // get offset and size of the area in question
        let (pos, size) = self
            .crop_tool
            .borrow()
            .get_crop()
            .map(|c| c.get_rectangle())
            .map(|rect| rect_ensure_in_bounds(rect, bounds))
            .map(rect_round)
            .filter(|(_, size)| !size.is_zero())
            .unwrap_or(bounds);

        // create render-target
        let image_id = canvas.create_image_empty(
            size.x as usize,
            size.y as usize,
            PixelFormat::Rgba8,
            ImageFlags::empty(),
        )?;
        canvas.set_render_target(femtovg::RenderTarget::Image(image_id));

        // apply offset
        let mut transform = Transform2D::identity();
        transform.translate(-pos.x, -pos.y);
        canvas.reset_transform();
        canvas.set_transform(&transform);

        self.render(
            canvas,
            font,
            false,
            femtovg::Color::rgbaf(0.0, 0.0, 0.0, 0.0),
            false,
        )?;

        // return screenshot
        let result = canvas.screenshot();

        // clean up
        canvas.set_render_target(femtovg::RenderTarget::Screen);
        canvas.delete_image(image_id);

        Ok(result?)
    }

    pub fn render_framebuffer(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        font: FontId,
    ) -> Result<()> {
        canvas.set_render_target(femtovg::RenderTarget::Screen);

        // setup transform to image coordinates
        let mut transform = Transform2D::identity();
        transform.scale(self.scale_factor, self.scale_factor);
        transform.translate(self.offset.x, self.offset.y);

        canvas.reset_transform();
        canvas.set_transform(&transform);

        //TODO: make background color configurable
        self.render(canvas, font, true, femtovg::Color::black(), true)?;

        Ok(())
    }

    fn render(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        font: FontId,
        render_crop: bool,
        outside_bg_color: femtovg::Color,
        onscreen: bool,
    ) -> Result<()> {
        // clear canvas

        canvas.clear_rect(0, 0, canvas.width(), canvas.height(), outside_bg_color);

        // render background
        self.render_background_image(canvas, onscreen)?;

        let bounds = (
            Vec2D::zero(),
            Vec2D::new(
                self.background_image.width() as f32,
                self.background_image.height() as f32,
            ),
        );
        // render the whole stack
        for d in &mut self.drawables {
            d.draw(canvas, font, bounds)?;
        }

        if onscreen && self.active_tool.borrow().get_tool_type() == Tools::Pointer {
            self.render_object_selection(canvas);
        }

        // render active tool
        if let Some(d) = self.active_tool.borrow().get_drawable() {
            d.draw(canvas, font, bounds)?;
        }

        // render crop tool
        if render_crop && let Some(c) = self.crop_tool.borrow().get_crop() {
            c.draw(canvas, font, bounds)?;
        }

        canvas.flush();
        Ok(())
    }

    fn render_object_selection(&self, canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>) {
        let Some(index) = self.selection_focus.selected else {
            return;
        };
        let Some(drawable) = self.drawables.get(index) else {
            return;
        };
        let Some(bounds) = drawable.edit_bounds() else {
            return;
        };

        edit::draw_bounds(canvas, bounds);
        edit::draw_handles(canvas, &drawable.edit_handles());
    }

    fn render_background_image(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        onscreen: bool,
    ) -> Result<()> {
        let background_image_id = match self.background_image_id {
            Some(id) => id,
            None => {
                let id = Self::upload_background_image(canvas, &self.background_image)?;
                self.background_image_id.replace(id);
                id
            }
        };

        let transparency_bg_id = match self.transparent_background_id {
            Some(id) if onscreen => Some(id),
            None => {
                if let Some(id) = Self::create_transparency_bg(canvas) {
                    self.transparent_background_id.replace(id);
                    Some(id)
                } else {
                    None
                }
            }
            _ => None,
        };

        // render the image
        let mut path = Path::new();

        let w = self.background_image.width() as f32;
        let h = self.background_image.height() as f32;

        path.rect(0.0, 0.0, w, h);

        if let Some(id) = transparency_bg_id {
            canvas.fill_path(
                &path,
                &Paint::image(
                    id,
                    0f32,
                    0f32,
                    TRANSPARENCY_SQUARE_SIZE as f32,
                    TRANSPARENCY_SQUARE_SIZE as f32,
                    0f32,
                    1f32,
                ),
            );
        }

        canvas.fill_path(
            &path,
            &Paint::image(background_image_id, 0f32, 0f32, w, h, 0f32, 1f32),
        );

        Ok(())
    }

    fn upload_background_image(
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        image: &Pixbuf,
    ) -> Result<ImageId> {
        let format = if image.has_alpha() {
            PixelFormat::Rgba8
        } else {
            PixelFormat::Rgb8
        };

        let background_image_id = canvas.create_image_empty(
            image.width() as usize,
            image.height() as usize,
            format,
            ImageFlags::empty(),
        )?;

        // extract values
        let width = image.width() as usize;
        let stride = image.rowstride() as usize; // stride is in bytes per row
        let height = image.height() as usize;
        let bytes_per_pixel = if image.has_alpha() { 4 } else { 3 }; // pixbuf supports rgb or rgba

        unsafe {
            let src_buffer = image.pixels();

            let row_length = width * bytes_per_pixel;
            let mut dst_buffer = if row_length == stride {
                // stride == row_length, there are no additional bytes after the end of each row
                src_buffer.to_vec()
            } else {
                // stride != row_length, there are additional bytes after the end of each row that
                // need to be truncated. We copy row by row..
                let mut dst_buffer = Vec::<u8>::with_capacity(width * height * bytes_per_pixel);

                for row in 0..height {
                    let src_offset = row * stride;
                    dst_buffer.extend_from_slice(&src_buffer[src_offset..src_offset + row_length]);
                }
                dst_buffer
            };

            // in almost all cases, that should be a no-op. Buf we might have additional elements after the
            // end of the buffer, e.g. after width * height * bytes_per_pixel
            dst_buffer.truncate(width * height * bytes_per_pixel);

            if image.has_alpha() {
                let img = Img::new_stride(
                    dst_buffer.align_to::<RGBA<u8>>().1.to_vec(),
                    width,
                    height,
                    width,
                );

                canvas.update_image(background_image_id, ImageSource::Rgba(img.as_ref()), 0, 0)?;
            } else {
                let img = Img::new_stride(
                    dst_buffer.align_to::<RGB<u8>>().1.to_owned(),
                    width,
                    height,
                    width,
                );

                canvas.update_image(background_image_id, ImageSource::Rgb(img.as_ref()), 0, 0)?;
            }
        }

        Ok(background_image_id)
    }

    fn create_transparency_bg(
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
    ) -> Option<femtovg::ImageId> {
        let tile: usize = TRANSPARENCY_SQUARE_SIZE * 2;
        let mut pixels = vec![RGBA8::new(204, 204, 204, 255); tile * tile];

        for y in 0..tile {
            for x in 0..tile {
                if (x / TRANSPARENCY_SQUARE_SIZE + y / TRANSPARENCY_SQUARE_SIZE) % 2 == 1 {
                    pixels[y * tile + x] = RGBA8::new(153, 153, 153, 255);
                }
            }
        }
        let img = Img::new(pixels, tile, tile);

        match canvas.create_image(
            ImageSource::Rgba(img.as_ref()),
            ImageFlags::REPEAT_X | ImageFlags::REPEAT_Y,
        ) {
            Ok(id) => Some(id),
            Err(_) => {
                eprintln!("Could not create transparency background image");
                None
            }
        }
    }

    pub fn update_transformation(
        &mut self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
    ) {
        let image_width = self.background_image.width() as f32;
        let image_height = self.background_image.height() as f32;
        let aspect_ratio = image_width / image_height;

        let canvas_width = canvas.width() as f32;
        let canvas_height = canvas.height() as f32;

        let prev_scale = self.scale_factor;
        let mut center_offset = Vec2D::zero();

        // update scale_factor
        if self.zoom_scale != 0.0 {
            if self.zoom_scale != self.last_scale {
                self.last_scale = self.zoom_scale;
                self.scale_factor = self.zoom_scale;

                if !self.is_reset {
                    // calculate offset from pointer
                    let pointer_offset = self.pointer_offset;
                    let zoom_offset = Vec2D::new(
                        (pointer_offset.x - self.offset.x) / prev_scale,
                        (pointer_offset.y - self.offset.y) / prev_scale,
                    );

                    let calculated_offset = pointer_offset - zoom_offset * self.scale_factor;

                    // update drag_offset
                    center_offset = Vec2D::new(
                        (canvas_width - image_width * self.scale_factor) / 2.0,
                        (canvas_height - image_height * self.scale_factor) / 2.0,
                    );

                    self.drag_offset = calculated_offset - center_offset;
                    self.store_last_offset();
                }
            } else {
                self.scale_factor = self.zoom_scale;
            }
        } else {
            self.scale_factor = if canvas_width / aspect_ratio <= canvas_height {
                canvas_width / aspect_ratio / image_height
            } else {
                canvas_height * aspect_ratio / image_width
            };
        }

        // final offset
        if center_offset.is_zero() {
            center_offset = Vec2D::new(
                (canvas_width - image_width * self.scale_factor) / 2.0,
                (canvas_height - image_height * self.scale_factor) / 2.0,
            );
        }

        if self.is_reset {
            //centered
            self.is_reset = false;
            self.offset = center_offset;
        } else {
            //dragged
            self.offset = center_offset + self.drag_offset;
        }
    }

    pub fn abs_canvas_to_image_coordinates(&self, input: Vec2D, dpi_scale_factor: f32) -> Vec2D {
        Vec2D::new(
            (input.x * dpi_scale_factor - self.offset.x) / self.scale_factor,
            (input.y * dpi_scale_factor - self.offset.y) / self.scale_factor,
        )
    }
    pub fn rel_canvas_to_image_coordinates(&self, input: Vec2D, dpi_scale_factor: f32) -> Vec2D {
        Vec2D::new(
            input.x * dpi_scale_factor / self.scale_factor,
            input.y * dpi_scale_factor / self.scale_factor,
        )
    }

    pub fn set_zoom_scale(&mut self, factor: f32, abs: bool) {
        if self.is_drag {
            return;
        }

        if abs {
            self.zoom_scale = factor;
        } else {
            if self.zoom_scale == 0.0 {
                self.zoom_scale = self.scale_factor;
            }

            self.zoom_scale *= factor;
            self.zoom_scale = self.zoom_scale.max(0.);
        }
    }

    pub fn set_pointer_offset(&mut self, offset: Vec2D) {
        self.pointer_offset = offset;
    }

    pub fn set_drag_offset(&mut self, offset: Vec2D) {
        self.drag_offset = self.last_offset + offset;
    }

    pub fn reset_drag_offset(&mut self) {
        self.drag_offset = Vec2D::zero();
        self.store_last_offset();
        self.is_reset = true;
    }

    pub fn store_last_offset(&mut self) {
        self.last_offset = self.drag_offset;
    }

    pub fn set_is_drag(&mut self, is_drag: bool) {
        self.is_drag = is_drag;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        style::{Color, Size, Style},
        tools::{StyleChange, ToolsManager, edit::ObjectBounds},
    };
    use relm4::gtk::gdk_pixbuf::Colorspace;

    #[derive(Clone, Debug)]
    struct TestDrawable {
        pos: Vec2D,
        size: Vec2D,
        style: Style,
        handles: Option<Vec<(EditHandle, Vec2D)>>,
    }

    impl Drawable for TestDrawable {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
            self
        }

        fn draw(
            &self,
            _canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
            _font: FontId,
            _bounds: (Vec2D, Vec2D),
        ) -> Result<()> {
            Ok(())
        }

        fn edit_bounds(&self) -> Option<ObjectBounds> {
            Some(ObjectBounds::new(self.pos, self.size))
        }

        fn move_by(&mut self, delta: Vec2D) -> bool {
            if delta.is_zero() {
                return false;
            }
            self.pos += delta;
            true
        }

        fn resize(&mut self, handle: EditHandle, delta: Vec2D) -> bool {
            if delta.is_zero() {
                return false;
            }

            let bounds = edit::resize_box(self.edit_bounds().unwrap(), handle, delta);
            self.pos = bounds.top_left;
            self.size = bounds.size;
            true
        }

        fn edit_handles(&self) -> Vec<(EditHandle, Vec2D)> {
            self.handles
                .clone()
                .unwrap_or_else(|| edit::box_handles(self.edit_bounds().unwrap()))
        }

        fn apply_style_change(&mut self, change: StyleChange) -> bool {
            crate::tools::apply_style_change_to_style(&mut self.style, change)
        }
    }

    fn test_drawable(x: f32) -> Box<dyn Drawable> {
        Box::new(TestDrawable {
            pos: Vec2D::new(x, 0.0),
            size: Vec2D::new(10.0, 10.0),
            style: Style {
                color: Color::red(),
                size: Size::Medium,
                fill: false,
                annotation_size_factor: 1.0,
            },
            handles: None,
        })
    }

    fn test_line_drawable() -> Box<dyn Drawable> {
        Box::new(TestDrawable {
            pos: Vec2D::new(0.0, 0.0),
            size: Vec2D::new(10.0, 0.0),
            style: Style {
                color: Color::red(),
                size: Size::Medium,
                fill: false,
                annotation_size_factor: 1.0,
            },
            handles: Some(vec![
                (EditHandle::Start, Vec2D::new(0.0, 0.0)),
                (EditHandle::End, Vec2D::new(10.0, 0.0)),
            ]),
        })
    }

    fn test_area() -> FemtoVgAreaMut {
        let tools = ToolsManager::new();
        FemtoVgAreaMut {
            background_image: Pixbuf::new(Colorspace::Rgb, false, 8, 1, 1).unwrap(),
            background_image_id: None,
            transparent_background_id: None,
            active_tool: tools.get(&Tools::Pointer),
            crop_tool: tools.get_crop_tool(),
            scale_factor: 1.0,
            offset: Vec2D::zero(),
            drawables: Vec::new(),
            history: Vec::new(),
            redo_history: Vec::new(),
            selection_focus: SelectionFocusState::default(),
            transform_session: None,
            zoom_scale: 0.0,
            last_scale: 0.0,
            pointer_offset: Vec2D::zero(),
            last_offset: Vec2D::zero(),
            drag_offset: Vec2D::zero(),
            is_drag: false,
            is_reset: false,
        }
    }

    fn drawable_pos(area: &FemtoVgAreaMut, index: usize) -> Vec2D {
        area.drawables[index].edit_bounds().unwrap().top_left
    }

    fn drawable_size(area: &FemtoVgAreaMut, index: usize) -> Vec2D {
        area.drawables[index].edit_bounds().unwrap().size
    }

    fn drawable_style(area: &FemtoVgAreaMut, index: usize) -> Style {
        area.drawables[index]
            .edit_snapshot()
            .into_any()
            .downcast::<TestDrawable>()
            .unwrap()
            .style
    }

    #[test]
    fn add_history_undo_redo_moves_drawable_between_stacks() {
        let mut area = test_area();

        area.commit(test_drawable(0.0));
        assert_eq!(area.drawables.len(), 1);

        assert!(area.undo());
        assert!(area.drawables.is_empty());

        assert!(area.redo());
        assert_eq!(area.drawables.len(), 1);
        assert_eq!(drawable_pos(&area, 0), Vec2D::new(0.0, 0.0));
    }

    #[test]
    fn commit_sets_style_target_without_selecting_object() {
        let mut area = test_area();

        area.commit(test_drawable(0.0));

        assert_eq!(area.selection_focus.selected, None);
        assert_eq!(area.selection_focus.focused, Some(0));
    }

    #[test]
    fn style_change_updates_target_in_place_and_is_undoable() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));
        let original_history_len = area.history.len();

        assert!(area.apply_style_change_to_target(StyleChange::Color(Color::blue())));

        assert_eq!(area.drawables.len(), 1);
        assert_eq!(drawable_style(&area, 0).color, Color::blue());
        assert_eq!(area.history.len(), original_history_len + 1);

        assert!(area.undo());
        assert_eq!(area.drawables.len(), 1);
        assert_eq!(drawable_style(&area, 0).color, Color::red());

        assert!(area.redo());
        assert_eq!(drawable_style(&area, 0).color, Color::blue());
    }

    #[test]
    fn pointer_selection_overrides_previous_style_target() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));
        area.commit(test_drawable(30.0));

        assert!(area.pointer_click(Vec2D::new(5.0, 5.0)));
        assert!(area.apply_style_change_to_target(StyleChange::Size(Size::Large)));

        assert_eq!(drawable_style(&area, 0).size, Size::Large);
        assert_eq!(drawable_style(&area, 1).size, Size::Medium);
    }

    #[test]
    fn switching_to_drawing_tool_clears_style_target() {
        let mut area = test_area();
        let tools = ToolsManager::new();
        area.commit(test_drawable(0.0));

        area.set_active_tool(tools.get(&Tools::Text));

        assert_eq!(area.selection_focus.focused, None);
        assert!(!area.apply_style_change_to_target(StyleChange::Color(Color::blue())));
        assert_eq!(drawable_style(&area, 0).color, Color::red());
    }

    #[test]
    fn pointer_hover_cursor_uses_selected_resize_handles() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));
        assert!(area.pointer_click(Vec2D::new(5.0, 5.0)));

        assert_eq!(
            area.pointer_hover_cursor(Vec2D::new(0.0, 0.0)),
            Some(PointerCursor::ResizeNwSe)
        );
        assert_eq!(
            area.pointer_hover_cursor(Vec2D::new(5.0, 0.0)),
            Some(PointerCursor::ResizeNs)
        );
    }

    #[test]
    fn pointer_hover_cursor_uses_move_for_object_body() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));

        assert_eq!(
            area.pointer_hover_cursor(Vec2D::new(5.0, 5.0)),
            Some(PointerCursor::Move)
        );
    }

    #[test]
    fn pointer_hover_cursor_clears_on_empty_area() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));

        assert_eq!(area.pointer_hover_cursor(Vec2D::new(50.0, 50.0)), None);
    }

    #[test]
    fn pointer_hover_cursor_uses_all_resize_for_line_endpoints() {
        let mut area = test_area();
        area.commit(test_line_drawable());
        assert!(area.pointer_click(Vec2D::new(5.0, 0.0)));

        assert_eq!(
            area.pointer_hover_cursor(Vec2D::new(10.0, 0.0)),
            Some(PointerCursor::ResizeAll)
        );
    }

    #[test]
    fn temporary_pointer_hover_cursor_uses_virtual_handles_without_selection() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));

        assert_eq!(
            area.temporary_pointer_hover_cursor(Vec2D::new(0.0, 0.0)),
            Some(PointerCursor::ResizeNwSe)
        );
        assert_eq!(area.selection_focus.selected, None);
    }

    #[test]
    fn temporary_pointer_hover_cursor_uses_move_for_body() {
        let mut area = test_area();
        area.scale_factor = 4.0;
        area.commit(test_drawable(0.0));

        assert_eq!(
            area.temporary_pointer_hover_cursor(Vec2D::new(5.0, 5.0)),
            Some(PointerCursor::Move)
        );
    }

    #[test]
    fn temporary_pointer_hover_cursor_clears_on_empty_area() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));

        assert_eq!(
            area.temporary_pointer_hover_cursor(Vec2D::new(50.0, 50.0)),
            None
        );
    }

    #[test]
    fn temporary_pointer_drag_moves_without_visible_selection() {
        let mut area = test_area();
        area.scale_factor = 4.0;
        area.commit(test_drawable(0.0));

        assert!(area.temporary_pointer_begin_drag(Vec2D::new(5.0, 5.0)));
        assert_eq!(area.selection_focus.selected, None);
        assert_eq!(area.selection_focus.focused, Some(0));
        assert!(area.pointer_end_drag(Vec2D::new(12.0, 0.0)));

        assert_eq!(drawable_pos(&area, 0), Vec2D::new(12.0, 0.0));
        assert_eq!(area.selection_focus.selected, None);
    }

    #[test]
    fn temporary_pointer_drag_resizes_without_visible_selection() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));

        assert!(area.temporary_pointer_begin_drag(Vec2D::new(0.0, 0.0)));
        assert_eq!(area.selection_focus.selected, None);
        assert!(area.pointer_end_drag(Vec2D::new(2.0, 3.0)));

        assert_eq!(drawable_pos(&area, 0), Vec2D::new(2.0, 3.0));
        assert_eq!(drawable_size(&area, 0), Vec2D::new(8.0, 7.0));
        assert_eq!(area.selection_focus.selected, None);
    }

    #[test]
    fn temporary_pointer_click_targets_style_without_selecting() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));

        assert!(area.temporary_pointer_click(Vec2D::new(5.0, 5.0)));
        assert_eq!(area.selection_focus.selected, None);
        assert_eq!(area.selection_focus.focused, Some(0));
        assert!(area.apply_style_change_to_target(StyleChange::Color(Color::blue())));

        assert_eq!(drawable_style(&area, 0).color, Color::blue());
        assert_eq!(area.selection_focus.selected, None);
    }

    #[test]
    fn temporary_pointer_click_on_empty_area_preserves_target() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));

        assert!(!area.temporary_pointer_click(Vec2D::new(50.0, 50.0)));

        assert_eq!(area.selection_focus.selected, None);
        assert_eq!(area.selection_focus.focused, Some(0));
    }

    #[test]
    fn temporary_pointer_hit_testing_prefers_topmost_drawable() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));
        area.commit(test_drawable(0.0));

        assert!(area.temporary_pointer_click(Vec2D::new(5.0, 5.0)));
        assert!(area.apply_style_change_to_target(StyleChange::Color(Color::blue())));

        assert_eq!(drawable_style(&area, 0).color, Color::red());
        assert_eq!(drawable_style(&area, 1).color, Color::blue());
    }

    #[test]
    fn modify_history_undo_redo_restores_snapshots() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));

        assert!(area.pointer_begin_drag(Vec2D::new(5.0, 5.0)));
        assert!(area.pointer_end_drag(Vec2D::new(12.0, 0.0)));
        assert_eq!(drawable_pos(&area, 0), Vec2D::new(12.0, 0.0));

        assert!(area.undo());
        assert_eq!(drawable_pos(&area, 0), Vec2D::new(0.0, 0.0));

        assert!(area.redo());
        assert_eq!(drawable_pos(&area, 0), Vec2D::new(12.0, 0.0));
    }

    #[test]
    fn noop_pointer_drag_does_not_push_history() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));
        let original_history_len = area.history.len();

        assert!(area.pointer_begin_drag(Vec2D::new(5.0, 5.0)));
        assert!(!area.pointer_end_drag(Vec2D::zero()));

        assert_eq!(area.history.len(), original_history_len);
        assert_eq!(drawable_pos(&area, 0), Vec2D::new(0.0, 0.0));
    }

    #[test]
    fn redo_history_is_cleared_after_new_change() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));

        assert!(area.pointer_begin_drag(Vec2D::new(5.0, 5.0)));
        assert!(area.pointer_end_drag(Vec2D::new(12.0, 0.0)));
        assert!(area.undo());

        area.commit(test_drawable(30.0));
        assert!(!area.redo());
    }

    #[test]
    fn reset_history_restores_drawables_in_order() {
        let mut area = test_area();
        area.commit(test_drawable(0.0));
        area.commit(test_drawable(20.0));

        assert!(area.reset());
        assert!(area.drawables.is_empty());

        assert!(area.undo());
        assert_eq!(area.drawables.len(), 2);
        assert_eq!(drawable_pos(&area, 0), Vec2D::new(0.0, 0.0));
        assert_eq!(drawable_pos(&area, 1), Vec2D::new(20.0, 0.0));
    }
}
