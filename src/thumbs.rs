//! 16px thumbnails and full image previews, made off the UI thread. The shared
//! freedesktop cache (~/.cache/thumbnails) is read here; anything it doesn't have goes to
//! swayfin-thumbd, which runs KDE's KIO thumbnailers (and stores their results in that
//! same cache) and decodes previews with Qt's image readers. The helper is only started
//! the first time it's needed.

use std::{
    collections::{HashMap, HashSet},
    env,
    fs::File,
    io::{self, BufReader, Read, Write},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use md5::{Digest, Md5};
use smithay_client_toolkit::reexports::calloop::channel::Sender;

pub const SIZE: usize = 16;

/// Premultiplied ARGB, at most SIZE x SIZE.
pub struct Thumb {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

pub struct Request {
    pub path: PathBuf,
    /// Modification time (seconds) the thumbnail must match.
    pub mtime: i64,
}

/// A decoded preview: premultiplied ARGB, `w` x `h`, showing the rect `covers`
/// (x, y, w, h in native pixels) of an image that is `native` in size (upright).
pub struct Image {
    pub native: (usize, usize),
    pub covers: (usize, usize, usize, usize),
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

pub enum PreviewRequest {
    /// The whole image, fitted into w x h (never enlarged).
    Fit { w: usize, h: usize },
    /// Native-pixel rect (x, y, w, h), scaled to `size`.
    Region {
        rect: (usize, usize, usize, usize),
        size: (usize, usize),
    },
}

/// A finished preview: None if the file isn't an image Qt can read.
pub struct PreviewDone {
    pub id: u32,
    pub image: Option<Image>,
}

/// Where results go.
#[derive(Clone)]
pub struct Ui {
    pub thumbs: Sender<Done>,
    pub previews: Sender<PreviewDone>,
}

/// A finished request: None if no thumbnail can be made for it.
pub struct Done {
    pub path: PathBuf,
    pub mtime: i64,
    pub thumb: Option<Thumb>,
}

enum Msg {
    /// What to make next, most important first; replaces anything not yet started.
    Queue(Vec<Request>),
    /// The folder changed: drop everything, including work the helper has in flight.
    Cancel,
    Preview {
        id: u32,
        path: PathBuf,
        req: PreviewRequest,
    },
}

pub struct Thumbnailer {
    tx: Option<mpsc::Sender<Msg>>,
    ui: Ui,
}

impl Thumbnailer {
    pub fn new(ui: Ui) -> Self {
        Self { tx: None, ui }
    }

    fn send(&mut self, msg: Msg) {
        let ui = self.ui.clone();
        let tx = self.tx.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || worker(rx, ui));
            tx
        });
        let _ = tx.send(msg);
    }

    pub fn request(&mut self, queue: Vec<Request>) {
        self.send(Msg::Queue(queue));
    }

    /// Asks for (part of) `path` decoded; the answer comes as a PreviewDone with `id`.
    /// Only the newest request is worked on.
    pub fn preview(&mut self, id: u32, path: PathBuf, req: PreviewRequest) {
        self.send(Msg::Preview { id, path, req });
    }

    pub fn cancel(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Cancel);
        }
    }
}

