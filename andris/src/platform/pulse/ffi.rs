//! Hand-written bindings to the five functions of libpulse's simple API
//! that andris uses, loaded at run time rather than linked.
//!
//! Andris links no part of libpulse. [`Api::open`] loads
//! `libpulse-simple.so.0` with `dlopen`, which brings in `libpulse.so.0`
//! as that library's own dependency, and looks up `pa_simple_new`,
//! `pa_simple_read`, `pa_simple_get_latency`, `pa_simple_free` and
//! `pa_strerror`. A system without libpulse can therefore still run
//! andris, without sound, rather than failing at exec.
//!
//! Every constant, struct and signature here is taken from libpulse's
//! headers, as the development image installs them (libpulse 17):
//! `pulse/sample.h`, `pulse/def.h`, `pulse/simple.h` and `pulse/error.h`.
//! Each item cites its header. The tests check each constant, and each
//! struct's size, alignment and field offsets, against `libpulse-sys`,
//! whose bindings link the real library, so that these cannot drift
//! from it unseen.
//!
//! This module, `platform/x11/shm.rs` and `platform/qemu/shm.rs` are
//! andris's only `unsafe` code; the crate denies it everywhere else.
//! The unsafety here is the foreign function interface: Rust cannot
//! check that a symbol has the type it is declared with, so these
//! declarations must match the headers exactly. Above it, [`Simple`] owns one connection and frees
//! it exactly once, so its users need no `unsafe` of their own.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::fmt;
use std::marker::{PhantomData, PhantomPinned};
use std::ptr::{self, NonNull};
use std::time::Duration;

use anyhow::Context;
use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};

/// `PA_SAMPLE_S16LE`, a `pa_sample_format_t`: signed 16 bit PCM, little
/// endian. The fourth enumerator of `enum pa_sample_format`, after
/// `PA_SAMPLE_U8`, `PA_SAMPLE_ALAW` and `PA_SAMPLE_ULAW` (`pulse/sample.h`).
/// The enum holds `PA_SAMPLE_INVALID = -1`, so it is a signed `int`.
pub(super) const PA_SAMPLE_S16LE: c_int = 3;

/// `PA_STREAM_RECORD`, a `pa_stream_direction_t`: a record stream. The
/// third enumerator of `enum pa_stream_direction`, after
/// `PA_STREAM_NODIRECTION` and `PA_STREAM_PLAYBACK` (`pulse/def.h`). The
/// enum has no negative values, so it is an `unsigned int`.
pub(super) const PA_STREAM_RECORD: c_uint = 2;

/// `PA_OK`, the `pa_error_code_t` for no error (`pulse/def.h`). Error
/// codes start out as this, so a failure that sets none reads as one.
pub(super) const PA_OK: c_int = 0;

/// `PA_USEC_INVALID`, `((pa_usec_t) -1)`: what `pa_simple_get_latency`
/// returns when it fails (`pulse/sample.h`).
pub(super) const PA_USEC_INVALID: u64 = u64::MAX;

/// `pa_sample_spec` (`pulse/sample.h`): a sample format, rate and channel
/// count.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct SampleSpec {
    /// `pa_sample_format_t format`, such as [`PA_SAMPLE_S16LE`].
    pub format: c_int,
    /// `uint32_t rate`, in Hz.
    pub rate: u32,
    /// `uint8_t channels`.
    pub channels: u8,
}

/// `pa_buffer_attr` (`pulse/def.h`): the buffering asked of the server.
/// `u32::MAX`, the header's `(uint32_t) -1`, leaves a field to the
/// server.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct BufferAttr {
    /// `uint32_t maxlength`: the most the server buffers, in bytes.
    pub maxlength: u32,
    /// `uint32_t tlength`: playback only.
    pub tlength: u32,
    /// `uint32_t prebuf`: playback only.
    pub prebuf: u32,
    /// `uint32_t minreq`: playback only.
    pub minreq: u32,
    /// `uint32_t fragsize`: recording only, the size of each block the
    /// server sends, in bytes.
    pub fragsize: u32,
}

