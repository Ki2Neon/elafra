use crate::pdf::{PageSize, RasterSize};

/// Picks the audience cache entries in navigation order, charging their fitted BGRA sizes.
pub(crate) fn audience_pages(
    current: usize,
    pages: &[Option<PageSize>],
    bounds: RasterSize,
    budget: usize,
) -> Vec<usize> {
    if current >= pages.len() || budget == 0 {
        return Vec::new();
    }

    let last = pages.len() - 1;
    let reach = current.max(last - current);
    let nearest = std::iter::once(current).chain((1..=reach).flat_map(|distance| {
        [
            current.checked_add(distance).filter(|&page| page <= last),
            current.checked_sub(distance),
        ]
        .into_iter()
        .flatten()
    }));

    let mut remaining = budget;
    let mut selected = Vec::new();
    for page in nearest {
        if let Some(bytes) = pages[page]
            .as_ref()
            .and_then(|geometry| geometry.fit(bounds))
            .and_then(|raster| raster.byte_len())
            && bytes <= remaining
        {
            selected.push(page);
            remaining -= bytes;
        }
    }
    selected
}

/// Returns the additional allocation allowed before physical memory usage reaches 80%.
pub(crate) fn physical_headroom(total: u64, available: u64) -> usize {
    let ceiling = (u128::from(total) * 4 / 5) as u64;
    let used = total.saturating_sub(available);
    usize::try_from(ceiling.saturating_sub(used)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: RasterSize = RasterSize {
        width: 8,
        height: 8,
    };
    const SQUARE: Option<PageSize> = Some(PageSize {
        width: 8.0,
        height: 8.0,
    });
    const WIDE: Option<PageSize> = Some(PageSize {
        width: 16.0,
        height: 4.0,
    });
    const TALL: Option<PageSize> = Some(PageSize {
        width: 4.0,
        height: 16.0,
    });

    #[test]
    fn includes_every_page_when_their_fitted_rasters_fill_the_budget() {
        let pages = [WIDE, SQUARE, TALL, WIDE, SQUARE];
        let budget = 2 * 256 + 3 * 64;
        assert_eq!(audience_pages(2, &pages, BOUNDS, budget), [2, 3, 1, 4, 0]);
        assert_eq!(audience_pages(0, &pages, BOUNDS, budget), [0, 1, 2, 3, 4]);
        assert_eq!(audience_pages(4, &pages, BOUNDS, budget), [4, 3, 2, 1, 0]);
    }

    #[test]
    fn skips_unknown_or_oversized_pages_and_refills_after_navigation() {
        let pages = [WIDE, SQUARE, SQUARE, None, TALL, WIDE];
        assert_eq!(audience_pages(2, &pages, BOUNDS, 384), [2, 4, 0]);
        assert_eq!(audience_pages(4, &pages, BOUNDS, 384), [4, 5, 2]);
        assert_eq!(audience_pages(2, &pages, BOUNDS, 128), [4, 0]);
        assert!(audience_pages(2, &pages, BOUNDS, 0).is_empty());
        assert!(audience_pages(pages.len(), &pages, BOUNDS, 384).is_empty());
        assert!(audience_pages(0, &[], BOUNDS, 384).is_empty());
    }

    #[test]
    fn default_budget_counts_actual_4k_and_8k_pixels() {
        let pages = [Some(PageSize {
            width: 16.0,
            height: 9.0,
        }); 5];
        let budget = 128 * 1024 * 1024;
        let four_k = RasterSize {
            width: 3840,
            height: 2160,
        };
        let eight_k = RasterSize {
            width: 7680,
            height: 4320,
        };
        assert_eq!(audience_pages(2, &pages, four_k, budget), [2, 3, 1, 4]);
        assert_eq!(audience_pages(2, &pages, eight_k, budget), [2]);
    }

    #[test]
    fn physical_headroom_is_exact_and_saturates_at_boundaries() {
        assert_eq!(physical_headroom(1_000, 300), 100);
        assert_eq!(physical_headroom(1_001, 202), 1);
        assert_eq!(physical_headroom(1_001, 201), 0);
        assert_eq!(physical_headroom(1_000, 199), 0);
        assert_eq!(physical_headroom(100, 200), 80);
        assert_eq!(physical_headroom(0, u64::MAX), 0);
        assert_eq!(
            physical_headroom(u64::MAX, u64::MAX),
            usize::try_from(14_757_395_258_967_641_292_u64).unwrap_or(usize::MAX)
        );
    }
}
