use std::cell::{Cell, RefCell};

use cairo::{self, ImageSurface, MatrixTrait, PatternTrait, Rectangle};

use allowed_url::{Fragment, Href};
use aspect_ratio::AspectRatio;
use attributes::Attribute;
use drawing_ctx::DrawingCtx;
use error::{NodeError, RenderingError};
use float_eq_cairo::ApproxEqCairo;
use node::{CascadedValues, NodeResult, NodeTrait, RsvgNode};
use parsers::{ParseError, ParseValue};
use property_bag::PropertyBag;
use rect::{IRect, RectangleExt};
use surface_utils::shared_surface::{SharedImageSurface, SurfaceType};
use viewbox::ViewBox;

use super::context::{FilterContext, FilterOutput, FilterResult};
use super::{Filter, FilterError, Primitive};

/// The `feImage` filter primitive.
pub struct Image {
    base: Primitive,
    aspect: Cell<AspectRatio>,
    href: RefCell<Option<Href>>,
}

impl Image {
    /// Constructs a new `Image` with empty properties.
    #[inline]
    pub fn new() -> Image {
        Image {
            base: Primitive::new::<Self>(),
            aspect: Cell::new(AspectRatio::default()),
            href: RefCell::new(None),
        }
    }

    /// Renders the filter if the source is an existing node.
    fn render_node(
        &self,
        ctx: &FilterContext,
        draw_ctx: &mut DrawingCtx,
        bounds: IRect,
        fragment: &Fragment,
    ) -> Result<ImageSurface, FilterError> {
        let acquired_drawable = draw_ctx
            .get_acquired_node(fragment)
            .ok_or(FilterError::InvalidInput)?;
        let drawable = acquired_drawable.get();

        let surface = ImageSurface::create(
            cairo::Format::ARgb32,
            ctx.source_graphic().width(),
            ctx.source_graphic().height(),
        )?;

        draw_ctx.get_cairo_context().set_matrix(ctx.paffine());

        let node_being_filtered_values = ctx.get_computed_values_from_node_being_filtered();

        let cascaded = CascadedValues::new_from_values(&drawable, node_being_filtered_values);

        draw_ctx
            .draw_node_on_surface(
                &drawable,
                &cascaded,
                &surface,
                f64::from(ctx.source_graphic().width()),
                f64::from(ctx.source_graphic().height()),
            )
            .map_err(|e| {
                if let RenderingError::Cairo(status) = e {
                    FilterError::CairoError(status)
                } else {
                    // FIXME: this is just a dummy value; we should probably have a way to indicate
                    // an error in the underlying drawing process.
                    FilterError::CairoError(cairo::Status::InvalidStatus)
                }
            })?;

        // Clip the output to bounds.
        let output_surface = ImageSurface::create(
            cairo::Format::ARgb32,
            ctx.source_graphic().width(),
            ctx.source_graphic().height(),
        )?;

        let cr = cairo::Context::new(&output_surface);
        cr.rectangle(
            f64::from(bounds.x0),
            f64::from(bounds.y0),
            f64::from(bounds.x1 - bounds.x0),
            f64::from(bounds.y1 - bounds.y0),
        );
        cr.clip();
        cr.set_source_surface(&surface, 0f64, 0f64);
        cr.paint();

        Ok(output_surface)
    }

    /// Renders the filter if the source is an external image.
    fn render_external_image(
        &self,
        ctx: &FilterContext,
        draw_ctx: &DrawingCtx,
        bounds: &IRect,
        unclipped_bounds: &IRect,
        href: &Href,
    ) -> Result<ImageSurface, FilterError> {
        let surface = if let Href::PlainUrl(ref url) = *href {
            // FIXME: translate the error better here
            draw_ctx
                .lookup_image(&url)
                .map_err(|_| FilterError::InvalidInput)?
        } else {
            unreachable!();
        };

        let output_surface = ImageSurface::create(
            cairo::Format::ARgb32,
            ctx.source_graphic().width(),
            ctx.source_graphic().height(),
        )?;

        // TODO: this goes through a f64->i32->f64 conversion.
        let aspect = self.aspect.get();
        let (x, y, w, h) = aspect.compute(
            &ViewBox::new(
                0.0,
                0.0,
                f64::from(surface.width()),
                f64::from(surface.height()),
            ),
            &Rectangle::new(
                f64::from(unclipped_bounds.x0),
                f64::from(unclipped_bounds.y0),
                f64::from(unclipped_bounds.x1 - unclipped_bounds.x0),
                f64::from(unclipped_bounds.y1 - unclipped_bounds.y0),
            ),
        );

        if w.approx_eq_cairo(&0.0) || h.approx_eq_cairo(&0.0) {
            return Ok(output_surface);
        }

        let ptn = surface.to_cairo_pattern();
        let mut matrix = cairo::Matrix::new(
            w / f64::from(surface.width()),
            0f64,
            0f64,
            h / f64::from(surface.height()),
            x,
            y,
        );
        matrix.invert();
        ptn.set_matrix(matrix);

        let cr = cairo::Context::new(&output_surface);
        cr.rectangle(
            f64::from(bounds.x0),
            f64::from(bounds.y0),
            f64::from(bounds.x1 - bounds.x0),
            f64::from(bounds.y1 - bounds.y0),
        );
        cr.clip();
        cr.set_source(&ptn);
        cr.paint();

        Ok(output_surface)
    }
}

impl NodeTrait for Image {
    fn set_atts(&self, node: &RsvgNode, pbag: &PropertyBag<'_>) -> NodeResult {
        self.base.set_atts(node, pbag)?;

        for (attr, value) in pbag.iter() {
            match attr {
                Attribute::PreserveAspectRatio => self.aspect.set(attr.parse(value)?),

                // "path" is used by some older Adobe Illustrator versions
                Attribute::XlinkHref | Attribute::Path => {
                    let href = Href::parse(value).map_err(|_| {
                        NodeError::parse_error(attr, ParseError::new("could not parse href"))
                    })?;

                    *self.href.borrow_mut() = Some(href);
                }

                _ => (),
            }
        }

        Ok(())
    }
}

impl Filter for Image {
    fn render(
        &self,
        _node: &RsvgNode,
        ctx: &FilterContext,
        draw_ctx: &mut DrawingCtx,
    ) -> Result<FilterResult, FilterError> {
        let bounds_builder = self.base.get_bounds(ctx);
        let bounds = bounds_builder.into_irect(draw_ctx);

        let href_borrow = self.href.borrow();
        let href_opt = href_borrow.as_ref();

        if let Some(href) = href_opt {
            let output_surface = match *href {
                Href::PlainUrl(_) => {
                    let unclipped_bounds = bounds_builder.into_irect_without_clipping(draw_ctx);
                    self.render_external_image(ctx, draw_ctx, &bounds, &unclipped_bounds, href)?
                }
                Href::WithFragment(ref frag) => self.render_node(ctx, draw_ctx, bounds, frag)?,
            };

            Ok(FilterResult {
                name: self.base.result.borrow().clone(),
                output: FilterOutput {
                    surface: SharedImageSurface::new(output_surface, SurfaceType::SRgb)?,
                    bounds,
                },
            })
        } else {
            Err(FilterError::InvalidInput)
        }
    }

    #[inline]
    fn is_affected_by_color_interpolation_filters(&self) -> bool {
        false
    }
}
