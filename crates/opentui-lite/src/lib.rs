use std::ffi::{CStr, CString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }
}

pub const ATTR_NONE: u32 = 0;
pub const ATTR_BOLD: u32 = 1 << 0;
pub const ATTR_DIM: u32 = 1 << 1;
pub const ATTR_ITALIC: u32 = 1 << 2;
pub const ATTR_UNDERLINE: u32 = 1 << 3;
pub const ATTR_BLINK: u32 = 1 << 4;
pub const ATTR_INVERSE: u32 = 1 << 5;
pub const ATTR_HIDDEN: u32 = 1 << 6;
pub const ATTR_STRIKETHROUGH: u32 = 1 << 7;

pub const BORDER_SIMPLE: [u32; 11] = [
    '+' as u32,
    '+' as u32,
    '+' as u32,
    '+' as u32,
    '-' as u32,
    '|' as u32,
    '+' as u32,
    '+' as u32,
    '+' as u32,
    '+' as u32,
    '+' as u32,
];

type RendererPtr = *mut std::ffi::c_void;
type BufferPtr = *mut std::ffi::c_void;

type FnCreateRenderer = unsafe extern "C" fn(u32, u32, bool) -> RendererPtr;
type FnDestroyRenderer = unsafe extern "C" fn(RendererPtr);
type FnSetupTerminal = unsafe extern "C" fn(RendererPtr, bool);
type FnSuspendRenderer = unsafe extern "C" fn(RendererPtr);
type FnRender = unsafe extern "C" fn(RendererPtr, bool);
type FnClearTerminal = unsafe extern "C" fn(RendererPtr);
type FnResizeRenderer = unsafe extern "C" fn(RendererPtr, u32, u32);
type FnGetNextBuffer = unsafe extern "C" fn(RendererPtr) -> BufferPtr;
type FnGetCurrentBuffer = unsafe extern "C" fn(RendererPtr) -> BufferPtr;
type FnBufferClear = unsafe extern "C" fn(BufferPtr, *const f32);
type FnBufferDrawText =
    unsafe extern "C" fn(BufferPtr, *const u8, usize, u32, u32, *const f32, *const f32, u32);
type FnBufferFillRect = unsafe extern "C" fn(BufferPtr, u32, u32, u32, u32, *const f32);
type FnBufferDrawBox = unsafe extern "C" fn(
    BufferPtr,
    i32,
    i32,
    u32,
    u32,
    *const u32,
    u32,
    *const f32,
    *const f32,
    *const u8,
    u32,
);

#[derive(Clone)]
pub struct OpenTui {
    inner: Arc<Inner>,
}

struct Inner {
    lib: *mut std::ffi::c_void,
    fns: Fns,
    path: String,
}

struct Fns {
    create_renderer: FnCreateRenderer,
    destroy_renderer: FnDestroyRenderer,
    setup_terminal: FnSetupTerminal,
    suspend_renderer: FnSuspendRenderer,
    render: FnRender,
    clear_terminal: FnClearTerminal,
    resize_renderer: FnResizeRenderer,
    get_next_buffer: FnGetNextBuffer,
    get_current_buffer: FnGetCurrentBuffer,
    buffer_clear: FnBufferClear,
    buffer_draw_text: FnBufferDrawText,
    buffer_fill_rect: FnBufferFillRect,
    buffer_draw_box: FnBufferDrawBox,
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe {
            if !self.lib.is_null() {
                let _ = dlclose(self.lib);
            }
        }
    }
}

