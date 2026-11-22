use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::{AnyWindowHandle, Context, RenderImage, Task, WindowId};
use image::{Frame, RgbaImage};
use sysinfo::System;

use crate::{
    cache_policy::{audience_pages, physical_headroom},
    pdf::{PageSize, PdfError, PdfEvent, PdfWorker, Raster, RasterSize, RenderKey},
    settings::{CurrentSlideSettings, Settings, SlideListSettings, ThemePreference, TimerSettings},
    slide_view::raster_size,
    timer::{Timer, TimerSnapshot, TimerState},
    view_raster::{DisplayPage, PageStamp, SlideViewport, ViewRasters, ViewRequest, ViewTarget},
};

const THUMBNAIL_SIZE: RasterSize = RasterSize {
    width: 384,
    height: 288,
};
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
const VIEWPORT_RESIZE_DELAY: Duration = Duration::from_millis(100);
const AUDIENCE_SETTLE_DELAY: Duration = Duration::from_millis(250);
const CACHE_RELEASE_DELAY: Duration = Duration::from_millis(80);
const MEMORY_NOTICE_DURATION: Duration = Duration::from_secs(5);

struct Document {
    id: u64,
    path: PathBuf,
    pages: Vec<Option<PageSize>>,
    index: usize,
}

struct LoadRequest {
    id: u64,
    path: PathBuf,
    tried_password: bool,
    pages: Option<Vec<Option<PageSize>>>,
    reclaimed_cache: bool,
    reclaim_wait: Option<Task<()>>,
}

#[derive(Clone)]
pub struct PasswordPrompt {
    pub id: u64,
    pub path: PathBuf,
    pub wrong_password: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderPurpose {
    FirstPage,
    Preview,
    Thumbnail,
    Viewport(ViewTarget),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderJob {
    id: u64,
    generation: u64,
    key: RenderKey,
    purpose: RenderPurpose,
}

#[derive(Default)]
struct Cache {
    images: HashMap<usize, Arc<RenderImage>>,
    failed: HashSet<usize>,
    next_sequential: usize,
    paused: bool,
}

impl Cache {
    fn needs(&self, page: usize) -> bool {
        !self.images.contains_key(&page) && !self.failed.contains(&page)
    }

    fn next_page(&mut self, count: usize) -> Option<usize> {
        while self.next_sequential < count {
            if self.needs(self.next_sequential) {
                return Some(self.next_sequential);
            }
            self.next_sequential += 1;
        }
        None
    }

    fn release_except(&mut self, current: usize) -> impl Iterator<Item = Arc<RenderImage>> + '_ {
        self.next_sequential = 0;
        self.images
            .extract_if(move |&page, _| page != current)
            .map(|(_, image)| image)
    }
}

#[derive(Default)]
struct AudienceCache {
    images: HashMap<usize, Arc<RenderImage>>,
    wanted: Vec<usize>,
    wanted_set: HashSet<usize>,
    next_wanted: usize,
}

impl AudienceCache {
    fn select(&mut self, pages: Vec<usize>, preview: &Cache) {
        self.wanted_set = pages.iter().copied().collect();
        self.wanted = pages;
        self.next_wanted = 0;
        self.images.retain(|page, _| self.wanted_set.contains(page));
        for page in &self.wanted {
            if let Some(image) = preview.images.get(page) {
                self.images.entry(*page).or_insert_with(|| image.clone());
            }
        }
    }

    fn insert(&mut self, page: usize, image: &Arc<RenderImage>) {
        if self.wanted_set.contains(&page) {
            self.images.insert(page, image.clone());
        }
    }

    fn next_page(&mut self, preview: &Cache) -> Option<usize> {
        while let Some(&page) = self.wanted.get(self.next_wanted) {
            if preview.needs(page) {
                return Some(page);
            }
            self.next_wanted += 1;
        }
        None
    }

