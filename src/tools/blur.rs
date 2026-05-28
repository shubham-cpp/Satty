use std::cell::RefCell;

use anyhow::Result;
use femtovg::{Color, ImageFilter, ImageFlags, ImageId, Paint, Path, imgref::Img};

use relm4::{Sender, gtk::gdk::Key};

use crate::{
    configuration::APP_CONFIG,
    math::{self, Vec2D},
    sketch_board::{MouseButton, MouseEventMsg, MouseEventType, SketchBoardInput},
    style::Style,
};

use super::{
    Drawable, DrawableClone, StyleChange, Tool, ToolUpdateResult, Tools,
    edit::{self, EditHandle, ObjectBounds},
};

#[derive(Debug)]
pub struct Blur {
    top_left: Vec2D,
    size: Option<Vec2D>,
    style: Style,
    editing: bool,
    cached_image: RefCell<Option<ImageId>>,
}

impl Clone for Blur {
    fn clone(&self) -> Self {
        Self {
            top_left: self.top_left,
            size: self.size,
            style: self.style,
            editing: self.editing,
            cached_image: RefCell::new(None),
        }
    }
}

impl Blur {
    fn blur(
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        pos: Vec2D,
        size: Vec2D,
        sigma: f32,
    ) -> Result<ImageId> {
        let img = canvas.screenshot()?;

        let transformed_pos = canvas.transform().transform_point(pos.x, pos.y);
        let transformed_size = size * canvas.transform().average_scale();

        let (buf, width, height) = img
            .sub_image(
                transformed_pos.0 as usize,
                transformed_pos.1 as usize,
                (transformed_size.x as usize).max(1),
                (transformed_size.y as usize).max(1),
            )
            .to_contiguous_buf();
        let sub = Img::new(buf.into_owned(), width, height);

        let src_image_id = canvas.create_image(sub.as_ref(), ImageFlags::empty())?;
        let dst_image_id = canvas.create_image_empty(
            sub.width(),
            sub.height(),
            femtovg::PixelFormat::Rgba8,
            ImageFlags::empty(),
        )?;

        canvas.filter_image(
            dst_image_id,
            ImageFilter::GaussianBlur { sigma },
            src_image_id,
        );
        //canvas.delete_image(src_image_id);

        Ok(dst_image_id)
    }
}

impl Drawable for Blur {
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }

    fn draw(
        &self,
        canvas: &mut femtovg::Canvas<femtovg::renderer::OpenGl>,
        _font: femtovg::FontId,
        bounds: (Vec2D, Vec2D),
    ) -> Result<()> {
        let size = match self.size {
            Some(s) => s,
            None => return Ok(()), // early exit if none
        };
        let (pos, size) = math::rect_ensure_in_bounds(
            math::rect_ensure_positive_size(self.top_left, size),
            bounds,
        );
        if self.editing {
            // set style
            let mut color = Color::black();
            color.set_alphaf(0.6);
            let paint = Paint::color(color);

            // make rect
            let mut path = Path::new();
            path.rounded_rect(
                pos.x,
                pos.y,
                size.x,
                size.y,
                APP_CONFIG.read().corner_roundness(),
            );

            // draw
            canvas.fill_path(&path, &paint);
        } else {
            if size.x <= 0.0 || size.y <= 0.0 {
                return Ok(());
            }

            canvas.save();
            canvas.flush();

            // create new cached image
            if self.cached_image.borrow().is_none() {
                self.cached_image.borrow_mut().replace(Self::blur(
                    canvas,
                    pos,
                    size,
                    self.style
                        .size
                        .to_blur_factor(self.style.annotation_size_factor),
                )?);
            }

            let mut path = Path::new();
            path.rounded_rect(
                pos.x,
                pos.y,
                size.x,
                size.y,
                APP_CONFIG.read().corner_roundness(),
            );

            canvas.fill_path(
                &path,
                &Paint::image(
                    self.cached_image.borrow().unwrap(), // this unwrap is safe because we placed it above
                    pos.x,
                    pos.y,
                    size.x,
                    size.y,
                    0f32,
                    1f32,
                ),
            );
            canvas.restore();
        }
        Ok(())
    }

    fn edit_bounds(&self) -> Option<ObjectBounds> {
        self.size.map(|size| ObjectBounds::new(self.top_left, size))
    }

    fn move_by(&mut self, delta: Vec2D) -> bool {
        if self.size.is_none() || delta.is_zero() {
            return false;
        }
        self.top_left += delta;
        self.invalidate_edit_cache();
        true
    }

    fn resize(&mut self, handle: EditHandle, delta: Vec2D) -> bool {
        let Some(bounds) = self.edit_bounds() else {
            return false;
        };
        if delta.is_zero() {
            return false;
        }

        let bounds = edit::resize_box(bounds, handle, delta);
        self.top_left = bounds.top_left;
        self.size = Some(bounds.size);
        self.editing = false;
        self.invalidate_edit_cache();
        true
    }

    fn invalidate_edit_cache(&mut self) {
        self.cached_image.borrow_mut().take();
    }

    fn edit_snapshot(&self) -> Box<dyn Drawable> {
        Box::new(self.clone())
    }

    fn apply_style_change(&mut self, change: StyleChange) -> bool {
        let changed = match change {
            StyleChange::Size(size) => {
                if self.style.size == size {
                    false
                } else {
                    self.style.size = size;
                    true
                }
            }
            StyleChange::AnnotationSizeFactor(annotation_size_factor) => {
                if (self.style.annotation_size_factor - annotation_size_factor).abs()
                    <= f32::EPSILON
                {
                    false
                } else {
                    self.style.annotation_size_factor = annotation_size_factor;
                    true
                }
            }
            StyleChange::Color(_) | StyleChange::Fill(_) => false,
        };
        if changed {
            self.invalidate_edit_cache();
        }
        changed
    }
}

