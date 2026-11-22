use pdfium_render::prelude::{
    PdfBitmap, PdfBitmapFormat, PdfDocument, PdfRenderConfig, Pdfium, PdfiumError,
    PdfiumInternalError,
};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RasterSize {
    pub width: u32,
    pub height: u32,
}

impl RasterSize {
    pub const DEFAULT_DISPLAY: Self = Self {
        width: 1920,
        height: 1080,
    };

    pub fn byte_len(self) -> Option<usize> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let bytes = (self.width as usize)
            .checked_mul(self.height as usize)?
            .checked_mul(4)?;
        // PdfBitmap::from_bytes uses an i32 stride and buffer size.
        (bytes <= i32::MAX as usize).then_some(bytes)
    }

    pub fn render_bytes(self) -> Option<usize> {
        // The returned buffer also backs the PDFium bitmap. Allow space for
        // an additional bitmap-equivalent of temporary rendering data.
        self.byte_len()?.checked_mul(2)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSize {
    pub width: f64,
    pub height: f64,
}

impl PageSize {
    pub fn fit(self, bounds: RasterSize) -> Option<RasterSize> {
        if !self.is_valid() || bounds.width == 0 || bounds.height == 0 {
            return None;
        }
        let scale =
            (f64::from(bounds.width) / self.width).min(f64::from(bounds.height) / self.height);
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let size = RasterSize {
            width: (self.width * scale)
                .round()
                .clamp(1.0, f64::from(bounds.width)) as u32,
            height: (self.height * scale)
                .round()
                .clamp(1.0, f64::from(bounds.height)) as u32,
        };
        size.byte_len()?;
        Some(size)
    }

    fn is_valid(self) -> bool {
        self.width.is_finite() && self.height.is_finite() && self.width > 0.0 && self.height > 0.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RenderKey {
    pub page: usize,
    pub max_width: u32,
    pub max_height: u32,
}

impl RenderKey {
    pub fn new(page: usize, bounds: RasterSize) -> Self {
        Self {
            page,
            max_width: bounds.width,
            max_height: bounds.height,
        }
    }

    fn bounds(self) -> RasterSize {
        RasterSize {
            width: self.max_width,
            height: self.max_height,
        }
    }
}

pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PdfError {
    PasswordRequired,
    LibraryUnavailable(String),
    CannotRead(String),
    InvalidPdf,
    EmptyPdf,
    CannotRender,
    InsufficientMemory,
}

impl fmt::Display for PdfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PasswordRequired => formatter.write_str("PDFを開くには正しいパスワードが必要"),
            Self::LibraryUnavailable(reason) => {
                write!(formatter, "PDFiumを読み込めない（{reason}）")
            }
            Self::CannotRead(reason) => write!(formatter, "ファイルを読み込めない（{reason}）"),
            Self::InvalidPdf => formatter.write_str("PDFの形式が無効、または非対応"),
            Self::EmptyPdf => formatter.write_str("PDFにページがない"),
            Self::CannotRender => formatter.write_str("PDFのページを描画できない"),
            Self::InsufficientMemory => formatter.write_str("PDFの描画に必要なメモリが足りない"),
        }
    }
}

pub enum PdfEvent {
    Opened {
        id: u64,
        pages: Vec<Option<PageSize>>,
    },
    OpenFailed {
        id: u64,
        error: PdfError,
    },
    Rendered {
        id: u64,
        generation: u64,
        key: RenderKey,
        candidate: bool,
        raster: Result<Raster, PdfError>,
    },
}

pub struct PdfWorker {
    mailbox: Arc<Mailbox>,
    events: async_channel::Receiver<PdfEvent>,
}

impl PdfWorker {
    pub fn start() -> (Self, async_channel::Receiver<PdfEvent>) {
        let mailbox = Arc::new(Mailbox::default());
        let worker_mailbox = Arc::clone(&mailbox);
        let (sender, events) = async_channel::bounded(1);
        thread::spawn(move || run(&worker_mailbox, &sender));

        (
            Self {
                mailbox,
                events: events.clone(),
            },
            events,
        )
    }