    fn release_preloaded(
        &mut self,
        current: usize,
        preview: &mut Cache,
        thumbnails: &mut Cache,
    ) -> Vec<Arc<RenderImage>> {
        self.images.retain(|&page, _| page == current);
        self.next_wanted = 0;
        preview
            .release_except(current)
            .chain(thumbnails.release_except(current))
            .collect()
    }
}

pub struct Presentation {
    settings: Settings,
    settings_dirty: bool,
    save_task: Option<Task<()>>,
    timer: Timer,
    timer_task: Option<Task<()>>,
    worker: PdfWorker,
    document: Option<Document>,
    next_id: u64,
    pending: Option<LoadRequest>,
    password_prompt: Option<PasswordPrompt>,
    notice: Option<String>,
    memory_notice: Option<String>,
    memory_notice_task: Option<Task<()>>,
    audience: Option<AnyWindowHandle>,
    raster_size: RasterSize,
    generation: u64,
    render_job: Option<RenderJob>,
    pending_page: Option<usize>,
    preview: Cache,
    thumbnails: Cache,
    audience_cache: AudienceCache,
    audience_recache: Option<Task<()>>,
    visible_thumbnails: Vec<usize>,
    previous_resolution_image: Option<Arc<RenderImage>>,
    viewports: ViewRasters,
    memory: System,
    memory_retry: Option<Task<()>>,
    warned_about_refresh: bool,
}

impl Presentation {
    pub fn new(settings: Settings, worker: PdfWorker, startup_notice: Option<String>) -> Self {
        Self {
            timer: Timer::new(settings.timer),
            settings,
            settings_dirty: false,
            save_task: None,
            timer_task: None,
            worker,
            document: None,
            next_id: 0,
            pending: None,
            password_prompt: None,
            notice: startup_notice,
            memory_notice: None,
            memory_notice_task: None,
            audience: None,
            raster_size: RasterSize::DEFAULT_DISPLAY,
            generation: 0,
            render_job: None,
            pending_page: None,
            preview: Cache::default(),
            thumbnails: Cache::default(),
            audience_cache: AudienceCache::default(),
            audience_recache: None,
            visible_thumbnails: Vec::new(),
            previous_resolution_image: None,
            viewports: ViewRasters::default(),
            memory: System::new(),
            memory_retry: None,
            warned_about_refresh: false,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.document
            .as_ref()
            .map(|document| document.path.as_path())
    }

    pub fn page_count(&self) -> usize {
        self.document
            .as_ref()
            .map_or(0, |document| document.pages.len())
    }

    pub fn document_id(&self) -> Option<u64> {
        self.document.as_ref().map(|document| document.id)
    }

    pub fn page_index(&self) -> usize {
        self.document.as_ref().map_or(0, |document| document.index)
    }

    pub fn presenter_page_index(&self) -> usize {
        self.pending_page.unwrap_or_else(|| self.page_index())
    }

    pub fn is_presenting(&self) -> bool {
        self.timer.is_presenting()
    }

    pub fn audience(&self) -> Option<AnyWindowHandle> {
        self.audience
    }

    pub fn loading(&self) -> bool {
        self.pending.is_some() && self.password_prompt.is_none()
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn set_notice(&mut self, notice: String, cx: &mut Context<Self>) {
        self.notice = Some(notice);
        cx.notify();
    }

    pub fn clear_notice(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        cx.notify();
    }

    pub fn memory_notice(&self) -> Option<&str> {
        self.memory_notice.as_deref()
    }

    fn set_memory_notice(&mut self, notice: String, cx: &mut Context<Self>) {
        if self.memory_notice.as_ref() == Some(&notice) {
            return;
        }
        self.memory_notice = Some(notice);
        self.memory_notice_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MEMORY_NOTICE_DURATION).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(task) = this.memory_notice_task.take() {
                    task.detach();
                }
                this.memory_notice = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub fn password_prompt(&self) -> Option<&PasswordPrompt> {
        self.password_prompt.as_ref()
    }

    pub fn open_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.next_id += 1;
        self.notice = None;
        self.memory_notice = None;
        self.memory_notice_task = None;
        self.password_prompt = None;
        self.pending = Some(LoadRequest {
            id: self.next_id,
            path: path.clone(),
            tried_password: false,
            pages: None,
            reclaimed_cache: false,
            reclaim_wait: None,
        });
        self.worker.open(self.next_id, path, None);
        cx.notify();
    }

    pub fn submit_password(&mut self, id: u64, password: String, cx: &mut Context<Self>) {
        if self
            .password_prompt
            .as_ref()
            .is_none_or(|prompt| prompt.id != id)
        {
            return;
        }
        if let Some(request) = self.pending.as_mut().filter(|request| request.id == id) {
            request.tried_password = true;
            self.password_prompt = None;
            self.worker.open(id, request.path.clone(), Some(password));
            cx.notify();
        }
    }

    pub fn cancel_password(&mut self, id: u64, cx: &mut Context<Self>) {
        if self
            .password_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.id == id)
        {
            self.pending = None;
            self.password_prompt = None;
            self.schedule_render(cx);
            cx.notify();
        }
    }

