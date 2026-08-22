//! Hew runtime: image processing via `ImageMagick` (`MagickWand`).
//!
//! Wraps `magick_rust` to provide image loading, transformation, and
//! output operations for compiled Hew programs. All returned strings are
//! returned as header-aware, NUL-terminated Hew allocations. Image handles are
//! registered under opaque integer handles and must be released with
//! [`hew_magick_destroy`].

use magick_rust::{magick_wand_genesis, MagickWand};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::raw::c_char;
use std::slice;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Once};

static INIT: Once = Once::new();
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);
static IMAGES: LazyLock<Mutex<HashMap<i64, Arc<Mutex<HewMagickImage>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BytesTriple {
    ptr: *mut u8,
    offset: u32,
    len: u32,
}

#[repr(C)]
struct BytesHeader {
    refcount: AtomicU32,
    capacity: u32,
}

const BYTES_HEADER_SIZE: usize = std::mem::size_of::<BytesHeader>();

const _: () = {
    assert!(std::mem::size_of::<BytesTriple>() == 16);
    assert!(BYTES_HEADER_SIZE == 8);
};

fn empty_bytes() -> BytesTriple {
    BytesTriple {
        ptr: std::ptr::null_mut(),
        offset: 0,
        len: 0,
    }
}

fn owned_bytes(value: &[u8]) -> BytesTriple {
    if value.is_empty() {
        return empty_bytes();
    }
    let capacity = value.len().max(16);
    let Ok(capacity_u32) = u32::try_from(capacity) else {
        std::process::abort()
    };
    let Ok(len_u32) = u32::try_from(value.len()) else {
        std::process::abort()
    };
    let Some(allocation_len) = capacity.checked_add(BYTES_HEADER_SIZE) else {
        std::process::abort()
    };
    // SAFETY: the allocation is checked before writing its header and payload.
    let base = unsafe { libc::malloc(allocation_len) }.cast::<u8>();
    if base.is_null() {
        std::process::abort();
    }
    // SAFETY: malloc provides the header alignment and the allocation contains
    // the complete header and payload. Hew owns and releases the returned bytes.
    unsafe {
        // `malloc` returns memory aligned for any fundamental type
        // (`max_align_t`, at least 8 bytes on every supported target), which
        // exceeds `BytesHeader`'s 4-byte alignment requirement, so this cast
        // never produces a misaligned pointer in practice.
        #[allow(
            clippy::cast_ptr_alignment,
            reason = "malloc's alignment guarantee covers BytesHeader; see comment above"
        )]
        base.cast::<BytesHeader>().write(BytesHeader {
            refcount: AtomicU32::new(1),
            capacity: capacity_u32,
        });
        let data = base.add(BYTES_HEADER_SIZE);
        std::ptr::copy_nonoverlapping(value.as_ptr(), data, value.len());
        BytesTriple {
            ptr: data,
            offset: 0,
            len: len_u32,
        }
    }
}

unsafe fn bytes_arg<'a>(value: *const BytesTriple) -> Option<&'a [u8]> {
    // SAFETY: the caller supplies a valid Hew bytes triple.
    let Some(value) = (unsafe { value.as_ref() }) else {
        set_error(ErrorKind::InvalidInput, "image blob pointer is null");
        return None;
    };
    if value.len == 0 {
        set_error(ErrorKind::InvalidInput, "image blob must not be empty");
        return None;
    }
    if value.ptr.is_null() {
        set_error(ErrorKind::InvalidInput, "image blob data is null");
        return None;
    }
    // SAFETY: Hew guarantees the active pointer-plus-length region is readable.
    Some(unsafe { slice::from_raw_parts(value.ptr.add(value.offset as usize), value.len as usize) })
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorKind {
    None = 0,
    InvalidInput = 1,
    Decode = 2,
    Transform = 3,
    Io = 4,
    Closed = 5,
}

#[derive(Debug)]
struct ErrorState {
    kind: ErrorKind,
    message: String,
}

thread_local! {
    static LAST_ERROR: RefCell<ErrorState> = const { RefCell::new(ErrorState {
        kind: ErrorKind::None, message: String::new(),
    }) };
}

fn clear_error() {
    LAST_ERROR.with(|s| {
        let mut s = s.borrow_mut();
        s.kind = ErrorKind::None;
        s.message.clear();
    });
}
fn set_error(kind: ErrorKind, message: impl Into<String>) {
    LAST_ERROR.with(|s| {
        let mut s = s.borrow_mut();
        s.kind = kind;
        s.message = message.into();
    });
}

fn malloc_c_string(value: &str) -> *mut c_char {
    let Some(size) = value.len().checked_add(1) else {
        return std::ptr::null_mut();
    };
    // SAFETY: size includes a trailing NUL and the allocation is checked.
    let output = unsafe { libc::malloc(size) }.cast::<u8>();
    if output.is_null() {
        return output.cast();
    }
    // SAFETY: output names size writable bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(value.as_ptr(), output, value.len());
        output.add(value.len()).write(0);
    }
    output.cast()
}