fn worker(rx: mpsc::Receiver<Msg>, ui: Ui) {
    let cache = thumbnail_cache_dir();
    let mut queue: Vec<Request> = Vec::new();
    let mut helper = HelperState::NotStarted;
    // What the helper is working on: path -> mtime, so its answers can be tagged.
    let sent: Arc<Mutex<HashMap<PathBuf, i64>>> = Arc::default();

    loop {
        let msg = if queue.is_empty() {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            }
        } else {
            rx.try_recv().ok()
        };
        match msg {
            Some(Msg::Queue(q)) => {
                queue = q;
                queue.reverse(); // pop() from the end = most important first
                continue;
            }
            Some(Msg::Cancel) => {
                queue.clear();
                sent.lock().unwrap().clear();
                if let HelperState::Running(h) = &mut helper {
                    if h.write(b"\x01\0").is_err() {
                        helper = HelperState::Dead;
                    }
                }
                continue;
            }
            Some(Msg::Preview { id, path, req }) => {
                helper.start(&ui, &sent);
                let args = match req {
                    PreviewRequest::Fit { w, h } => format!("{id} F {w} {h} "),
                    PreviewRequest::Region {
                        rect: (x, y, rw, rh),
                        size: (w, h),
                    } => format!("{id} R {x} {y} {rw} {rh} {w} {h} "),
                };
                let mut record = format!("\x02{args}").into_bytes();
                record.extend_from_slice(path.as_os_str().as_bytes());
                record.push(0);
                let delivered = match &mut helper {
                    HelperState::Running(h) => h.write(&record).is_ok(),
                    _ => false,
                };
                if !delivered {
                    helper = HelperState::Dead;
                    let _ = ui.previews.send(PreviewDone { id, image: None });
                }
                continue;
            }
            None => {}
        }
        let Some(req) = queue.pop() else { continue };
        if sent.lock().unwrap().contains_key(&req.path) {
            continue;
        }
        if let Some(thumb) = cache.as_deref().and_then(|c| from_cache(c, &req)) {
            let _ = ui.thumbs.send(Done {
                path: req.path,
                mtime: req.mtime,
                thumb: Some(thumb),
            });
            continue;
        }

        helper.start(&ui, &sent);
        let mut record = req.path.as_os_str().as_bytes().to_vec();
        record.push(0);
        let delivered = match &mut helper {
            HelperState::Running(h) => {
                sent.lock().unwrap().insert(req.path.clone(), req.mtime);
                let ok = h.write(&record).is_ok();
                if !ok {
                    sent.lock().unwrap().remove(&req.path);
                    helper = HelperState::Dead;
                }
                ok
            }
            _ => false,
        };
        if !delivered {
            // No helper: only cached thumbnails can be shown.
            let _ = ui.thumbs.send(Done {
                path: req.path,
                mtime: req.mtime,
                thumb: None,
            });
        }
    }
}

enum HelperState {
    NotStarted,
    Running(Helper),
    Dead,
}

impl HelperState {
    fn start(&mut self, ui: &Ui, sent: &Arc<Mutex<HashMap<PathBuf, i64>>>) {
        if let HelperState::NotStarted = self {
            *self = match Helper::spawn(ui.clone(), sent.clone()) {
                Some(h) => HelperState::Running(h),
                None => HelperState::Dead,
            };
        }
    }
}

struct Helper {
    _child: Child,
    stdin: ChildStdin,
}

impl Helper {
    fn spawn(ui: Ui, sent: Arc<Mutex<HashMap<PathBuf, i64>>>) -> Option<Self> {
        let mut child = Command::new(helper_path()?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            // Ends when the helper exits; unanswered paths just stay without a thumbnail.
            while let Ok(answer) = read_answer(&mut r) {
                let sent_ok = match answer {
                    Answer::Thumb(path, thumb) => {
                        let Some(mtime) = sent.lock().unwrap().remove(&path) else {
                            continue; // cancelled meanwhile
                        };
                        ui.thumbs.send(Done { path, mtime, thumb }).is_ok()
                    }
                    Answer::Preview(id, image) => {
                        ui.previews.send(PreviewDone { id, image }).is_ok()
                    }
                };
                if !sent_ok {
                    return;
                }
            }
        });
        Some(Self {
            _child: child,
            stdin,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stdin.write_all(bytes)
    }
}

/// Built next to swayfin by build.rs; an installed copy beside the binary also works.
fn helper_path() -> Option<PathBuf> {
    let built = option_env!("SWAYFIN_THUMBD")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    let beside = env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.join("swayfin-thumbd")));
    built.into_iter().chain(beside).find(|p| p.is_file())
}

enum Answer {
    Thumb(PathBuf, Option<Thumb>),
    Preview(u32, Option<Image>),
}

/// Largest preview side we accept from the helper.
const PREVIEW_MAX: usize = 16384;

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_pixels(r: &mut impl Read, n: usize) -> io::Result<Vec<u32>> {
    let mut raw = vec![0u8; n * 4];
    r.read_exact(&mut raw)?;
    Ok(raw
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// One answer from the helper (see thumbd/main.cpp for the format).
fn read_answer(r: &mut impl Read) -> io::Result<Answer> {
    let mut status = [0u8; 1];
    r.read_exact(&mut status)?;
    let len = read_u32(r)? as usize;
    let mut path = vec![0u8; len];
    r.read_exact(&mut path)?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(path));
    match status[0] {
        b'O' => {}
        b'X' => return Ok(Answer::Thumb(path, None)),
        b'Q' => return Ok(Answer::Preview(read_u32(r)?, None)),
        b'P' => {
            let id = read_u32(r)?;
            let mut v = [0usize; 8];
            for x in &mut v {
                *x = read_u32(r)? as usize;
            }
            let [nw, nh, cx, cy, cw, ch, w, h] = v;
            if w == 0 || h == 0 || w > PREVIEW_MAX || h > PREVIEW_MAX || cw == 0 || ch == 0 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let px = read_pixels(r, w * h)?;
            let image = Image {
                native: (nw, nh),
                covers: (cx, cy, cw, ch),
                w,
                h,
                px,
            };
            return Ok(Answer::Preview(id, Some(image)));
        }
        _ => return Err(io::ErrorKind::InvalidData.into()),
    }
    let mut dims = [0u8; 2];
    r.read_exact(&mut dims)?;
    let (w, h) = (dims[0] as usize, dims[1] as usize);
    if w == 0 || h == 0 || w > SIZE || h > SIZE {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let px = read_pixels(r, w * h)?;
    Ok(Answer::Thumb(path, Some(Thumb { w, h, px })))
}

// ---------------------------------------------------------------- shared cache

fn thumbnail_cache_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("thumbnails"))
}