    pub fn handle_pdf_event(&mut self, event: PdfEvent, cx: &mut Context<Self>) {
        match event {
            PdfEvent::Opened { id, pages } => {
                if let Some(request) = self.pending.as_mut().filter(|request| request.id == id) {
                    request.pages = Some(pages);
                }
            }
            PdfEvent::OpenFailed { id, error } => {
                if let Some(request) = self.pending.as_ref().filter(|request| request.id == id) {
                    if matches!(error, PdfError::PasswordRequired) {
                        self.password_prompt = Some(PasswordPrompt {
                            id,
                            path: request.path.clone(),
                            wrong_password: request.tried_password,
                        });
                        cx.notify();
                    } else {
                        self.reject_open(id, &error, cx);
                    }
                }
            }
            PdfEvent::Rendered {
                id,
                generation,
                key,
                candidate,
                raster,
            } => {
                if let Some(job) = self.render_job.take_if(|job| {
                    job.id == id
                        && job.generation == generation
                        && job.key == key
                        && (job.purpose == RenderPurpose::FirstPage) == candidate
                }) {
                    self.finish_render(job, raster, cx);
                }
            }
        }
        self.schedule_render(cx);
    }

    fn reject_open(&mut self, id: u64, error: &PdfError, cx: &mut Context<Self>) {
        self.worker.discard(id);
        if self
            .pending
            .take()
            .is_some_and(|request| request.reclaimed_cache)
        {
            self.restart_paused_caches();
        }
        self.password_prompt = None;
        if *error == PdfError::InsufficientMemory {
            self.set_memory_notice("PDFを開くための空きメモリが足りない。".into(), cx);
        } else {
            self.set_notice(format!("PDFを読み込めなかった。{error}"), cx);
        }
    }