    pub fn open(&self, id: u64, path: PathBuf, password: Option<String>) {
        self.mailbox.change(|pending| {
            pending.open = Some(OpenRequest { id, path, password });
        });
    }

    /// Promotes the document staged by `Opened` after the UI accepts it.
    pub fn accept(&self, id: u64) {
        self.mailbox.change(|pending| pending.accept = Some(id));
    }

    pub fn discard(&self, id: u64) {
        self.mailbox.change(|pending| pending.discard = Some(id));
    }

    pub fn render_candidate(&self, id: u64, generation: u64, key: RenderKey) {
        self.request_render(id, generation, key, true);
    }

    /// The scheduler must await the result before requesting another raster.
    pub fn render(&self, id: u64, generation: u64, key: RenderKey) {
        self.request_render(id, generation, key, false);
    }

    fn request_render(&self, id: u64, generation: u64, key: RenderKey, candidate: bool) {
        self.mailbox.change(|pending| {
            debug_assert!(pending.render.is_none());
            pending.render = Some(RenderRequest {
                id,
                generation,
                key,
                candidate,
            });
        });
    }
}

impl Drop for PdfWorker {
    fn drop(&mut self) {
        self.events.close();
        self.mailbox.change(|pending| pending.closed = true);
    }
}

struct OpenRequest {
    id: u64,
    path: PathBuf,
    password: Option<String>,
}

struct RenderRequest {
    id: u64,
    generation: u64,
    key: RenderKey,
    candidate: bool,
}

enum Command {
    Open(OpenRequest),
    Accept(u64),
    Discard(u64),
    Render(RenderRequest),
}

#[derive(Default)]
struct Pending {
    open: Option<OpenRequest>,
    accept: Option<u64>,
    discard: Option<u64>,
    render: Option<RenderRequest>,
    closed: bool,
}

#[derive(Default)]
struct Mailbox {
    pending: Mutex<Pending>,
    ready: Condvar,
}

impl Mailbox {
    fn change(&self, update: impl FnOnce(&mut Pending)) {
        update(&mut self.pending.lock().unwrap());
        self.ready.notify_one();
    }

    fn next(&self) -> Option<Command> {
        let mut pending = self.pending.lock().unwrap();
        loop {
            if pending.closed {
                return None;
            }
            if let Some(id) = pending.accept.take() {
                return Some(Command::Accept(id));
            }
            if let Some(id) = pending.discard.take() {
                return Some(Command::Discard(id));
            }
            if let Some(request) = pending.open.take() {
                return Some(Command::Open(request));
            }
            if let Some(request) = pending.render.take() {
                return Some(Command::Render(request));
            }
            pending = self.ready.wait(pending).unwrap();
        }
    }
}

struct StagedDocument<'a> {
    id: u64,
    document: PdfDocument<'a>,
    validated: bool,
}