/// `pa_simple` (`pulse/simple.h`): an opaque connection, only ever
/// handled through a pointer. The marker makes it neither `Send`, `Sync`
/// nor `Unpin`, as the nomicon advises for an opaque foreign type.
#[repr(C)]
pub(super) struct PaSimple {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// `pa_simple_new` (`pulse/simple.h`). The channel map is a
/// `const pa_channel_map *`, which andris always passes as null, for the
/// default map, so it is declared opaque.
type SimpleNew = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_uint,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const SampleSpec,
    map: *const c_void,
    attr: *const BufferAttr,
    error: *mut c_int,
) -> *mut PaSimple;

/// `pa_simple_free` (`pulse/simple.h`).
type SimpleFree = unsafe extern "C" fn(s: *mut PaSimple);

/// `pa_simple_read` (`pulse/simple.h`): returns zero once `bytes` bytes
/// have been read, or a negative number, setting `error`.
type SimpleRead = unsafe extern "C" fn(
    s: *mut PaSimple,
    data: *mut c_void,
    bytes: usize,
    error: *mut c_int,
) -> c_int;

/// `pa_simple_get_latency` (`pulse/simple.h`): returns a `pa_usec_t`,
/// microseconds.
type SimpleGetLatency = unsafe extern "C" fn(s: *mut PaSimple, error: *mut c_int) -> u64;

/// `pa_strerror` (`pulse/error.h`), which lives in `libpulse.so.0`.
type Strerror = unsafe extern "C" fn(error: c_int) -> *const c_char;

/// Why libpulse could not be loaded: the library, or one of the symbols
/// andris needs from it, could not be found. It reads as the loader's
/// own error, which names the file.
#[derive(Debug, Clone)]
pub struct LoadError(String);

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

/// A failed call, as `pa_strerror` describes the error code it set.
#[derive(Debug)]
pub(super) struct PulseError {
    message: String,
}

impl fmt::Display for PulseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PulseError {}

/// libpulse's simple API, loaded. The function pointers are valid for as
/// long as the library is loaded, which is as long as this lives; the
/// one andris captures through lives for the whole process.
pub(super) struct Api {
    simple_new: SimpleNew,
    simple_free: SimpleFree,
    simple_read: SimpleRead,
    simple_get_latency: SimpleGetLatency,
    strerror: Strerror,
    _library: Library,
}

impl fmt::Debug for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Api").finish_non_exhaustive()
    }
}

impl Api {
    /// Load the library `file`, which is `libpulse-simple.so.0` but for
    /// tests, and look up the five functions.
    ///
    /// The library is opened with `RTLD_NOW`, so a library that cannot
    /// resolve a symbol it needs fails here, rather than killing the
    /// process with a symbol lookup error part way through a capture.
    /// `RTLD_LOCAL` keeps its symbols out of the global namespace. A
    /// lookup through a library's handle searches the library and the
    /// dependencies it brought in, so `pa_strerror` is found in
    /// `libpulse.so.0` through `libpulse-simple.so.0`'s handle.
    pub(super) fn open(file: &str) -> Result<Self, LoadError> {
        // SAFETY: loading a library runs its initialisers. libpulse's,
        // and those of the libraries it depends on, set up only their
        // own state, and ask nothing of the caller; dlopen itself is
        // thread-safe. Andris never unloads the library it captures
        // through (see *Stopping* in the parent module's documentation),
        // so its finalisers never run while a capture thread is inside
        // it.
        let library = unsafe { Library::open(Some(file), RTLD_NOW | RTLD_LOCAL) }
            .with_context(|| format!("opening {file}"))
            .map_err(|e| LoadError(format!("{e:#}")))?;
        // SAFETY: each symbol is looked up with the type its header
        // declares, transcribed above, as an `Option` of a function
        // pointer so that a null symbol is `None` rather than an invalid
        // function pointer. `Option<fn>` has a pointer's size and layout.
        unsafe {
            Ok(Self {
                simple_new: symbol(&library, "pa_simple_new")?,
                simple_free: symbol(&library, "pa_simple_free")?,
                simple_read: symbol(&library, "pa_simple_read")?,
                simple_get_latency: symbol(&library, "pa_simple_get_latency")?,
                strerror: symbol(&library, "pa_strerror")?,
                _library: library,
            })
        }
    }