#[derive(Default)]
pub struct BlurTool {
    blur: Option<Blur>,
    style: Style,
    input_enabled: bool,
    sender: Option<Sender<SketchBoardInput>>,
}

impl Tool for BlurTool {
    fn input_enabled(&self) -> bool {
        self.input_enabled
    }

    fn set_input_enabled(&mut self, value: bool) {
        self.input_enabled = value;
    }

    fn get_tool_type(&self) -> super::Tools {
        Tools::Blur
    }

    fn handle_mouse_event(&mut self, event: MouseEventMsg) -> ToolUpdateResult {
        match event.type_ {
            MouseEventType::BeginDrag => {
                if event.button == MouseButton::Middle {
                    return ToolUpdateResult::Unmodified;
                }

                // start new
                self.blur = Some(Blur {
                    top_left: event.pos,
                    size: None,
                    style: self.style,
                    editing: true,
                    cached_image: RefCell::new(None),
                });

                ToolUpdateResult::Redraw
            }
            MouseEventType::EndDrag => {
                if event.button == MouseButton::Middle {
                    return ToolUpdateResult::Unmodified;
                }

                if let Some(a) = &mut self.blur {
                    if event.pos == Vec2D::zero() {
                        self.blur = None;

                        ToolUpdateResult::Redraw
                    } else {
                        a.size = Some(event.pos);
                        a.editing = false;

                        let result = a.clone_box();
                        self.blur = None;

                        ToolUpdateResult::Commit(result)
                    }
                } else {
                    ToolUpdateResult::Unmodified
                }
            }
            MouseEventType::UpdateDrag => {
                if event.button == MouseButton::Middle {
                    return ToolUpdateResult::Unmodified;
                }

                if let Some(a) = &mut self.blur {
                    if event.pos == Vec2D::zero() {
                        return ToolUpdateResult::Unmodified;
                    }
                    a.size = Some(event.pos);

                    ToolUpdateResult::Redraw
                } else {
                    ToolUpdateResult::Unmodified
                }
            }
            _ => ToolUpdateResult::Unmodified,
        }
    }

    fn handle_key_event(&mut self, event: crate::sketch_board::KeyEventMsg) -> ToolUpdateResult {
        if event.key == Key::Escape && self.blur.is_some() {
            self.blur = None;
            ToolUpdateResult::Redraw
        } else {
            ToolUpdateResult::Unmodified
        }
    }

    fn handle_style_event(&mut self, style: Style) -> ToolUpdateResult {
        self.style = style;
        ToolUpdateResult::Unmodified
    }

    fn get_drawable(&self) -> Option<&dyn Drawable> {
        match &self.blur {
            Some(d) => Some(d),
            None => None,
        }
    }

    fn set_sender(&mut self, sender: Sender<SketchBoardInput>) {
        self.sender = Some(sender);
    }
}