impl OpenTui {
    pub fn load() -> Result<Self> {
        let (lib, path) = load_library()?;
        let fns = unsafe {
            Fns {
                create_renderer: load_symbol(lib, "createRenderer")?,
                destroy_renderer: load_symbol(lib, "destroyRenderer")?,
                setup_terminal: load_symbol(lib, "setupTerminal")?,
                suspend_renderer: load_symbol(lib, "suspendRenderer")?,
                render: load_symbol(lib, "render")?,
                clear_terminal: load_symbol(lib, "clearTerminal")?,
                resize_renderer: load_symbol(lib, "resizeRenderer")?,
                get_next_buffer: load_symbol(lib, "getNextBuffer")?,
                get_current_buffer: load_symbol(lib, "getCurrentBuffer")?,
                buffer_clear: load_symbol(lib, "bufferClear")?,
                buffer_draw_text: load_symbol(lib, "bufferDrawText")?,
                buffer_fill_rect: load_symbol(lib, "bufferFillRect")?,
                buffer_draw_box: load_symbol(lib, "bufferDrawBox")?,
            }
        };
        Ok(Self {
            inner: Arc::new(Inner { lib, fns, path }),
        })
    }

    pub fn path(&self) -> &str {
        &self.inner.path
    }

    pub fn create_renderer(&self, width: u32, height: u32, testing: bool) -> Result<Renderer> {
        let ptr = unsafe { (self.inner.fns.create_renderer)(width, height, testing) };
        if ptr.is_null() {
            return Err(Error::new("opentui: createRenderer returned null"));
        }
        Ok(Renderer {
            inner: self.inner.clone(),
            ptr,
        })
    }
}

pub struct Renderer {
    inner: Arc<Inner>,
    ptr: RendererPtr,
}

impl Renderer {
    pub fn setup_terminal(&self, use_alternate_screen: bool) {
        unsafe { (self.inner.fns.setup_terminal)(self.ptr, use_alternate_screen) };
    }

    pub fn suspend(&self) {
        unsafe { (self.inner.fns.suspend_renderer)(self.ptr) };
    }

    pub fn clear_terminal(&self) {
        unsafe { (self.inner.fns.clear_terminal)(self.ptr) };
    }

    pub fn resize(&self, width: u32, height: u32) {
        unsafe { (self.inner.fns.resize_renderer)(self.ptr, width, height) };
    }

    pub fn render(&self, force: bool) {
        unsafe { (self.inner.fns.render)(self.ptr, force) };
    }

    pub fn next_buffer(&self) -> Buffer {
        let ptr = unsafe { (self.inner.fns.get_next_buffer)(self.ptr) };
        Buffer {
            inner: self.inner.clone(),
            ptr,
        }
    }

    pub fn current_buffer(&self) -> Buffer {
        let ptr = unsafe { (self.inner.fns.get_current_buffer)(self.ptr) };
        Buffer {
            inner: self.inner.clone(),
            ptr,
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            (self.inner.fns.destroy_renderer)(self.ptr);
        }
    }
}

pub struct Buffer {
    inner: Arc<Inner>,
    ptr: BufferPtr,
}

impl Buffer {
    pub fn clear(&self, bg: Color) {
        unsafe { (self.inner.fns.buffer_clear)(self.ptr, &bg as *const Color as *const f32) };
    }

    pub fn fill_rect(&self, x: u32, y: u32, width: u32, height: u32, bg: Color) {
        unsafe {
            (self.inner.fns.buffer_fill_rect)(
                self.ptr,
                x,
                y,
                width,
                height,
                &bg as *const Color as *const f32,
            )
        };
    }

    pub fn draw_text(&self, text: &str, x: u32, y: u32, fg: Color, bg: Option<Color>, attr: u32) {
        let bg_ptr = match bg {
            Some(color) => &color as *const Color as *const f32,
            None => std::ptr::null(),
        };
        unsafe {
            (self.inner.fns.buffer_draw_text)(
                self.ptr,
                text.as_ptr(),
                text.len(),
                x,
                y,
                &fg as *const Color as *const f32,
                bg_ptr,
                attr,
            )
        };
    }

    pub fn draw_box(
        &self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        border_chars: &[u32; 11],
        packed_options: u32,
        border: Color,
        background: Color,
        title: Option<&str>,
    ) {
        let (title_ptr, title_len) = match title {
            Some(value) => (value.as_ptr(), value.len() as u32),
            None => (std::ptr::null(), 0),
        };
        unsafe {
            (self.inner.fns.buffer_draw_box)(
                self.ptr,
                x,
                y,
                width,
                height,
                border_chars.as_ptr(),
                packed_options,
                &border as *const Color as *const f32,
                &background as *const Color as *const f32,
                title_ptr,
                title_len,
            )
        };
    }
}

