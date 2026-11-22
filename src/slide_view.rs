use std::sync::Arc;

use gpui_kit::{Bounds, Corners, Pixels, RenderImage, Window, point, px, size};

use crate::{
    pdf::{PageSize, RasterSize},
    settings::SlideFit,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PagePlacement {
    pub bounds: Bounds<Pixels>,
    pub raster_size: RasterSize,
    pub render_bounds: RasterSize,
}

/// Measures how many physical pixels the PDF should cover and aligns its origin to that grid.
pub(crate) fn page_placement(
    bounds: Bounds<Pixels>,
    scale: f32,
    geometry: PageSize,
    fit: SlideFit,
) -> Option<PagePlacement> {
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let scale = f64::from(scale);
    let (left, width) = pixel_span(bounds.origin.x, bounds.size.width, scale)?;
    let (top, height) = match fit {
        SlideFit::WholePage => pixel_span(bounds.origin.y, bounds.size.height, scale)?,
        SlideFit::FitWidth => {
            // The scrolled page origin can be negative. Its visible height belongs to
            // the scroll viewport and does not constrain the full-width page raster.
            let top = (f64::from(bounds.origin.y.as_f32()) * scale).round();
            if !top.is_finite() {
                return None;
            }
            (top, u32::MAX)
        }
    };
    let render_bounds = RasterSize { width, height };
    let raster_size = geometry.fit(render_bounds)?;
    let x = left + f64::from((width - raster_size.width) / 2);
    let y = match fit {
        SlideFit::WholePage => top + f64::from((height - raster_size.height) / 2),
        SlideFit::FitWidth => top,
    };
    let logical = |device: f64| px((device / scale) as f32);

    Some(PagePlacement {
        bounds: Bounds::new(
            point(logical(x), logical(y)),
            size(
                logical(f64::from(raster_size.width)),
                logical(f64::from(raster_size.height)),
            ),
        ),
        raster_size,
        render_bounds,
    })
}

/// Fits an existing bitmap to the viewport as an immediate rendering fallback.
pub(crate) fn fitted_page(
    bounds: Bounds<Pixels>,
    scale: f32,
    source: RasterSize,
) -> Option<(Bounds<Pixels>, RasterSize)> {
    let geometry = PageSize {
        width: f64::from(source.width),
        height: f64::from(source.height),
    };
    let placement = page_placement(bounds, scale, geometry, SlideFit::WholePage)?;
    Some((placement.bounds, placement.raster_size))
}

fn pixel_span(origin: Pixels, length: Pixels, scale: f64) -> Option<(f64, u32)> {
    let origin = f64::from(origin.as_f32());
    let length = f64::from(length.as_f32());
    let start = (origin * scale).ceil();
    let end = ((origin + length) * scale).floor();
    let extent = end - start;
    (1.0..=f64::from(u32::MAX))
        .contains(&extent)
        .then_some((start, extent as u32))
}

pub(crate) fn raster_size(image: &RenderImage) -> RasterSize {
    let size = image.size(0);
    RasterSize {
        width: u32::try_from(size.width.0).unwrap_or_default(),
        height: u32::try_from(size.height.0).unwrap_or_default(),
    }
}

pub(crate) fn paint_page(
    bounds: Bounds<Pixels>,
    image_bounds: Bounds<Pixels>,
    image: Arc<RenderImage>,
    window: &mut Window,
) -> bool {
    window
        .paint_image(bounds, image_bounds, Corners::default(), image, 0, false)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_page_uses_exact_device_pixels_from_pdf_geometry() {
        let viewport = Bounds::new(point(px(3.), px(5.)), size(px(960.), px(540.)));
        let geometry = PageSize {
            width: 50.5,
            height: 100.0,
        };

        for scale in [1.25, 1.5, 2.0] {
            let placement = page_placement(viewport, scale, geometry, SlideFit::WholePage).unwrap();
            let pixels = |point: Pixels| (point.as_f32() * scale).round() as u32;
            let left = (viewport.origin.x.as_f32() * scale).ceil() as u32;
            let top = (viewport.origin.y.as_f32() * scale).ceil() as u32;
            let right = ((viewport.origin.x.as_f32() + viewport.size.width.as_f32()) * scale)
                .floor() as u32;
            let bottom = ((viewport.origin.y.as_f32() + viewport.size.height.as_f32()) * scale)
                .floor() as u32;
            let width = (f64::from(bottom - top) * 0.505).round() as u32;

            assert_eq!(
                placement.raster_size,
                RasterSize {
                    width,
                    height: bottom - top
                }
            );
            assert_eq!(
                placement.render_bounds,
                RasterSize {
                    width: right - left,
                    height: bottom - top
                }
            );
            assert_eq!(
                geometry.fit(placement.render_bounds),
                Some(placement.raster_size)
            );
            assert_eq!(
                pixels(placement.bounds.origin.x),
                left + (right - left - width) / 2
            );
            assert_eq!(pixels(placement.bounds.origin.y), top);
            assert_eq!(pixels(placement.bounds.size.width), width);
            assert_eq!(pixels(placement.bounds.size.height), bottom - top);
        }
    }

    #[test]
    fn fit_width_anchors_the_full_height_at_the_scrolled_page_origin() {
        let viewport = Bounds::new(point(px(10.2), px(-37.8)), size(px(321.4), px(110.0)));
        let geometry = PageSize {
            width: 100.,
            height: 200.,
        };
        let placement = page_placement(viewport, 1.25, geometry, SlideFit::FitWidth).unwrap();
        assert_eq!(
            placement.raster_size,
            RasterSize {
                width: 401,
                height: 802
            }
        );
        assert_eq!(placement.bounds.origin, point(px(10.4), px(-37.6)));
        assert_eq!(placement.bounds.size, size(px(320.8), px(641.6)));
        assert_eq!(
            geometry.fit(placement.render_bounds),
            Some(placement.raster_size)
        );
    }

    #[test]
    fn fits_a_retina_viewport_in_device_pixels() {
        let viewport = Bounds::new(point(px(0.0), px(0.0)), size(px(960.0), px(540.0)));
        let source = RasterSize {
            width: 3840,
            height: 2160,
        };
        let (bounds, target) = fitted_page(viewport, 2.0, source).unwrap();
        assert_eq!(bounds, viewport);
        assert_eq!(
            target,
            RasterSize {
                width: 1920,
                height: 1080,
            }
        );
    }

    #[test]
    fn centers_a_portrait_page_on_whole_pixels_inside_a_small_viewport() {
        let viewport = Bounds::new(point(px(1.25), px(2.75)), size(px(10.5), px(6.5)));
        let source = RasterSize {
            width: 500,
            height: 1000,
        };
        let (bounds, target) = fitted_page(viewport, 2.0, source).unwrap();
        assert_eq!(
            target,
            RasterSize {
                width: 6,
                height: 12,
            }
        );
        assert_eq!(
            bounds,
            Bounds::new(point(px(5.0), px(3.0)), size(px(3.0), px(6.0)))
        );
    }

    #[test]
    fn fits_a_landscape_page_inside_a_tall_viewport() {
        let viewport = Bounds::new(point(px(0.0), px(0.0)), size(px(540.0), px(960.0)));
        let source = RasterSize {
            width: 3840,
            height: 2160,
        };
        let (bounds, target) = fitted_page(viewport, 1.5, source).unwrap();
        assert_eq!(
            target,
            RasterSize {
                width: 810,
                height: 456,
            }
        );
        assert_eq!(
            bounds,
            Bounds::new(point(px(0.0), px(328.0)), size(px(540.0), px(304.0)))
        );
    }

    #[test]
    fn fits_a_portrait_page_inside_a_standard_landscape_window() {
        let viewport = Bounds::new(point(px(0.0), px(0.0)), size(px(960.0), px(540.0)));
        let source = RasterSize {
            width: 2160,
            height: 3840,
        };
        let (bounds, target) = fitted_page(viewport, 1.0, source).unwrap();
        assert_eq!(
            target,
            RasterSize {
                width: 304,
                height: 540,
            }
        );
        assert_eq!(
            bounds,
            Bounds::new(point(px(328.0), px(0.0)), size(px(304.0), px(540.0)))
        );
    }

    #[test]
    fn ignores_viewports_without_a_whole_device_pixel() {
        let viewport = Bounds::new(point(px(0.0), px(0.0)), size(px(0.3), px(100.0)));
        let source = RasterSize {
            width: 1280,
            height: 720,
        };
        assert!(fitted_page(viewport, 2.0, source).is_none());
        assert!(fitted_page(viewport, f32::NAN, source).is_none());
    }
}