    /// What `pa_strerror` says of `code`.
    pub(super) fn strerror(&self, code: c_int) -> String {
        // SAFETY: pa_strerror takes any int, and returns either null or a
        // pointer to a NUL-terminated string in libpulse's static data,
        // which lives as long as the library is loaded; the copy is made
        // before `self`, and so the library, can go.
        let message = unsafe { (self.strerror)(code) };
        if message.is_null() {
            return format!("unknown PulseAudio error {code}");
        }
        // SAFETY: as above, `message` is non-null, NUL-terminated and
        // valid for the length of this call.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }

    /// A [`PulseError`] for the error code a call set.
    fn error(&self, code: c_int) -> PulseError {
        PulseError {
            message: self.strerror(code),
        }
    }
}

/// The symbol `name` in `library`, as a `T`, which must be a function
/// pointer type.
///
/// # Safety
///
/// `T` must be the exact type of the function the library exports as
/// `name`.
unsafe fn symbol<T: Copy>(library: &Library, name: &str) -> Result<T, LoadError> {
    // SAFETY: the caller promises that `T` is the symbol's type, and
    // `Option<T>` of a function pointer is null for a null symbol.
    let found = unsafe { library.get::<Option<T>>(name) }
        .with_context(|| format!("looking up {name}"))
        .map_err(|e| LoadError(format!("{e:#}")))?;
    (*found).ok_or_else(|| LoadError(format!("looking up {name}: the symbol is null")))
}

/// One connection to a sound server, recording one stream, freed when
/// dropped. It borrows the [`Api`] it was made with, so the library
/// outlives it.
///
/// It is neither `Send` nor `Sync`: a capture makes, uses and frees its
/// connection on one thread.
pub(super) struct Simple<'a> {
    api: &'a Api,
    stream: NonNull<PaSimple>,
}

impl<'a> Simple<'a> {
    /// Connect to `server`, or the one libpulse finds if `None`, and
    /// record `source` in `spec`'s format with `attr`'s buffering. `name`
    /// and `stream_name` are what the server shows for the client and the
    /// stream.
    pub(super) fn record(
        api: &'a Api,
        server: Option<&CStr>,
        name: &CStr,
        source: &CStr,
        stream_name: &CStr,
        spec: &SampleSpec,
        attr: &BufferAttr,
    ) -> Result<Self, PulseError> {
        let mut error = PA_OK;
        // SAFETY: every pointer is either null where the header allows it
        // (`server` for the default server, and `map` for the default
        // channel map) or points at a live value of the declared type for
        // the whole call: the strings are NUL-terminated `CStr`s, `spec`
        // and `attr` are `#[repr(C)]` mirrors of the header's structs, and
        // `error` is a local `int`. libpulse copies what it keeps.
        let stream = unsafe {
            (api.simple_new)(
                server.map_or(ptr::null(), CStr::as_ptr),
                name.as_ptr(),
                PA_STREAM_RECORD,
                source.as_ptr(),
                stream_name.as_ptr(),
                spec,
                ptr::null(),
                attr,
                &mut error,
            )
        };
        let stream = NonNull::new(stream).ok_or_else(|| api.error(error))?;
        Ok(Self { api, stream })
    }

    /// Fill `bytes`, blocking until the server has sent that much.
    pub(super) fn read(&self, bytes: &mut [u8]) -> Result<(), PulseError> {
        let mut error = PA_OK;
        // SAFETY: `stream` is a live connection this owns, used only on
        // this thread; `bytes` is valid for writes of its whole length,
        // which is the length passed, and libpulse writes no more.
        let read = unsafe {
            (self.api.simple_read)(
                self.stream.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                &mut error,
            )
        };
        if read < 0 {
            return Err(self.api.error(error));
        }
        Ok(())
    }

    /// The record stream's latency: how long ago the next byte read was
    /// captured.
    pub(super) fn latency(&self) -> Result<Duration, PulseError> {
        let mut error = PA_OK;
        // SAFETY: `stream` is a live connection this owns, used only on
        // this thread, and `error` is a local `int`.
        let latency = unsafe { (self.api.simple_get_latency)(self.stream.as_ptr(), &mut error) };
        if error != PA_OK {
            return Err(self.api.error(error));
        }
        if latency == PA_USEC_INVALID {
            return Err(PulseError {
                message: "the sound server reported no latency".to_string(),
            });
        }
        Ok(Duration::from_micros(latency))
    }
}