fn load_library() -> Result<(*mut std::ffi::c_void, String)> {
    let mut errors = Vec::new();
    for path in candidate_paths() {
        match try_dlopen(&path) {
            Ok(lib) => return Ok((lib, path.display().to_string())),
            Err(err) => errors.push(format!("{}: {}", path.display(), err)),
        }
    }
    let mut message = String::from("opentui: failed to load native library");
    if !errors.is_empty() {
        message.push_str(" (tried: ");
        message.push_str(&errors.join(", "));
        message.push(')');
    }
    Err(Error::new(message))
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let lib_name = lib_filename();

    if let Ok(path) = std::env::var("OPENTUI_LIB_PATH") {
        paths.push(PathBuf::from(path));
    }

    if let Ok(dir) = std::env::var("OPENTUI_LIB_DIR") {
        paths.push(PathBuf::from(dir).join(lib_name));
    }

    if let Ok(prefix) = std::env::var("OPENTUI_PREFIX") {
        paths.push(PathBuf::from(prefix).join("lib").join(lib_name));
    }

    if let Ok(home) = std::env::var("HOME") {
        let home_path = PathBuf::from(&home);
        if let Some(target_dir) = zig_target_dir() {
            paths.push(
                home_path
                    .join("repos/anomalyco/opentui/packages/core/src/zig/lib")
                    .join(target_dir)
                    .join(lib_name),
            );
        }
        paths.push(home_path.join(".local/lib").join(lib_name));
    }

    paths.push(PathBuf::from(lib_name));
    paths
}

fn zig_target_dir() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Some("aarch64-macos"),
        ("x86_64", "macos") => Some("x86_64-macos"),
        ("aarch64", "linux") => Some("aarch64-linux"),
        ("x86_64", "linux") => Some("x86_64-linux"),
        _ => None,
    }
}

fn lib_filename() -> &'static str {
    if cfg!(target_os = "macos") {
        "libopentui.dylib"
    } else if cfg!(target_os = "linux") {
        "libopentui.so"
    } else {
        "libopentui"
    }
}

fn try_dlopen(path: &Path) -> Result<*mut std::ffi::c_void> {
    let cpath = path_to_cstring(path)?;
    unsafe {
        let handle = dlopen(cpath.as_ptr(), libc::RTLD_NOW);
        if handle.is_null() {
            return Err(Error::new(dl_error_string()));
        }
        Ok(handle)
    }
}

fn path_to_cstring(path: &Path) -> Result<CString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::new("opentui: invalid library path"))
    }
    #[cfg(not(unix))]
    {
        Err(Error::new("opentui: unsupported platform"))
    }
}

unsafe fn load_symbol<T>(lib: *mut std::ffi::c_void, symbol: &str) -> Result<T> {
    let name = CString::new(symbol).map_err(|_| Error::new("opentui: invalid symbol"))?;
    let ptr = unsafe { dlsym(lib, name.as_ptr()) };
    if ptr.is_null() {
        return Err(Error::new(format!("opentui: missing symbol {symbol}")));
    }
    Ok(unsafe { std::mem::transmute_copy(&ptr) })
}

fn dl_error_string() -> String {
    unsafe {
        let err = dlerror();
        if err.is_null() {
            return "unknown dlopen error".to_string();
        }
        CStr::from_ptr(err).to_string_lossy().to_string()
    }
}

unsafe extern "C" {
    fn dlopen(path: *const libc::c_char, mode: libc::c_int) -> *mut std::ffi::c_void;
    fn dlsym(handle: *mut std::ffi::c_void, symbol: *const libc::c_char) -> *mut std::ffi::c_void;
    fn dlclose(handle: *mut std::ffi::c_void) -> libc::c_int;
    fn dlerror() -> *const libc::c_char;
}