fn run(mailbox: &Mailbox, events: &async_channel::Sender<PdfEvent>) {
    let pdfium = load_pdfium();
    let mut active = None;
    let mut staged: Option<StagedDocument<'_>> = None;

    while let Some(command) = mailbox.next() {
        let event = match command {
            Command::Open(request) => {
                // The prior candidate can be dropped. Keep the active document until the UI
                // accepts a candidate whose first page also rendered successfully.
                staged = None;
                let id = request.id;
                match pdfium
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|pdfium| open_document(pdfium, request))
                {
                    Ok((document, pages)) => {
                        staged = Some(StagedDocument {
                            id,
                            document,
                            validated: false,
                        });
                        PdfEvent::Opened { id, pages }
                    }
                    Err(error) => PdfEvent::OpenFailed { id, error },
                }
            }
            Command::Accept(id) => {
                if staged
                    .as_ref()
                    .is_some_and(|staged| staged.id == id && staged.validated)
                {
                    active = staged.take().map(|staged| (staged.id, staged.document));
                }
                continue;
            }
            Command::Discard(id) => {
                if staged.as_ref().is_some_and(|staged| staged.id == id) {
                    staged = None;
                }
                continue;
            }
            Command::Render(RenderRequest {
                id,
                generation,
                key,
                candidate,
            }) => {
                let raster = if candidate {
                    staged
                        .as_mut()
                        .filter(|staged| staged.id == id)
                        .ok_or(PdfError::CannotRender)
                        .and_then(|staged| {
                            let result = render_page(&staged.document, key);
                            if key.page == 0 {
                                staged.validated = result.is_ok();
                            }
                            result
                        })
                } else {
                    active
                        .as_ref()
                        .filter(|(current, _)| *current == id)
                        .ok_or(PdfError::CannotRender)
                        .and_then(|(_, document)| render_page(document, key))
                };
                PdfEvent::Rendered {
                    id,
                    generation,
                    key,
                    candidate,
                    raster,
                }
            }
        };

        if events.send_blocking(event).is_err() {
            break;
        }
    }
}

fn library_path() -> Result<PathBuf, PdfError> {
    let executable =
        std::env::current_exe().map_err(|error| PdfError::LibraryUnavailable(error.to_string()))?;
    let directory = executable
        .parent()
        .ok_or_else(|| PdfError::LibraryUnavailable(executable.display().to_string()))?;

    if cfg!(target_os = "macos")
        && let Some(contents) = directory.parent()
    {
        let framework = Pdfium::pdfium_platform_library_name_at_path(&contents.join("Frameworks"));
        if framework.is_file() {
            return Ok(framework);
        }
    }

    Ok(Pdfium::pdfium_platform_library_name_at_path(directory))
}

fn load_pdfium() -> Result<Pdfium, PdfError> {
    let path = library_path()?;
    Pdfium::bind_to_library(&path)
        .map(Pdfium::new)
        .map_err(|error| PdfError::LibraryUnavailable(format!("{} / {error}", path.display())))
}

fn open_document(
    pdfium: &Pdfium,
    request: OpenRequest,
) -> Result<(PdfDocument<'_>, Vec<Option<PageSize>>), PdfError> {
    let metadata = request
        .path
        .metadata()
        .map_err(|error| PdfError::CannotRead(error.to_string()))?;
    if !metadata.is_file() {
        return Err(PdfError::InvalidPdf);
    }

    let password = request.password.as_deref();
    if password.is_some_and(|password| password.contains('\0')) {
        return Err(PdfError::PasswordRequired);
    }
    let document =
        pdfium
            .load_pdf_from_file(&request.path, password)
            .map_err(|error| match error {
                PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                    PdfError::PasswordRequired
                }
                PdfiumError::IoError(error) => PdfError::CannotRead(error.to_string()),
                _ => PdfError::InvalidPdf,
            })?;
    let count = document.pages().len();
    if count == 0 {
        return Err(PdfError::EmptyPdf);
    }
    let mut pages = Vec::new();
    pages
        .try_reserve_exact(usize::try_from(count).map_err(|_| PdfError::InvalidPdf)?)
        .map_err(|_| PdfError::InsufficientMemory)?;
    pages.extend((0..count).map(|index| page_size(&document, index)));
    if pages.first().is_none_or(Option::is_none) {
        return Err(PdfError::CannotRender);
    }
    Ok((document, pages))
}

fn page_size(document: &PdfDocument<'_>, index: i32) -> Option<PageSize> {
    let rect = document.pages().page_size(index).ok()?;
    let size = PageSize {
        width: f64::from(rect.width().value),
        height: f64::from(rect.height().value),
    };
    size.is_valid().then_some(size)
}