impl Drop for Simple<'_> {
    fn drop(&mut self) {
        // SAFETY: `stream` came from pa_simple_new, has not been freed,
        // and is never used again: this is the only place it is freed,
        // and `self` goes with it.
        unsafe { (self.api.simple_free)(self.stream.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use std::mem::{align_of, offset_of, size_of};

    use libpulse_sys as sys;

    use super::*;

    #[test]
    fn constants_match_the_headers() {
        assert_eq!(PA_SAMPLE_S16LE, sys::PA_SAMPLE_S16LE as c_int);
        assert_eq!(PA_STREAM_RECORD, sys::PA_STREAM_RECORD as c_uint);
        assert_eq!(PA_OK, sys::PA_OK as c_int);
        assert_eq!(PA_USEC_INVALID, sys::PA_USEC_INVALID);
    }

    #[test]
    fn enums_are_int_sized() {
        assert_eq!(size_of::<sys::pa_sample_format_t>(), size_of::<c_int>());
        assert_eq!(size_of::<sys::pa_stream_direction_t>(), size_of::<c_uint>());
    }

    #[test]
    fn the_sample_spec_has_the_headers_layout() {
        assert_eq!(size_of::<SampleSpec>(), size_of::<sys::pa_sample_spec>());
        assert_eq!(align_of::<SampleSpec>(), align_of::<sys::pa_sample_spec>());
        assert_eq!(
            offset_of!(SampleSpec, format),
            offset_of!(sys::pa_sample_spec, format)
        );
        assert_eq!(
            offset_of!(SampleSpec, rate),
            offset_of!(sys::pa_sample_spec, rate)
        );
        assert_eq!(
            offset_of!(SampleSpec, channels),
            offset_of!(sys::pa_sample_spec, channels)
        );
    }

    #[test]
    fn the_buffer_attr_has_the_headers_layout() {
        assert_eq!(size_of::<BufferAttr>(), size_of::<sys::pa_buffer_attr>());
        assert_eq!(align_of::<BufferAttr>(), align_of::<sys::pa_buffer_attr>());
        assert_eq!(
            offset_of!(BufferAttr, maxlength),
            offset_of!(sys::pa_buffer_attr, maxlength)
        );
        assert_eq!(
            offset_of!(BufferAttr, tlength),
            offset_of!(sys::pa_buffer_attr, tlength)
        );
        assert_eq!(
            offset_of!(BufferAttr, prebuf),
            offset_of!(sys::pa_buffer_attr, prebuf)
        );
        assert_eq!(
            offset_of!(BufferAttr, minreq),
            offset_of!(sys::pa_buffer_attr, minreq)
        );
        assert_eq!(
            offset_of!(BufferAttr, fragsize),
            offset_of!(sys::pa_buffer_attr, fragsize)
        );
    }

    /// `pa_strerror`, looked up through `libpulse-simple.so.0`'s handle,
    /// is the function `libpulse-sys` links: the lookup reached
    /// `libpulse.so.0` through the dependency.
    #[test]
    fn strerror_is_found_through_the_simple_library() {
        let api = Api::open(crate::platform::pulse::LIBRARY).unwrap();
        for code in [PA_OK, sys::PA_ERR_CONNECTIONREFUSED as c_int, 1 << 20] {
            // SAFETY: pa_strerror takes any int, and returns null or a
            // static NUL-terminated string.
            let linked = unsafe { sys::pa_strerror(code) };
            let expected = if linked.is_null() {
                format!("unknown PulseAudio error {code}")
            } else {
                // SAFETY: non-null, NUL-terminated and static.
                unsafe { CStr::from_ptr(linked) }
                    .to_string_lossy()
                    .into_owned()
            };
            assert_eq!(api.strerror(code), expected);
        }
    }

    #[test]
    fn a_missing_library_is_a_load_error_naming_it() {
        let message = Api::open("libandris-no-such-library.so.0")
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("libandris-no-such-library.so.0"),
            "{message}"
        );
    }

    #[test]
    fn a_library_without_the_symbols_is_a_load_error_naming_one() {
        // The C library is always there, and has none of them.
        let message = Api::open("libc.so.6").unwrap_err().to_string();
        assert!(message.contains("pa_simple_new"), "{message}");
    }
}
