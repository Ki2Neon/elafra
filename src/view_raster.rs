use std::sync::Arc;

use gpui_kit::{Entity, RenderImage, Styled, Task, WindowId, canvas};

use crate::{
    pdf::{PageSize, PdfError, RasterSize},
    presentation::Presentation,
    settings::SlideFit,
    slide_view::{fitted_page, page_placement, paint_page, raster_size},
};

// An optional full-width raster can exceed the DirectX atlas dimensions on extra-tall pages.
pub(crate) const MAX_TEXTURE_EDGE: u32 = 16_384;

#[derive(Clone, Copy)]
pub(crate) enum SlideViewport {
    Audience(WindowId),
    CurrentSlide,
}

impl SlideViewport {
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Audience(_) => 0,
            Self::CurrentSlide => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PageStamp {
    pub document: u64,
    pub generation: u64,
    pub index: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct DisplayPage {
    pub stamp: PageStamp,
    pub geometry: PageSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ViewTarget {
    pub page: PageStamp,
    pub pixels: RasterSize,
}

#[derive(Clone, Copy)]
pub(crate) struct ViewRequest {
    pub target: ViewTarget,
    pub render_bounds: RasterSize,
}

#[derive(Default)]
pub(crate) struct ViewRaster {
    pub request: Option<ViewRequest>,
    pub debounce: Option<Task<()>>,
    pub image: Option<Arc<RenderImage>>,
    failure: Option<ViewFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewFailure {
    Memory,
    Render,
}

impl ViewRaster {
    pub fn target(&self) -> Option<ViewTarget> {
        self.request.map(|request| request.target)
    }

    pub fn pending(&self) -> Option<ViewRequest> {
        if self.debounce.is_none() && self.image.is_none() && self.failure.is_none() {
            self.request
        } else {
            None
        }
    }
}

#[derive(Default)]
pub(crate) struct ViewRasters(pub [ViewRaster; 2]);

impl ViewRasters {
    pub fn image(&self, target: ViewTarget) -> Option<Arc<RenderImage>> {
        self.0
            .iter()
            .filter(|view| view.target() == Some(target))
            .find_map(|view| view.image.clone())
    }

    pub fn reset(&mut self, index: usize) -> Option<Arc<RenderImage>> {
        let retired = std::mem::take(&mut self.0[index]).image?;
        let shared = self.0.iter().any(|view| {
            view.image
                .as_ref()
                .is_some_and(|image| Arc::ptr_eq(image, &retired))
        });
        (!shared).then_some(retired)
    }

    pub fn finish(
        &mut self,
        target: ViewTarget,
        result: Result<Arc<RenderImage>, PdfError>,
    ) -> bool {
        let mut changed = false;
        for view in &mut self.0 {
            if view.target() == Some(target) {
                match &result {
                    Ok(image) => {
                        view.image = Some(image.clone());
                        view.failure = None;
                        changed = true;
                    }
                    Err(error) => {
                        view.image = None;
                        view.failure = Some(if *error == PdfError::InsufficientMemory {
                            ViewFailure::Memory
                        } else {
                            ViewFailure::Render
                        });
                    }
                }
                view.debounce = None;
            }
        }
        changed
    }

    pub fn resume_memory_failures(&mut self) {
        for view in &mut self.0 {
            if view.failure == Some(ViewFailure::Memory) {
                view.failure = None;
            }
        }
    }
}

pub(crate) fn slide_canvas(
    model: Entity<Presentation>,
    viewport: SlideViewport,
    page: DisplayPage,
    fallback: Arc<RenderImage>,
    fit: SlideFit,
) -> impl gpui_kit::IntoElement {
    canvas(
        move |bounds, window, cx| {
            let scale = window.scale_factor();
            let Some(placement) = page_placement(bounds, scale, page.geometry, fit) else {
                clear_stale_viewport(&model, viewport, window, cx);
                let image_bounds = match fit {
                    SlideFit::WholePage => fitted_page(bounds, scale, raster_size(&fallback))?.0,
                    SlideFit::FitWidth => bounds,
                };
                return Some((image_bounds, fallback, None));
            };
            if placement
                .raster_size
                .width
                .max(placement.raster_size.height)
                > MAX_TEXTURE_EDGE
            {
                clear_stale_viewport(&model, viewport, window, cx);
                return Some((placement.bounds, fallback, None));
            }

            let target = ViewTarget {
                page: page.stamp,
                pixels: placement.raster_size,
            };
            let presentation = model.read(cx);
            let exact = presentation.viewport_image(target);
            if presentation.needs_viewport_request(viewport, target) {
                let request = ViewRequest {
                    target,
                    render_bounds: placement.render_bounds,
                };
                let model = model.clone();
                window.defer(cx, move |_, cx| {
                    model.update(cx, |model, cx| {
                        model.request_viewport(viewport, request, cx)
                    });
                });
            }
            Some((placement.bounds, fallback, exact))
        },
        |_, slide, window, _| {
            if let Some((image_bounds, fallback, exact)) = slide
                && !exact.is_some_and(|image| paint_page(image_bounds, image_bounds, image, window))
            {
                paint_page(image_bounds, image_bounds, fallback, window);
            }
        },
    )
    .size_full()
}

fn clear_stale_viewport(
    model: &Entity<Presentation>,
    viewport: SlideViewport,
    window: &mut gpui_kit::Window,
    cx: &mut gpui_kit::App,
) {
    if model.read(cx).has_viewport_request(viewport) {
        let model = model.clone();
        window.defer(cx, move |_, cx| {
            model.update(cx, |model, cx| model.clear_viewport(viewport, cx));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{PageStamp, PdfError, RasterSize, ViewRasters, ViewRequest, ViewTarget};
    use gpui_kit::RenderImage;
    use image::{Frame, RgbaImage};
    use std::sync::Arc;

    fn request(document: u64, generation: u64, index: usize, width: u32) -> ViewRequest {
        let pixels = RasterSize { width, height: 2 };
        ViewRequest {
            target: ViewTarget {
                page: PageStamp {
                    document,
                    generation,
                    index,
                },
                pixels,
            },
            render_bounds: pixels,
        }
    }

    fn image() -> Arc<RenderImage> {
        Arc::new(RenderImage::new([Frame::new(RgbaImage::new(4, 2))]))
    }

    #[test]
    fn matching_views_share_one_raster_until_the_last_view_releases_it() {
        let mut views = ViewRasters::default();
        let request = request(1, 0, 0, 4);
        views.0[0].request = Some(request);
        views.0[1].request = Some(request);
        let image = image();
        assert!(views.finish(request.target, Ok(image.clone())));
        assert!(Arc::ptr_eq(
            views.0[0].image.as_ref().unwrap(),
            views.0[1].image.as_ref().unwrap()
        ));
        assert!(views.reset(0).is_none());
        assert!(Arc::ptr_eq(&views.reset(1).unwrap(), &image));
    }

    #[test]
    fn stale_results_cannot_replace_new_page_or_resized_view() {
        let mut views = ViewRasters::default();
        let latest = request(2, 1, 2, 4);
        views.0[0].request = Some(latest);
        for stale in [
            request(1, 1, 2, 4),
            request(2, 0, 2, 4),
            request(2, 1, 1, 4),
            request(2, 1, 2, 3),
        ] {
            assert!(!views.finish(stale.target, Ok(image())));
        }
        assert!(views.0[0].pending().is_some());
        assert!(views.image(latest.target).is_none());
    }

    #[test]
    fn only_memory_failures_are_retried_when_rebuilding_the_cache() {
        let mut views = ViewRasters::default();
        let audience = request(1, 0, 0, 4);
        let current = request(1, 0, 0, 8);
        views.0[0].request = Some(audience);
        views.0[1].request = Some(current);
        assert!(!views.finish(audience.target, Err(PdfError::InsufficientMemory)));
        assert!(!views.finish(current.target, Err(PdfError::CannotRender)));
        assert!(views.image(audience.target).is_none());
        assert!(views.0[0].pending().is_none());
        views.resume_memory_failures();
        assert!(views.0[0].pending().is_some());
        assert!(views.0[1].pending().is_none());
    }
}