unsafe fn utf8_with_len<'a>(ptr: *const c_char, len: i64, what: &str) -> Option<&'a str> {
    let Ok(len) = usize::try_from(len) else {
        set_error(
            ErrorKind::InvalidInput,
            format!("{what} length is negative"),
        );
        return None;
    };
    if ptr.is_null() {
        if len == 0 {
            return Some("");
        }
        set_error(ErrorKind::InvalidInput, format!("{what} pointer is null"));
        return None;
    }
    // SAFETY: the caller promises `len` readable bytes.
    let bytes = unsafe { slice::from_raw_parts(ptr.cast::<u8>(), len) };
    match std::str::from_utf8(bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            set_error(ErrorKind::InvalidInput, format!("{what} is not UTF-8: {e}"));
            None
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_magick_last_error_kind() -> i32 {
    LAST_ERROR.with(|s| s.borrow().kind as i32)
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_magick_last_error() -> *mut c_char {
    LAST_ERROR.with(|s| malloc_c_string(&s.borrow().message))
}

#[unsafe(no_mangle)]
pub extern "C" fn hew_magick_image_count() -> i64 {
    IMAGES
        .lock()
        .ok()
        .and_then(|v| i64::try_from(v.len()).ok())
        .unwrap_or(-1)
}

/// Ensure `MagickWand` is initialized exactly once.
fn ensure_init() {
    INIT.call_once(|| {
        magick_wand_genesis();
    });
}

/// An image handle wrapping a `MagickWand`.
#[derive(Debug)]
pub struct HewMagickImage {
    wand: MagickWand,
}

/// ABI-compatible handle for passing opaque image IDs across the Hew FFI boundary.
///
/// `type Image {}` is zero-sized in the Hew compiler, so return values from FFI functions
/// are discarded. Adding `{ handle: i64 }` to the Hew type and using this `#[repr(C)]`
/// wrapper ensures the handle survives the FFI call. Zero means null.
///
/// `Copy` is intentional: the handle is just a registry key. Stale and duplicate copies
/// are rejected after `destroy` removes the key from the registry.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct HewMagickImageHandle {
    pub handle: i64,
}

impl HewMagickImageHandle {
    fn null() -> Self {
        Self { handle: 0 }
    }
    #[must_use]
    pub fn is_null(&self) -> bool {
        self.handle == 0
    }
}

fn register_image(image: HewMagickImage) -> HewMagickImageHandle {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    if handle <= 0 {
        return HewMagickImageHandle::null();
    }
    let Ok(mut images) = IMAGES.lock() else {
        return HewMagickImageHandle::null();
    };
    if images.contains_key(&handle) {
        return HewMagickImageHandle::null();
    }
    images.insert(handle, Arc::new(Mutex::new(image)));
    HewMagickImageHandle { handle }
}

fn lookup_image(img: HewMagickImageHandle) -> Option<Arc<Mutex<HewMagickImage>>> {
    if img.is_null() {
        return None;
    }
    IMAGES.lock().ok()?.get(&img.handle).cloned()
}

fn with_image<R>(
    img: HewMagickImageHandle,
    default: impl Fn() -> R,
    f: impl FnOnce(&mut HewMagickImage) -> R,
) -> R {
    let Some(image) = lookup_image(img) else {
        set_error(ErrorKind::Closed, "Image handle is closed");
        return default();
    };
    let Ok(mut image) = image.lock() else {
        return default();
    };
    f(&mut image)
}

fn transform_status<E: std::fmt::Display>(result: Result<(), E>, operation: &str) -> i32 {
    match result {
        Ok(()) => {
            clear_error();
            0
        }
        Err(error) => {
            set_error(
                ErrorKind::Transform,
                format!("ImageMagick {operation} failed: {error}"),
            );
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Image I/O
// ---------------------------------------------------------------------------

/// Open an image file and return a handle.
///
/// Returns a null handle on error.
///
/// # Safety
///
/// `path` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_magick_open_len(
    path: *const c_char,
    path_len: i64,
) -> HewMagickImageHandle {
    ensure_init();
    // SAFETY: the caller provides the declared readable byte range.
    let Some(path_str) = (unsafe { utf8_with_len(path, path_len, "path") }) else {
        return HewMagickImageHandle::null();
    };
    let wand = MagickWand::new();
    if let Err(error) = wand.read_image(path_str) {
        set_error(
            ErrorKind::Decode,
            format!("ImageMagick open failed: {error}"),
        );
        return HewMagickImageHandle::null();
    }
    let handle = register_image(HewMagickImage { wand });
    if !handle.is_null() {
        clear_error();
    }
    handle
}

/// Decode an encoded image blob and return an owned image handle.
///
/// # Safety
///
/// `blob` must point to a valid Hew `bytes` triple for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_magick_open_blob(blob: *const BytesTriple) -> HewMagickImageHandle {
    ensure_init();
    // SAFETY: the caller supplies the declared Hew bytes value.
    let Some(blob) = (unsafe { bytes_arg(blob) }) else {
        return HewMagickImageHandle::null();
    };
    let wand = MagickWand::new();
    if let Err(error) = wand.read_image_blob(blob) {
        set_error(
            ErrorKind::Decode,
            format!("ImageMagick blob decode failed: {error}"),
        );
        return HewMagickImageHandle::null();
    }
    let handle = register_image(HewMagickImage { wand });
    if !handle.is_null() {
        clear_error();
    }
    handle
}

/// Create a new blank image with the given dimensions and background color.
///
/// Returns a null handle on error.
///
/// # Safety
///
/// `color` must be a valid NUL-terminated C string (e.g. "white", "#FF0000").
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_magick_new_len(
    width: i32,
    height: i32,
    color: *const c_char,
    color_len: i64,
) -> HewMagickImageHandle {
    ensure_init();
    // SAFETY: the caller provides the declared readable byte range.
    let Some(color_str) = (unsafe { utf8_with_len(color, color_len, "color") }) else {
        return HewMagickImageHandle::null();
    };

    let wand = MagickWand::new();
    let mut pw = magick_rust::PixelWand::new();
    if let Err(error) = pw.set_color(color_str) {
        set_error(
            ErrorKind::InvalidInput,
            format!("invalid image color: {error}"),
        );
        return HewMagickImageHandle::null();
    }

    let (Ok(w), Ok(h)) = (usize::try_from(width), usize::try_from(height)) else {
        set_error(ErrorKind::InvalidInput, "image dimensions must be positive");
        return HewMagickImageHandle::null();
    };
    if w == 0 || h == 0 {
        set_error(ErrorKind::InvalidInput, "image dimensions must be positive");
        return HewMagickImageHandle::null();
    }
    if let Err(error) = wand.new_image(w, h, &pw) {
        set_error(
            ErrorKind::Transform,
            format!("ImageMagick image creation failed: {error}"),
        );
        return HewMagickImageHandle::null();
    }
    let handle = register_image(HewMagickImage { wand });
    if !handle.is_null() {
        clear_error();
    }
    handle
}

/// Write an image to a file path.
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// - `img` must be a valid handle from [`hew_magick_open`] or [`hew_magick_new`].
/// - `path` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_magick_write_len(
    img: HewMagickImageHandle,
    path: *const c_char,
    path_len: i64,
) -> i32 {
    // SAFETY: the caller provides the declared readable byte range.
    let Some(path_str) = (unsafe { utf8_with_len(path, path_len, "path") }) else {
        return -1;
    };
    with_image(
        img,
        || -1,
        |image| match image.wand.write_image(path_str) {
            Ok(()) => {
                clear_error();
                0
            }
            Err(error) => {
                set_error(ErrorKind::Io, format!("ImageMagick write failed: {error}"));
                -1
            }
        },
    )
}

/// Encode an image into a caller-owned Hew `bytes` blob.
///
/// The ImageMagick-owned intermediate is relinquished by `magick_rust`; this
/// function copies it into Hew's refcounted allocation so the Hew drop path is
/// the sole owner and allocator pair for the returned blob.
///
/// # Safety
///
/// `img` must be a live image handle and `format` must name `format_len`
/// readable UTF-8 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hew_magick_write_blob_len(
    img: HewMagickImageHandle,
    format: *const c_char,
    format_len: i64,
) -> BytesTriple {
    // SAFETY: the caller provides the declared readable byte range.
    let Some(format) = (unsafe { utf8_with_len(format, format_len, "format") }) else {
        return empty_bytes();
    };
    if format.is_empty() {
        set_error(ErrorKind::InvalidInput, "image format must not be empty");
        return empty_bytes();
    }
    with_image(img, empty_bytes, |image| {
        match image.wand.write_image_blob(format) {
            Ok(blob) if !blob.is_empty() => {
                clear_error();
                owned_bytes(&blob)
            }
            Ok(_) => {
                set_error(ErrorKind::Io, "ImageMagick returned an empty image blob");
                empty_bytes()
            }
            Err(error) => {
                set_error(
                    ErrorKind::Io,
                    format!("ImageMagick blob encode failed: {error}"),
                );
                empty_bytes()
            }
        }
    })
}

#[cfg(test)]
unsafe fn c_string_len(value: *const c_char) -> i64 {
    if value.is_null() {
        return 0;
    }
    // SAFETY: test callers pass either null or valid C strings.
    unsafe { i64::try_from(std::ffi::CStr::from_ptr(value).to_bytes().len()).unwrap() }
}

#[cfg(test)]
unsafe fn hew_magick_open(path: *const c_char) -> HewMagickImageHandle {
    // SAFETY: forwards the same test C string and its measured payload length.
    unsafe { hew_magick_open_len(path, c_string_len(path)) }
}

#[cfg(test)]
unsafe fn hew_magick_new(width: i32, height: i32, color: *const c_char) -> HewMagickImageHandle {
    // SAFETY: forwards the same test C string and its measured payload length.
    unsafe { hew_magick_new_len(width, height, color, c_string_len(color)) }
}

#[cfg(test)]
unsafe fn hew_magick_write(img: HewMagickImageHandle, path: *const c_char) -> i32 {
    // SAFETY: forwards the same test C string and its measured payload length.
    unsafe { hew_magick_write_len(img, path, c_string_len(path)) }
}

#[cfg(test)]
unsafe fn free_cstring(value: *mut c_char) {
    // SAFETY: test callers pass an allocation-base pointer returned by this module.
    unsafe {
        libc::free(value.cast());
    }
}

// ---------------------------------------------------------------------------
// Transformations
// ---------------------------------------------------------------------------

/// Resize an image to the given dimensions.
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_resize(
    img: HewMagickImageHandle,
    width: i32,
    height: i32,
) -> i32 {
    let (Ok(w), Ok(h)) = (usize::try_from(width), usize::try_from(height)) else {
        return -1;
    };
    if w == 0 || h == 0 {
        return -1;
    }
    with_image(
        img,
        || -1,
        |image| {
            transform_status(
                image
                    .wand
                    .resize_image(w, h, magick_rust::bindings::FilterType::Lanczos),
                "resize",
            )
        },
    )
}

/// Crop a region from an image.
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_crop(
    img: HewMagickImageHandle,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
) -> i32 {
    let (Ok(w), Ok(h)) = (usize::try_from(width), usize::try_from(height)) else {
        return -1;
    };
    if w == 0 || h == 0 {
        return -1;
    }
    let (ox, oy) = (x as isize, y as isize);
    with_image(
        img,
        || -1,
        |image| transform_status(image.wand.crop_image(w, h, ox, oy), "crop"),
    )
}

/// Rotate an image by the given degrees.
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_rotate(img: HewMagickImageHandle, degrees: f64) -> i32 {
    let pw = magick_rust::PixelWand::new();
    with_image(
        img,
        || -1,
        |image| transform_status(image.wand.rotate_image(&pw, degrees), "rotate"),
    )
}

/// Apply a Gaussian blur to an image.
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_blur(
    img: HewMagickImageHandle,
    radius: f64,
    sigma: f64,
) -> i32 {
    with_image(
        img,
        || -1,
        |image| transform_status(image.wand.blur_image(radius, sigma), "blur"),
    )
}

/// Sharpen an image.
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_sharpen(
    img: HewMagickImageHandle,
    radius: f64,
    sigma: f64,
) -> i32 {
    with_image(
        img,
        || -1,
        |image| transform_status(image.wand.sharpen_image(radius, sigma), "sharpen"),
    )
}

/// Flip an image vertically.
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_flip(img: HewMagickImageHandle) -> i32 {
    with_image(
        img,
        || -1,
        |image| transform_status(image.wand.flip_image(), "flip"),
    )
}

/// Flop an image horizontally.
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_flop(img: HewMagickImageHandle) -> i32 {
    with_image(
        img,
        || -1,
        |image| transform_status(image.wand.flop_image(), "flop"),
    )
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// Get the image width in pixels.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_width(img: HewMagickImageHandle) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "image dimensions fit in i32"
    )]
    with_image(img, || 0, |image| image.wand.get_image_width() as i32)
}