    fn reclaim_for_open(&mut self, id: u64, cx: &mut Context<Self>) -> bool {
        if self
            .pending
            .as_ref()
            .is_none_or(|request| request.id != id || request.reclaimed_cache)
            || self.document.is_none()
        {
            return false;
        }
        let current = self.page_index();
        let retired =
            self.audience_cache
                .release_preloaded(current, &mut self.preview, &mut self.thumbnails);
        if retired.is_empty() {
            return false;
        }
        Self::retire_images(retired, cx);
        let request = self.pending.as_mut().expect("validated open request");
        request.reclaimed_cache = true;
        request.reclaim_wait = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CACHE_RELEASE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(request) = this.pending.as_mut().filter(|request| request.id == id) {
                    if let Some(task) = request.reclaim_wait.take() {
                        task.detach();
                    }
                    this.schedule_render(cx);
                }
            });
        }));
        cx.notify();
        true
    }

    fn accept_open(&mut self, id: u64, image: Arc<RenderImage>, cx: &mut Context<Self>) {
        let request = self.pending.take().expect("matched pending PDF");
        let pages = request.pages.expect("inspected pending PDF");
        self.worker.accept(id);
        self.end_presentation(cx);
        self.clear_viewports(cx);
        self.timer.reset();
        self.audience_cache = AudienceCache::default();
        self.audience_recache = None;
        let mut retired: Vec<_> = std::mem::take(&mut self.preview)
            .images
            .into_values()
            .collect();
        retired.extend(std::mem::take(&mut self.thumbnails).images.into_values());
        retired.extend(self.previous_resolution_image.take());
        Self::retire_images(retired, cx);
        self.memory_retry = None;
        self.warned_about_refresh = false;
        self.pending_page = None;
        self.visible_thumbnails.clear();
        self.password_prompt = None;
        self.settings.record_recent(&request.path);
        self.document = Some(Document {
            id,
            path: request.path,
            pages,
            index: 0,
        });
        self.preview.images.insert(0, image);
        self.update_audience_cache();
        self.schedule_save(cx);
        cx.notify();
    }

    fn finish_render(
        &mut self,
        job: RenderJob,
        raster: Result<Raster, PdfError>,
        cx: &mut Context<Self>,
    ) {
        let target_document = if job.purpose == RenderPurpose::FirstPage {
            self.pending.as_ref().map(|request| request.id)
        } else {
            self.document.as_ref().map(|document| document.id)
        };
        if job.generation != self.generation || target_document != Some(job.id) {
            return;
        }
        if let RenderPurpose::Viewport(target) = job.purpose
            && !self
                .viewports
                .0
                .iter()
                .any(|view| view.target() == Some(target))
        {
            return;
        }

        let image = raster.and_then(|raster| {
            if let RenderPurpose::Viewport(target) = job.purpose
                && (raster.width != target.pixels.width || raster.height != target.pixels.height)
            {
                return Err(PdfError::CannotRender);
            }
            let frame = RgbaImage::from_raw(raster.width, raster.height, raster.bgra)
                .ok_or(PdfError::CannotRender)?;
            Ok(Arc::new(RenderImage::new([Frame::new(frame)])))
        });
        let image = match image {
            Ok(image) => image,
            Err(error) => {
                self.render_failed(job, error, cx);
                return;
            }
        };

        let page = job.key.page;
        match job.purpose {
            RenderPurpose::FirstPage => self.accept_open(job.id, image, cx),
            RenderPurpose::Preview => {
                self.audience_cache.insert(page, &image);
                self.preview.images.insert(page, image);
                if self.pending_page == Some(page) {
                    self.commit_page(page, cx);
                } else if page == self.page_index() {
                    Self::retire_images(self.previous_resolution_image.take(), cx);
                    self.warned_about_refresh = false;
                }
            }
            RenderPurpose::Thumbnail => {
                self.thumbnails.images.insert(page, image);
            }
            RenderPurpose::Viewport(target) => {
                self.viewports.finish(target, Ok(image));
            }
        }
        cx.notify();
    }

    fn render_failed(&mut self, job: RenderJob, error: PdfError, cx: &mut Context<Self>) {
        if let RenderPurpose::Viewport(target) = job.purpose {
            if error == PdfError::InsufficientMemory {
                self.retry_when_memory_is_available(cx);
            }
            self.viewports.finish(target, Err(error));
            return;
        }
        if job.purpose == RenderPurpose::FirstPage {
            if error == PdfError::InsufficientMemory && self.reclaim_for_open(job.id, cx) {
                return;
            }
            self.reject_open(job.id, &error, cx);
            return;
        }
        let page = job.key.page;
        let cache = if job.purpose == RenderPurpose::Thumbnail {
            &mut self.thumbnails
        } else {
            &mut self.preview
        };
        if error == PdfError::InsufficientMemory {
            cache.paused = true;
            self.retry_when_memory_is_available(cx);
        } else {
            cache.failed.insert(page);
            cx.notify();
        }

        if job.purpose != RenderPurpose::Preview {
            return;
        }
        let requested = self.pending_page == Some(page);
        if requested {
            self.pending_page = None;
        }
        if requested || (page == self.page_index() && !self.warned_about_refresh) {
            self.warned_about_refresh = page == self.page_index();
            if error == PdfError::InsufficientMemory {
                self.set_memory_notice(
                    format!("ページ {} の描画に必要な空きメモリが足りない。", page + 1),
                    cx,
                );
            } else {
                self.set_notice(
                    format!("ページ {} を表示できなかった。{error}", page + 1),
                    cx,
                );
            }
        }
    }

    pub fn navigate_to(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.page_count() || index == self.presenter_page_index() {
            return;
        }
        if index == self.page_index() {
            self.pending_page = None;
        } else if self.preview.images.contains_key(&index) {
            self.commit_page(index, cx);
        } else if self.page_error(index) {
            self.pending_page = None;
            self.set_notice(format!("ページ {} を描画できなかった。", index + 1), cx);
        } else {
            self.pending_page = Some(index);
        }
        cx.notify();
        self.schedule_render(cx);
    }

    fn commit_page(&mut self, index: usize, cx: &mut Context<Self>) {
        self.document.as_mut().expect("open document").index = index;
        self.pending_page = None;
        self.warned_about_refresh = false;
        self.clear_viewports(cx);
        Self::retire_images(self.previous_resolution_image.take(), cx);
        let state_before = self.timer_snapshot().state;
        self.timer.page_changed(Instant::now());
        if state_before != self.timer_snapshot().state {
            self.refresh_timer_task(cx);
        }
        self.schedule_audience_recache(cx);
    }

    pub fn navigate_relative(&mut self, delta: i32, cx: &mut Context<Self>) {
        let from = self.presenter_page_index();
        self.navigate_to(from.saturating_add_signed(delta as isize), cx);
    }

    pub fn show_audience(&mut self, handle: AnyWindowHandle, cx: &mut Context<Self>) {
        if let Some(previous) = self.audience.replace(handle) {
            Self::retire_images(self.viewports.reset(0), cx);
            Self::close_window(previous, cx);
        } else {
            self.timer.begin_presentation();
        }
        cx.notify();
    }

    pub fn audience_closed(&mut self, id: WindowId, cx: &mut Context<Self>) {
        if self.audience.is_some_and(|handle| handle.window_id() == id) {
            self.audience = None;
            self.stop_projection(cx);
        }
    }

    pub fn end_presentation(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.audience.take() {
            Self::close_window(handle, cx);
            self.stop_projection(cx);
        }
    }

    fn close_window(handle: AnyWindowHandle, cx: &mut Context<Self>) {
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        });
    }

    fn stop_projection(&mut self, cx: &mut Context<Self>) {
        Self::retire_images(self.viewports.reset(0), cx);
        self.timer.end_presentation(Instant::now());
        self.timer_task = None;
        cx.notify();
    }

    pub fn image(&self, page: usize) -> Option<Arc<RenderImage>> {
        self.preview.images.get(&page).cloned().or_else(|| {
            if page == self.page_index() {
                self.previous_resolution_image.clone()
            } else {
                None
            }
        })
    }

    pub fn audience_image(&self) -> Option<Arc<RenderImage>> {
        let page = self.page_index();
        self.audience_cache
            .images
            .get(&page)
            .cloned()
            .or_else(|| self.image(page))
    }

    pub(crate) fn audience_view(&self) -> Option<(ViewTarget, Option<Arc<RenderImage>>)> {
        let viewport =
            &self.viewports.0[SlideViewport::Audience(self.audience?.window_id()).index()];
        Some((viewport.target()?, viewport.image.clone()))
    }

    pub fn presenter_image(&self) -> Option<Arc<RenderImage>> {
        let page = self.presenter_page_index();
        self.image(page).or_else(|| self.thumbnail(page))
    }

    pub(crate) fn display_page(&self) -> Option<DisplayPage> {
        self.display_page_at(self.page_index())
    }

    pub(crate) fn presenter_display_page(&self) -> Option<DisplayPage> {
        self.display_page_at(self.presenter_page_index())
    }

    fn display_page_at(&self, index: usize) -> Option<DisplayPage> {
        let document = self.document.as_ref()?;
        let geometry = document.pages.get(index).copied().flatten()?;
        Some(DisplayPage {
            stamp: PageStamp {
                document: document.id,
                generation: self.generation,
                index,
            },
            geometry,
        })
    }

    fn viewport_is_active(&self, viewport: SlideViewport) -> bool {
        match viewport {
            SlideViewport::Audience(id) => {
                self.audience.is_some_and(|window| window.window_id() == id)
            }
            SlideViewport::CurrentSlide => true,
        }
    }

    pub(crate) fn viewport_image(&self, target: ViewTarget) -> Option<Arc<RenderImage>> {
        self.viewports.image(target)
    }

    pub(crate) fn has_viewport_request(&self, viewport: SlideViewport) -> bool {
        self.viewport_is_active(viewport) && self.viewports.0[viewport.index()].request.is_some()
    }

    pub(crate) fn needs_viewport_request(
        &self,
        viewport: SlideViewport,
        target: ViewTarget,
    ) -> bool {
        self.pending_page.is_none()
            && self.viewport_is_active(viewport)
            && self
                .display_page()
                .is_some_and(|page| page.stamp == target.page)
            && self.viewports.0[viewport.index()].target() != Some(target)
    }

    pub(crate) fn request_viewport(
        &mut self,
        viewport: SlideViewport,
        request: ViewRequest,
        cx: &mut Context<Self>,
    ) {
        if !self.needs_viewport_request(viewport, request.target) {
            return;
        }
        let index = viewport.index();
        Self::retire_images(self.viewports.reset(index), cx);
        let shared = self.viewports.image(request.target);
        let cached = self.image(request.target.page.index);
        let view = &mut self.viewports.0[index];
        view.request = Some(request);
        view.image = shared;
        if view.image.is_some()
            || cached
                .as_ref()
                .is_some_and(|image| raster_size(image) == request.target.pixels)
        {
            return;
        }

        let delay = match viewport {
            SlideViewport::Audience(_) => AUDIENCE_SETTLE_DELAY,
            SlideViewport::CurrentSlide => VIEWPORT_RESIZE_DELAY,
        };
        view.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                let view = &mut this.viewports.0[index];
                if view.target() == Some(request.target) {
                    if let Some(task) = view.debounce.take() {
                        task.detach();
                    }
                    this.schedule_render(cx);
                }
            });
        }));
    }

    pub(crate) fn clear_viewport(&mut self, viewport: SlideViewport, cx: &mut Context<Self>) {
        if self.viewport_is_active(viewport) {
            Self::retire_images(self.viewports.reset(viewport.index()), cx);
        }
    }

    fn clear_viewports(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.viewports.0.len() {
            Self::retire_images(self.viewports.reset(index), cx);
        }
    }

    pub fn thumbnail(&self, page: usize) -> Option<Arc<RenderImage>> {
        self.thumbnails.images.get(&page).cloned()
    }

    pub fn page_error(&self, page: usize) -> bool {
        self.preview.failed.contains(&page)
            || self
                .document
                .as_ref()
                .is_some_and(|document| document.pages.get(page) == Some(&None))
    }

    pub fn thumbnail_error(&self, page: usize) -> bool {
        self.thumbnails.failed.contains(&page)
            || self
                .document
                .as_ref()
                .is_some_and(|document| document.pages.get(page) == Some(&None))
    }

    pub fn set_visible_thumbnails(&mut self, pages: Vec<usize>, cx: &mut Context<Self>) {
        if pages != self.visible_thumbnails {
            self.visible_thumbnails = pages;
            self.schedule_render(cx);
        }
    }

    pub fn set_raster_size(&mut self, size: RasterSize, cx: &mut Context<Self>) {
        if self.raster_size == size {
            return;
        }
        self.raster_size = size;
        self.generation = self.generation.wrapping_add(1);
        self.clear_viewports(cx);
        self.warned_about_refresh = false;
        self.audience_cache = AudienceCache::default();
        let current = self.page_index();
        let mut old = std::mem::take(&mut self.preview);
        if let Some(image) = old.images.remove(&current) {
            Self::retire_images(self.previous_resolution_image.replace(image), cx);
        }
        Self::retire_images(old.images.into_values(), cx);
        self.update_audience_cache();
        self.schedule_render(cx);
        cx.notify();
    }

    fn retire_images(images: impl IntoIterator<Item = Arc<RenderImage>>, cx: &mut Context<Self>) {
        let images: Vec<_> = images.into_iter().collect();
        if !images.is_empty() {
            cx.defer(move |cx| {
                for image in images {
                    cx.drop_image(image, None);
                }
            });
        }
    }

    fn update_audience_cache(&mut self) {
        if let Some(document) = &self.document {
            let bytes = (self.settings.audience_cache_mib as usize).saturating_mul(1024 * 1024);
            let pages = audience_pages(document.index, &document.pages, self.raster_size, bytes);
            self.audience_cache.select(pages, &self.preview);
        }
    }

    fn schedule_audience_recache(&mut self, cx: &mut Context<Self>) {
        self.audience_recache = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AUDIENCE_SETTLE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(task) = this.audience_recache.take() {
                    task.detach();
                }
                this.update_audience_cache();
                this.schedule_render(cx);
            });
        }));
    }

    fn schedule_render(&mut self, cx: &mut Context<Self>) {
        while self.render_job.is_none() {
            let Some((job, page)) = self.next_render() else {
                return;
            };
            match self.start_render(job, page) {
                Ok(()) => self.render_job = Some(job),
                Err(error) => self.render_failed(job, error, cx),
            }
        }
    }

    fn next_render(&mut self) -> Option<(RenderJob, Option<PageSize>)> {
        if let Some(request) = &self.pending
            && let Some(pages) = &request.pages
        {
            if request.reclaim_wait.is_some() {
                return None;
            }
            let job = RenderJob {
                id: request.id,
                generation: self.generation,
                key: RenderKey::new(0, self.raster_size),
                purpose: RenderPurpose::FirstPage,
            };
            return Some((job, pages.first().copied().flatten()));
        }

        let document = self.document.as_ref()?;
        let requested = self.pending_page;
        let page = requested.unwrap_or(document.index);
        let (page, purpose, size) =
            if (requested.is_some() || !self.preview.paused) && self.preview.needs(page) {
                (page, RenderPurpose::Preview, self.raster_size)
            } else {
                if self.loading() {
                    return None;
                }
                if let Some(job) = self.next_viewport_render() {
                    return Some(job);
                }
                let count = document.pages.len();
                self.next_preload(count)?
            };
        let document = self.document.as_ref()?;
        let job = RenderJob {
            id: document.id,
            generation: self.generation,
            key: RenderKey::new(page, size),
            purpose,
        };
        Some((job, document.pages.get(page).copied().flatten()))
    }

    fn next_viewport_render(&self) -> Option<(RenderJob, Option<PageSize>)> {
        if self.pending_page.is_some() {
            return None;
        }
        let page = self.display_page()?;
        let cached = self.image(page.stamp.index).as_deref().map(raster_size);
        let request = self
            .viewports
            .0
            .iter()
            .rev()
            .filter_map(|view| view.pending())
            .find(|request| {
                request.target.page == page.stamp && Some(request.target.pixels) != cached
            })?;
        Some((
            RenderJob {
                id: page.stamp.document,
                generation: page.stamp.generation,
                key: RenderKey::new(page.stamp.index, request.render_bounds),
                purpose: RenderPurpose::Viewport(request.target),
            },
            Some(page.geometry),
        ))
    }

    fn next_preload(&mut self, count: usize) -> Option<(usize, RenderPurpose, RasterSize)> {
        if !self.thumbnails.paused
            && let Some(page) = self
                .visible_thumbnails
                .iter()
                .copied()
                .find(|&page| page < count && self.thumbnails.needs(page))
        {
            return Some((page, RenderPurpose::Thumbnail, THUMBNAIL_SIZE));
        }
        if !self.preview.paused
            && self.audience_recache.is_none()
            && let Some(page) = self.audience_cache.next_page(&self.preview)
        {
            return Some((page, RenderPurpose::Preview, self.raster_size));
        }
        if !self.thumbnails.paused
            && let Some(page) = self.thumbnails.next_page(count)
        {
            return Some((page, RenderPurpose::Thumbnail, THUMBNAIL_SIZE));
        }
        if !self.preview.paused
            && let Some(page) = self.preview.next_page(count)
        {
            return Some((page, RenderPurpose::Preview, self.raster_size));
        }
        None
    }

    fn start_render(&mut self, job: RenderJob, page: Option<PageSize>) -> Result<(), PdfError> {
        let bounds = RasterSize {
            width: job.key.max_width,
            height: job.key.max_height,
        };
        let required = page
            .and_then(|page| page.fit(bounds))
            .and_then(RasterSize::render_bytes)
            .ok_or(PdfError::CannotRender)?;
        self.memory.refresh_memory();
        if required > physical_headroom(self.memory.total_memory(), self.memory.available_memory())
        {
            return Err(PdfError::InsufficientMemory);
        }
        if job.purpose == RenderPurpose::FirstPage {
            self.worker
                .render_candidate(job.id, job.generation, job.key);
        } else {
            self.worker.render(job.id, job.generation, job.key);
        }
        Ok(())
    }

    fn retry_when_memory_is_available(&mut self, cx: &mut Context<Self>) {
        if self.memory_retry.is_none() {
            self.memory_retry = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(RETRY_INTERVAL).await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(task) = this.memory_retry.take() {
                        task.detach();
                    }
                    this.restart_paused_caches();
                    this.schedule_render(cx);
                });
            }));
        }
    }

    fn restart_paused_caches(&mut self) {
        self.memory_retry = None;
        self.preview.paused = false;
        self.thumbnails.paused = false;
        self.viewports.resume_memory_failures();
    }

    pub fn set_preferences(
        &mut self,
        theme: ThemePreference,
        audience_cache_mib: u64,
        cx: &mut Context<Self>,
    ) {
        let cache_changed = self.settings.audience_cache_mib != audience_cache_mib;
        if self.settings.theme == theme && !cache_changed {
            return;
        }
        self.settings.theme = theme;
        self.settings.audience_cache_mib = audience_cache_mib;
        if cache_changed {
            self.audience_recache = None;
            self.restart_paused_caches();
            self.update_audience_cache();
            self.schedule_render(cx);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    pub fn timer_snapshot(&self) -> TimerSnapshot {
        self.timer.snapshot(Instant::now())
    }

    pub fn timer_start(&mut self, cx: &mut Context<Self>) {
        if self.timer_snapshot().state != TimerState::Running {
            self.timer.start(Instant::now());
            self.refresh_timer_task(cx);
            cx.notify();
        }
    }

    pub fn timer_pause(&mut self, cx: &mut Context<Self>) {
        self.timer.pause(Instant::now());
        self.timer_task = None;
        cx.notify();
    }

    pub fn timer_reset(&mut self, cx: &mut Context<Self>) {
        self.timer.reset();
        self.timer_task = None;
        cx.notify();
    }

    fn refresh_timer_task(&mut self, cx: &mut Context<Self>) {
        let initial = self.timer_snapshot();
        self.timer_task = (initial.state == TimerState::Running).then(|| {
            cx.spawn(async move |this, cx| {
                let mut previous = initial;
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    let Ok(now) = this.update(cx, |this, cx| {
                        let now = this.timer_snapshot();
                        if now != previous {
                            cx.notify();
                        }
                        now
                    }) else {
                        break;
                    };
                    if now.state != TimerState::Running {
                        break;
                    }
                    previous = now;
                }
            })
        });
    }

    pub fn set_timer_settings(&mut self, settings: TimerSettings, cx: &mut Context<Self>) {
        if self.settings.timer != settings {
            self.settings.timer = settings;
            self.timer.set_settings(settings);
            self.timer_task = None;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    pub fn set_slide_list_settings(&mut self, settings: SlideListSettings, cx: &mut Context<Self>) {
        if self.settings.slides != settings {
            self.settings.slides = settings;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    pub fn set_current_slide_settings(
        &mut self,
        settings: CurrentSlideSettings,
        cx: &mut Context<Self>,
    ) {
        if self.settings.current_slide != settings {
            self.settings.current_slide = settings;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    pub fn set_dock_layout(&mut self, layout: serde_json::Value, cx: &mut Context<Self>) {
        if self.settings.dock.as_ref() != Some(&layout) {
            self.settings.dock = Some(layout);
            self.schedule_save(cx);
        }
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.settings_dirty = true;
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(240))
                .await;
            let _ = this.update(cx, |this, cx| this.save_settings(cx));
        }));
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings_dirty {
            match self.settings.save() {
                Ok(()) => self.settings_dirty = false,
                Err(error) => self.set_notice(format!("設定を保存できなかった。{error}"), cx),
            }
        }
    }

    pub fn flush_settings(&mut self, cx: &mut Context<Self>) {
        self.save_task = None;
        self.save_settings(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{Arc, AudienceCache, Cache, Frame, RenderImage, RgbaImage};

    fn image() -> Arc<RenderImage> {
        Arc::new(RenderImage::new([Frame::new(RgbaImage::new(1, 1))]))
    }

    #[test]
    fn reclaim_releases_shared_preloads_but_keeps_the_displayed_page() {
        let mut preview = Cache::default();
        let mut thumbnails = Cache::default();
        let mut audience = AudienceCache::default();
        let current = image();
        let old = image();
        preview
            .images
            .extend([(0, old.clone()), (1, current.clone())]);
        thumbnails.images.extend([(0, image()), (1, image())]);
        preview.next_sequential = 2;
        thumbnails.next_sequential = 2;
        audience.select(vec![1, 0], &preview);
        assert_eq!(Arc::strong_count(&old), 3);

        let retired = audience.release_preloaded(1, &mut preview, &mut thumbnails);
        assert_eq!(retired.len(), 2);
        assert!(Arc::ptr_eq(&preview.images[&1], &current));
        assert!(Arc::ptr_eq(&audience.images[&1], &current));
        assert_eq!(thumbnails.images.len(), 1);
        drop(retired);
        assert_eq!(Arc::strong_count(&old), 1);
        assert_eq!(preview.next_page(2), Some(0));
        assert_eq!(thumbnails.next_page(2), Some(0));
        assert_eq!(audience.next_page(&preview), Some(0));
    }

    #[test]
    fn rebuilding_a_smaller_then_larger_audience_cache_reuses_resident_images() {
        let mut preview = Cache::default();
        let shared = image();
        preview.images.insert(1, shared.clone());
        let mut audience = AudienceCache::default();
        audience.select(vec![1, 3], &preview);
        assert_eq!(audience.next_page(&preview), Some(3));
        audience.select(vec![], &preview);
        assert!(audience.images.is_empty());
        assert_eq!(audience.next_page(&preview), None);
        audience.select(vec![1, 2], &preview);
        assert!(Arc::ptr_eq(&audience.images[&1], &shared));
        assert_eq!(audience.next_page(&preview), Some(2));
    }
}
