use std::{
    os::raw::{c_char, c_int, c_uint, c_ulong, c_ushort},
    ptr,
};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use libz_sys::{gz_headerp, z_stream as mz_stream, z_streamp};

pub const Z_ENOUGH_LENS: usize = 852;
pub const Z_ENOUGH_DISTS: usize = 592;
pub const Z_ENOUGH: usize = Z_ENOUGH_LENS + Z_ENOUGH_DISTS;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ZCode {
    pub op: u8,
    pub bits: u8,
    pub val: c_ushort,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ZInflateState {
    pub strm: z_streamp,
    pub inflate_mode: c_uint,

    pub last: c_int,
    pub wrap: c_int,

    pub havedict: c_int,
    pub flags: c_int,

    pub dmax: c_uint,
    pub check: c_ulong,
    pub total: c_ulong,
    pub head: gz_headerp,

    pub wbits: c_uint,
    pub wsize: c_uint,
    pub whave: c_uint,
    pub wnext: c_uint,
    pub window: *mut c_char,

    pub hold: c_ulong,
    pub bits: c_uint,

    pub length: c_uint,
    pub offset: c_uint,
    pub extra: c_uint,

    pub lencode: *mut ZCode,
    pub distcode: *mut ZCode,

    pub lenbits: c_uint,
    pub distbits: c_uint,

    pub ncode: c_uint,
    pub nlen: c_uint,
    pub ndist: c_uint,
    pub have: c_uint,
    pub next: *mut ZCode,

    pub lens: [c_ushort; 320],
    pub work: [c_ushort; 288],

    pub codes: [ZCode; Z_ENOUGH],

    pub sane: c_int,
    pub back: c_int,
    pub was: c_uint,
}

pub fn write_zlib_state(buf: &mut BytesMut, stream: &mut mz_stream) {
    // `as u64` casts: c_ulong is u32 on Windows, u64 on 64-bit Unix.
    buf.put_u64(stream.total_in as u64);
    buf.put_u64(stream.total_out as u64);
    buf.put_i32(stream.data_type);
    buf.put_u64(stream.adler as u64);

    let state = stream.state as *mut ZInflateState;
    assert!(!state.is_null(), "zlib stream has no inflate state");
    let state_ref = unsafe { &*state };

    let size = size_of::<ZInflateState>();
    let mut blob = vec![0u8; size];
    unsafe {
        ptr::copy_nonoverlapping(state as *const u8, blob.as_mut_ptr(), size);
    }
    buf.extend_from_slice(&blob);

    if !state_ref.window.is_null() {
        let window_size = 1usize << state_ref.wbits;
        let window =
            unsafe { std::slice::from_raw_parts(state_ref.window as *const u8, window_size) };
        buf.extend_from_slice(window);
    }

    let base = state_ref.codes.as_ptr();
    let lencode_index = unsafe { state_ref.lencode.offset_from(base) };
    let distcode_index = unsafe { state_ref.distcode.offset_from(base) };
    let next_index = unsafe { state_ref.next.offset_from(base) };

    for (name, idx) in [
        ("lencode", lencode_index),
        ("distcode", distcode_index),
        ("next", next_index),
    ] {
        assert!(
            (0..Z_ENOUGH as isize).contains(&idx),
            "Can't serialize this zlib state, {name} out of range \
             (lencode: {lencode_index}, distcode: {distcode_index}, next: {next_index})"
        );
    }

    buf.put_u32(lencode_index as u32);
    buf.put_u32(distcode_index as u32);
    buf.put_u32(next_index as u32);

    buf.put_u32(state_ref.lenbits);
    buf.put_u32(state_ref.distbits);
}

pub fn restore_zlib_state(buf: &mut Bytes, stream: &mut mz_stream) {
    stream.total_in = buf.get_u64() as _;
    stream.total_out = buf.get_u64() as _;
    stream.data_type = buf.get_i32();
    stream.adler = buf.get_u64() as _;

    let state = stream.state as *mut ZInflateState;
    assert!(!state.is_null(), "zlib stream has no inflate state");

    let size = size_of::<ZInflateState>();
    let blob = buf.copy_to_bytes(size);
    unsafe {
        ptr::copy_nonoverlapping(blob.as_ptr(), state as *mut u8, size);
        (*state).strm = stream;
    }

    let state_ref = unsafe { &mut *state };

    if !state_ref.window.is_null() {
        let window_size = 1usize << state_ref.wbits;
        let window = buf.copy_to_bytes(window_size);

        // libz-sys wraps C function pointers in Option;
        let zalloc = stream.zalloc;
        state_ref.window =
            unsafe { zalloc(stream.opaque, 1, window_size as c_uint) as *mut c_char };
        assert!(!state_ref.window.is_null(), "zalloc failed for inflate window");

        unsafe {
            ptr::copy_nonoverlapping(
                window.as_ptr(),
                state_ref.window as *mut u8,
                window_size,
            );
        }
    }

    let lencode = buf.get_u32() as isize;
    let distcode = buf.get_u32() as isize;
    let nextcode = buf.get_u32() as isize;

    for (name, idx) in [("lencode", lencode), ("distcode", distcode), ("next", nextcode)] {
        assert!(
            (0..Z_ENOUGH as isize).contains(&idx),
            "Can't deserialize this zlib state, {name} out of range"
        );
    }

    let base = state_ref.codes.as_mut_ptr();
    state_ref.lencode = unsafe { base.offset(lencode) };
    state_ref.distcode = unsafe { base.offset(distcode) };
    state_ref.next = unsafe { base.offset(nextcode) };

    state_ref.lenbits = buf.get_u32();
    state_ref.distbits = buf.get_u32();
}