/// The cached thumbnail for `req`, if one exists and is still current (Thumb::MTime).
/// Smallest flavor first, since we only need 16px.
fn from_cache(cache: &Path, req: &Request) -> Option<Thumb> {
    let hash = Md5::digest(cache_uri(&req.path).as_bytes());
    let name: String = hash.iter().map(|b| format!("{b:02x}")).collect::<String>() + ".png";
    ["normal", "large", "x-large", "xx-large"]
        .iter()
        .find_map(|flavor| load_png(&cache.join(flavor).join(&name), req.mtime))
}

/// The URI the thumbnail spec hashes, encoded the way KIO (QUrl) and GIO do: unreserved
/// characters, sub-delimiters, ':', '@' and '/' stay; everything else is %XX.
fn cache_uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn load_png(path: &Path, mtime: i64) -> Option<Thumb> {
    let mut decoder = png::Decoder::new(BufReader::new(File::open(path).ok()?));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    let info = reader.info();
    let stored = info
        .uncompressed_latin1_text
        .iter()
        .find(|t| t.keyword == "Thumb::MTime")
        .and_then(|t| t.text.trim().parse::<i64>().ok());
    if stored != Some(mtime) {
        return None;
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    let bytes = &buf[..frame.buffer_size()];
    let rgba: Vec<[u8; 4]> = match frame.color_type {
        png::ColorType::Rgba => bytes
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect(),
        png::ColorType::Rgb => bytes
            .chunks_exact(3)
            .map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => bytes
            .chunks_exact(2)
            .map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        png::ColorType::Grayscale => bytes.iter().map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    (w > 0 && h > 0 && rgba.len() == w * h).then(|| fit(&rgba, w, h))
}

/// Box-filters an RGBA image down to fit SIZE x SIZE (never up), keeping the aspect
/// ratio, into premultiplied ARGB.
fn fit(src: &[[u8; 4]], w: usize, h: usize) -> Thumb {
    let scale = (SIZE as f64 / w as f64)
        .min(SIZE as f64 / h as f64)
        .min(1.0);
    let dw = ((w as f64 * scale).round() as usize).clamp(1, SIZE);
    let dh = ((h as f64 * scale).round() as usize).clamp(1, SIZE);
    let mut px = Vec::with_capacity(dw * dh);
    for y in 0..dh {
        let (y0, y1) = (y * h / dh, ((y + 1) * h / dh).max(y * h / dh + 1));
        for x in 0..dw {
            let (x0, x1) = (x * w / dw, ((x + 1) * w / dw).max(x * w / dw + 1));
            let mut sum = [0u64; 4];
            for row in y0..y1 {
                for p in &src[row * w + x0..row * w + x1] {
                    let a = p[3] as u64;
                    sum[0] += p[0] as u64 * a;
                    sum[1] += p[1] as u64 * a;
                    sum[2] += p[2] as u64 * a;
                    sum[3] += a;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            // Premultiply while averaging: color sums are already weighted by alpha.
            let a = sum[3] / n;
            let c = |s: u64| (s / (n * 255)) as u32;
            px.push((a as u32) << 24 | c(sum[0]) << 16 | c(sum[1]) << 8 | c(sum[2]));
        }
    }
    Thumb { w: dw, h: dh, px }
}

/// Paths whose thumbnails are wanted but not known yet, for the visible rows.
pub fn missing<'a>(
    known: &HashMap<PathBuf, (i64, Option<Thumb>)>,
    rows: impl Iterator<Item = (PathBuf, i64)> + 'a,
) -> Vec<Request> {
    let mut seen = HashSet::new();
    rows.filter(|(path, mtime)| {
        known.get(path).is_none_or(|(m, _)| m != mtime) && seen.insert(path.clone())
    })
    .map(|(path, mtime)| Request { path, mtime })
    .collect()
}