fn render_page(document: &PdfDocument<'_>, key: RenderKey) -> Result<Raster, PdfError> {
    let index = i32::try_from(key.page).map_err(|_| PdfError::CannotRender)?;
    let size = page_size(document, index)
        .and_then(|geometry| geometry.fit(key.bounds()))
        .ok_or(PdfError::CannotRender)?;
    let page = document
        .pages()
        .get(index)
        .map_err(|_| PdfError::CannotRender)?;

    let bytes = size.byte_len().ok_or(PdfError::CannotRender)?;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(bytes)
        .map_err(|_| PdfError::InsufficientMemory)?;
    bgra.resize(bytes, 0);
    let width = size.width as i32;
    let height = size.height as i32;
    let config = PdfRenderConfig::new()
        .set_fixed_size(width, height)
        .set_format(PdfBitmapFormat::BGRA)
        .set_reverse_byte_order(false);
    let mut bitmap = PdfBitmap::from_bytes(width, height, PdfBitmapFormat::BGRA, &mut bgra)
        .map_err(|_| PdfError::CannotRender)?;
    page.render_into_bitmap_with_config(&mut bitmap, &config)
        .map_err(|_| PdfError::CannotRender)?;
    drop(bitmap);

    Ok(Raster {
        width: size.width,
        height: size.height,
        bgra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn next_event(events: &async_channel::Receiver<PdfEvent>) -> PdfEvent {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match events.try_recv() {
                Ok(event) => return event,
                Err(async_channel::TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("PDF worker did not return an event: {error}"),
            }
        }
    }

    fn fine_detail_pdf() -> Vec<u8> {
        // The thin mark is exactly one device pixel wide in the 125% viewport.
        let content = "0 g\n40 20 0.4 20 re f\nBT /F1 9 Tf 80 105 Td (Fine text) Tj ET\n";
        let stream = format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        );
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 150] \
             /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
            &stream,
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            writeln!(&mut pdf, "{} 0 obj\n{object}\nendobj", index + 1).unwrap();
        }
        let xref = pdf.len();
        writeln!(
            &mut pdf,
            "xref\n0 {}\n0000000000 65535 f ",
            offsets.len() + 1
        )
        .unwrap();
        for offset in offsets {
            writeln!(&mut pdf, "{offset:010} 00000 n ").unwrap();
        }
        writeln!(
            &mut pdf,
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
            objects.len() + 1,
        )
        .unwrap();
        pdf
    }

    fn verify_viewport_rendering(
        worker: &PdfWorker,
        events: &async_channel::Receiver<PdfEvent>,
        path: &Path,
    ) {
        fs::write(path, fine_detail_pdf()).unwrap();
        let id = 8;
        let generation = 15;
        let monitor = RasterSize {
            width: 2560,
            height: 1600,
        };
        worker.open(id, path.to_owned(), None);
        let geometry = match next_event(events) {
            PdfEvent::Opened { id: 8, pages } => pages[0].unwrap(),
            _ => panic!("expected the vector fixture to be staged"),
        };
        worker.render_candidate(id, generation, RenderKey::new(0, monitor));
        assert!(matches!(
            next_event(events),
            PdfEvent::Rendered {
                id: 8,
                candidate: true,
                raster: Ok(Raster {
                    width: 2560,
                    height: 1600,
                    ..
                }),
                ..
            }
        ));
        worker.accept(id);

        // Device-pixel bounds for a 600×300 logical viewport at 125%, 150%, and 200%.
        for (width, height, fitted_width) in [(750, 375, 600), (900, 450, 720), (1200, 600, 960)] {
            let bounds = RasterSize { width, height };
            let expected = RasterSize {
                width: fitted_width,
                height,
            };
            assert_eq!(geometry.fit(bounds), Some(expected));
            let key = RenderKey::new(0, bounds);
            worker.render(id, generation, key);
            let raster = match next_event(events) {
                PdfEvent::Rendered {
                    id: 8,
                    generation: 15,
                    key: rendered_key,
                    candidate: false,
                    raster: Ok(raster),
                } if rendered_key == key => raster,
                _ => panic!("expected a direct viewport render"),
            };
            assert_eq!(
                (raster.width, raster.height),
                (expected.width, expected.height)
            );
            assert_eq!(raster.bgra.len(), expected.byte_len().unwrap());

            let pixel = |x: u32, y: u32| {
                let index = ((y * raster.width + x) * 4) as usize;
                &raster.bgra[index..index + 4]
            };
            let line_x = raster.width / 6;
            let line_y = raster.height * 4 / 5;
            assert_eq!(pixel(line_x - 1, line_y), [255, 255, 255, 255]);
            assert_eq!(pixel(line_x, line_y), [0, 0, 0, 255]);
            assert_eq!(pixel(line_x + 2, line_y), [255, 255, 255, 255]);

            let text_region = (raster.height / 4..raster.height * 3 / 10)
                .any(|y| (raster.width / 3..raster.width / 2).any(|x| pixel(x, y)[0] < 64));
            assert!(text_region, "expected text ink at {width}×{height}");
        }
    }

    #[test]
    fn raster_dimensions_fit_8k_without_changing_page_aspect() {
        let display = RasterSize {
            width: 7680,
            height: 4320,
        };
        let landscape = PageSize {
            width: 16.0,
            height: 9.0,
        };
        let portrait = PageSize {
            width: 9.0,
            height: 16.0,
        };
        let one_pixel_wide = PageSize {
            width: 1.0,
            height: 10_000.0,
        };
        assert_eq!(landscape.fit(display), Some(display));
        assert_eq!(display.byte_len(), Some(132_710_400));
        assert_eq!(display.render_bytes(), Some(265_420_800));
        assert_eq!(
            portrait.fit(display),
            Some(RasterSize {
                width: 2430,
                height: 4320
            })
        );
        assert_eq!(
            one_pixel_wide.fit(display),
            Some(RasterSize {
                width: 1,
                height: 4320
            })
        );
        assert_eq!(
            landscape.fit(RasterSize {
                width: u32::MAX,
                height: u32::MAX
            }),
            None
        );
        assert_eq!(
            PageSize {
                width: f64::NAN,
                height: 1.0
            }
            .fit(display),
            None
        );
    }

    #[test]
    fn worker_renders_a_pdf_and_keeps_it_on_failed_open() {
        if !library_path().unwrap().is_file() {
            assert!(
                std::env::var_os("ELAFRA_REQUIRE_PDFIUM").is_none(),
                "the PDFium library is missing from the test executable directory"
            );
            return;
        }

        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let folder = tempfile::tempdir().unwrap();
        let basic = folder.path().join("投影資料.pdf");
        let rotated = folder.path().join("回転資料.pdf");
        let encrypted = folder.path().join("保護資料.pdf");
        let malformed = folder.path().join("破損資料.pdf");
        fs::copy(fixtures.join("basic.pdf"), &basic).unwrap();
        fs::copy(fixtures.join("encrypted.pdf"), &encrypted).unwrap();
        fs::write(&malformed, b"%PDF-1.7\nThis file is truncated.").unwrap();
        let mut rotated_pdf = fs::read(&basic).unwrap();
        let page_rotation = b"/Rotate 0 /Trans";
        let offset = rotated_pdf
            .windows(page_rotation.len())
            .position(|window| window == page_rotation)
            .unwrap();
        // Keep the cross-reference offsets valid by replacing the same number of bytes.
        rotated_pdf[offset..offset + page_rotation.len()].copy_from_slice(b"/Rotate 90/Trans");
        fs::write(&rotated, rotated_pdf).unwrap();

        let (worker, events) = PdfWorker::start();
        worker.open(1, basic, None);
        let first_key = RenderKey::new(
            0,
            RasterSize {
                width: 400,
                height: 400,
            },
        );
        let expected_size = match next_event(&events) {
            PdfEvent::Opened { id: 1, pages } => {
                assert_eq!(pages.len(), 2);
                pages[0].unwrap().fit(first_key.bounds()).unwrap()
            }
            PdfEvent::OpenFailed { error, .. } => panic!("could not open fixture: {error}"),
            _ => panic!("expected the first document to be opened"),
        };
        worker.render_candidate(1, 10, first_key);
        match next_event(&events) {
            PdfEvent::Rendered {
                id: 1,
                generation: 10,
                candidate: true,
                key,
                raster: Ok(first_page),
            } => {
                assert_eq!(key, first_key);
                assert_eq!(
                    (first_page.width, first_page.height),
                    (expected_size.width, expected_size.height)
                );
                assert_eq!(first_page.height * 2, first_page.width);
                assert!(
                    first_page
                        .bgra
                        .chunks_exact(4)
                        .any(|pixel| pixel == [0, 0, 255, 255]),
                    "opaque red should be in BGRA channel order"
                );
            }
            _ => panic!("expected the first staged page to render"),
        }
        worker.accept(1);

        worker.open(2, malformed, None);
        assert!(matches!(
            next_event(&events),
            PdfEvent::OpenFailed {
                id: 2,
                error: PdfError::InvalidPdf,
            }
        ));

        for (id, password) in [(3, None), (4, Some("incorrect"))] {
            worker.open(id, encrypted.clone(), password.map(str::to_owned));
            assert!(matches!(
                next_event(&events),
                PdfEvent::OpenFailed {
                    id: event_id,
                    error: PdfError::PasswordRequired,
                } if event_id == id
            ));
        }

        worker.open(5, encrypted.clone(), Some("elafra".into()));
        assert!(matches!(
            next_event(&events),
            PdfEvent::Opened { id: 5, .. }
        ));

        let bounded = RenderKey {
            page: 0,
            max_width: 100,
            max_height: 30,
        };
        worker.accept(5);
        worker.render(1, 11, bounded);
        match next_event(&events) {
            PdfEvent::Rendered {
                id: 1,
                generation: 11,
                key,
                candidate: false,
                raster: Ok(raster),
            } => {
                assert_eq!(key, bounded);
                assert_eq!((raster.width, raster.height), (60, 30));
            }
            _ => panic!("expected the active document to remain available"),
        }

        worker.discard(5);
        worker.render_candidate(5, 12, bounded);
        assert!(matches!(
            next_event(&events),
            PdfEvent::Rendered {
                id: 5,
                generation: 12,
                candidate: true,
                raster: Err(PdfError::CannotRender),
                ..
            }
        ));

        worker.open(6, encrypted, Some("elafra".into()));
        assert!(matches!(
            next_event(&events),
            PdfEvent::Opened { id: 6, .. }
        ));
        worker.render_candidate(6, 13, bounded);
        assert!(matches!(
            next_event(&events),
            PdfEvent::Rendered {
                id: 6,
                generation: 13,
                candidate: true,
                raster: Ok(_),
                ..
            }
        ));
        worker.accept(6);
        worker.render(6, 13, bounded);
        assert!(matches!(
            next_event(&events),
            PdfEvent::Rendered {
                id: 6,
                generation: 13,
                candidate: false,
                raster: Ok(_),
                ..
            }
        ));

        worker.open(7, rotated, None);
        let rotated_key = RenderKey::new(
            0,
            RasterSize {
                width: 400,
                height: 400,
            },
        );
        let expected = RasterSize {
            width: 200,
            height: 400,
        };
        match next_event(&events) {
            PdfEvent::Opened { id: 7, pages } => {
                assert_eq!(pages[0].unwrap().fit(rotated_key.bounds()), Some(expected));
            }
            _ => panic!("expected the rotated document to be staged"),
        }
        worker.render_candidate(7, 14, rotated_key);
        assert!(matches!(
            next_event(&events),
            PdfEvent::Rendered {
                id: 7,
                generation: 14,
                candidate: true,
                raster: Ok(Raster {
                    width: 200,
                    height: 400,
                    ..
                }),
                ..
            }
        ));
        verify_viewport_rendering(&worker, &events, &folder.path().join("細字と細線.pdf"));
    }
}