/// Get the image height in pixels.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_height(img: HewMagickImageHandle) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "image dimensions fit in i32"
    )]
    with_image(img, || 0, |image| image.wand.get_image_height() as i32)
}

/// Get the image format (e.g. "JPEG", "PNG").
///
/// Returns a `malloc`-allocated NUL-terminated string, or null on error.
///
/// # Safety
///
/// `img` must be a valid handle from image creation functions.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_format(img: HewMagickImageHandle) -> *mut c_char {
    let fmt = with_image(
        img,
        || Err("image handle is closed".to_owned()),
        |image| {
            image
                .wand
                .get_image_format()
                .map_err(|error| error.to_string())
        },
    );
    match fmt {
        Ok(f) => {
            clear_error();
            malloc_c_string(&f)
        }
        Err(error) => {
            set_error(
                ErrorKind::Decode,
                format!("ImageMagick format query failed: {error}"),
            );
            std::ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// Resource management
// ---------------------------------------------------------------------------

/// Destroy an image and free its resources.
///
/// # Safety
///
/// `img` may be any handle returned from image creation functions. Null,
/// zero, stale, and already-destroyed handles are accepted as no-ops.
#[no_mangle]
pub unsafe extern "C" fn hew_magick_destroy(img: HewMagickImageHandle) {
    if img.is_null() {
        return;
    }
    if let Ok(mut images) = IMAGES.lock() {
        images.remove(&img.handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static TEST_FILE_ID: AtomicUsize = AtomicUsize::new(1);

    unsafe fn release_bytes_like_hew(value: BytesTriple) {
        if value.ptr.is_null() {
            return;
        }
        #[allow(
            clippy::cast_ptr_alignment,
            reason = "malloc's alignment guarantee covers BytesHeader; see SAFETY comment below"
        )]
        // SAFETY: test callers pass live values returned by `owned_bytes`.
        // The cast never misaligns: `owned_bytes` derives `value.ptr` from a
        // `malloc`ed base (aligned to `max_align_t`, >= 8 bytes on every
        // supported target) offset by exactly `BYTES_HEADER_SIZE`, so
        // subtracting that offset recovers the original, still-aligned base.
        let header = unsafe { value.ptr.sub(BYTES_HEADER_SIZE).cast::<BytesHeader>() };
        // SAFETY: `header` points to the initialized Hew bytes header.
        if unsafe { (*header).refcount.fetch_sub(1, Ordering::Release) } == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            // SAFETY: the final owner releases the allocation base.
            unsafe { libc::free(header.cast()) };
        }
    }

    fn test_path(name: &str, ext: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-output");
        std::fs::create_dir_all(&dir).unwrap();
        let id = TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("{name}-{id}.{ext}"))
    }

    #[test]
    fn init_magick_wand() {
        ensure_init();
        // Should not panic on repeated calls
        ensure_init();
    }

    #[test]
    fn null_open_returns_null() {
        // SAFETY: null is the scenario under test.
        let img = unsafe { hew_magick_open(std::ptr::null()) };
        assert!(img.is_null());
    }

    #[test]
    fn destroy_null_is_noop() {
        // SAFETY: null handle should be a safe no-op.
        unsafe { hew_magick_destroy(HewMagickImageHandle::null()) };
    }

    #[test]
    fn destroy_is_idempotent_and_stale_handle_is_rejected() {
        let color = std::ffi::CString::new("white").unwrap();
        // SAFETY: color is a valid CString.
        let img = unsafe { hew_magick_new(8, 8, color.as_ptr()) };
        assert!(!img.is_null());

        // SAFETY: img was allocated by hew_magick_new.
        unsafe { hew_magick_destroy(img) };
        // SAFETY: destroying a stale handle is an intentional no-op.
        unsafe { hew_magick_destroy(img) };

        // SAFETY: stale handles are rejected by the registry before dereference.
        assert_eq!(unsafe { hew_magick_resize(img, 4, 4) }, -1);
        // SAFETY: stale handles are rejected by the registry before dereference.
        assert_eq!(unsafe { hew_magick_width(img) }, 0);
    }

    #[test]
    fn new_image_has_correct_dimensions() {
        let color = std::ffi::CString::new("white").unwrap();
        // SAFETY: color is a valid CString.
        let img = unsafe { hew_magick_new(100, 50, color.as_ptr()) };
        assert!(!img.is_null());

        // SAFETY: img is valid from hew_magick_new.
        let w = unsafe { hew_magick_width(img) };
        // SAFETY: img is valid.
        let h = unsafe { hew_magick_height(img) };
        assert_eq!(w, 100);
        assert_eq!(h, 50);

        // SAFETY: img was allocated by hew_magick_new.
        unsafe { hew_magick_destroy(img) };
    }

    #[test]
    fn resize_changes_dimensions() {
        let color = std::ffi::CString::new("blue").unwrap();
        // SAFETY: color is a valid CString.
        let img = unsafe { hew_magick_new(200, 100, color.as_ptr()) };
        assert!(!img.is_null());

        // SAFETY: img is valid.
        let rc = unsafe { hew_magick_resize(img, 50, 25) };
        assert_eq!(rc, 0);

        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_width(img) }, 50);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_height(img) }, 25);

        // SAFETY: img was allocated by hew_magick_new.
        unsafe { hew_magick_destroy(img) };
    }

    #[test]
    fn crop_changes_dimensions() {
        let color = std::ffi::CString::new("blue").unwrap();
        // SAFETY: color is a valid CString.
        let img = unsafe { hew_magick_new(120, 80, color.as_ptr()) };
        assert!(!img.is_null());

        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_crop(img, 40, 30, 10, 5) }, 0);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_width(img) }, 40);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_height(img) }, 30);

        // SAFETY: img was allocated by hew_magick_new.
        unsafe { hew_magick_destroy(img) };
    }

    #[test]
    fn rotate_right_angle_swaps_dimensions() {
        let color = std::ffi::CString::new("yellow").unwrap();
        // SAFETY: color is a valid CString.
        let img = unsafe { hew_magick_new(40, 20, color.as_ptr()) };
        assert!(!img.is_null());

        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_rotate(img, 90.0) }, 0);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_width(img) }, 20);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_height(img) }, 40);

        // SAFETY: img was allocated by hew_magick_new.
        unsafe { hew_magick_destroy(img) };
    }

    #[test]
    fn flip_flop_succeed() {
        let color = std::ffi::CString::new("red").unwrap();
        // SAFETY: color is a valid CString.
        let img = unsafe { hew_magick_new(10, 10, color.as_ptr()) };
        assert!(!img.is_null());

        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_flip(img) }, 0);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_flop(img) }, 0);

        // SAFETY: img was allocated by hew_magick_new.
        unsafe { hew_magick_destroy(img) };
    }

    #[test]
    fn write_to_test_output_file() {
        let color = std::ffi::CString::new("green").unwrap();
        // SAFETY: color is a valid CString.
        let img = unsafe { hew_magick_new(10, 10, color.as_ptr()) };
        assert!(!img.is_null());

        let path = test_path("hew-magick-test", "png");
        let path_cstr = std::ffi::CString::new(path.to_str().unwrap()).unwrap();

        // SAFETY: img and path_cstr are valid.
        let rc = unsafe { hew_magick_write(img, path_cstr.as_ptr()) };
        assert_eq!(rc, 0);
        assert!(path.exists());

        // Clean up
        std::fs::remove_file(&path).ok();
        // SAFETY: img was allocated by hew_magick_new.
        unsafe { hew_magick_destroy(img) };
    }

    /// Full round-trip: create image → write PNG → open PNG → resize →
    /// blur → write JPEG → open JPEG → verify dimensions and format.
    #[test]
    fn thumbnail_round_trip() {
        let src_path = test_path("hew-magick-src", "png");
        let thumb_path = test_path("hew-magick-thumb", "jpg");

        // 1. Create a 400x300 source image and write it as PNG.
        let color = std::ffi::CString::new("#3366CC").unwrap();
        // SAFETY: color is a valid CString.
        let src = unsafe { hew_magick_new(400, 300, color.as_ptr()) };
        assert!(!src.is_null());

        let src_cstr = std::ffi::CString::new(src_path.to_str().unwrap()).unwrap();
        // SAFETY: src and src_cstr are valid.
        assert_eq!(unsafe { hew_magick_write(src, src_cstr.as_ptr()) }, 0);
        // SAFETY: src was allocated by hew_magick_new.
        unsafe { hew_magick_destroy(src) };
        assert!(src_path.exists());

        // 2. Open the PNG we just wrote.
        // SAFETY: src_cstr points to a file that exists.
        let img = unsafe { hew_magick_open(src_cstr.as_ptr()) };
        assert!(!img.is_null());
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_width(img) }, 400);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_height(img) }, 300);

        // 3. Resize to thumbnail (80x60).
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_resize(img, 80, 60) }, 0);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_width(img) }, 80);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_height(img) }, 60);

        // 4. Apply blur and sharpen.
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_blur(img, 0.0, 1.5) }, 0);
        // SAFETY: img is valid.
        assert_eq!(unsafe { hew_magick_sharpen(img, 0.0, 0.5) }, 0);

        // 5. Write as JPEG (format inferred from extension).
        let thumb_cstr = std::ffi::CString::new(thumb_path.to_str().unwrap()).unwrap();
        // SAFETY: img and thumb_cstr are valid.
        assert_eq!(unsafe { hew_magick_write(img, thumb_cstr.as_ptr()) }, 0);
        // SAFETY: img was allocated by hew_magick_open.
        unsafe { hew_magick_destroy(img) };
        assert!(thumb_path.exists());

        // 6. Re-open the JPEG and verify dimensions + format.
        // SAFETY: thumb_cstr points to a file that exists.
        let thumb = unsafe { hew_magick_open(thumb_cstr.as_ptr()) };
        assert!(!thumb.is_null());
        // SAFETY: thumb is valid.
        assert_eq!(unsafe { hew_magick_width(thumb) }, 80);
        // SAFETY: thumb is valid.
        assert_eq!(unsafe { hew_magick_height(thumb) }, 60);

        // SAFETY: thumb is valid.
        let fmt_ptr = unsafe { hew_magick_format(thumb) };
        assert!(!fmt_ptr.is_null());
        // SAFETY: fmt_ptr is a malloc-allocated NUL-terminated string.
        let fmt = unsafe { std::ffi::CStr::from_ptr(fmt_ptr) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: fmt_ptr was allocated with malloc.
        unsafe { free_cstring(fmt_ptr) };
        assert_eq!(fmt, "JPEG");

        // SAFETY: thumb was allocated by hew_magick_open.
        unsafe { hew_magick_destroy(thumb) };

        // Clean up temp files.
        std::fs::remove_file(&src_path).ok();
        std::fs::remove_file(&thumb_path).ok();
    }

    #[test]
    fn encoded_blob_round_trips_a_real_image() {
        let color = std::ffi::CString::new("#2f80ed").unwrap();
        // SAFETY: color is a valid CString.
        let source = unsafe { hew_magick_new(37, 19, color.as_ptr()) };
        assert!(!source.is_null());

        let png = b"PNG";
        // SAFETY: source is live and png is valid for its declared length.
        let blob = unsafe {
            hew_magick_write_blob_len(
                source,
                png.as_ptr().cast(),
                i64::try_from(png.len()).unwrap(),
            )
        };
        assert!(!blob.ptr.is_null());
        assert!(blob.len > 8);
        // SAFETY: blob was just populated above; its ptr/offset/len describe
        // the encoded PNG bytes within the live allocation.
        let encoded =
            unsafe { slice::from_raw_parts(blob.ptr.add(blob.offset as usize), blob.len as usize) };
        assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");

        // SAFETY: blob is a live Hew bytes allocation containing a real PNG.
        let decoded = unsafe { hew_magick_open_blob(&raw const blob) };
        assert!(!decoded.is_null());
        // SAFETY: decoded was just opened above and is live.
        assert_eq!(unsafe { hew_magick_width(decoded) }, 37);
        // SAFETY: decoded was just opened above and is live.
        assert_eq!(unsafe { hew_magick_height(decoded) }, 19);

        // SAFETY: source and decoded were opened above and blob is the live
        // Hew bytes allocation written by hew_magick_write_blob_len; each is
        // released here exactly once.
        unsafe {
            hew_magick_destroy(source);
            hew_magick_destroy(decoded);
            release_bytes_like_hew(blob);
        }
    }
}
