use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::error;
use miniz_oxide::inflate::stream::InflateState;
use std::{mem::size_of, ptr};

const Z_MAGIC: u32 = u32::from_be_bytes(*b"ZSTB"); // new format, not zlib's internal layout

/// Serialize an InflateState snapshot into a buffer,
/// prefixed with a magic number and the length of the serialized data.
pub(crate) fn write_zlib_state(buf: &mut BytesMut, state: &InflateState) {
    buf.put_u32(Z_MAGIC);

    let len = size_of::<InflateState>();
    let encoded =
        unsafe { std::slice::from_raw_parts((state as *const InflateState).cast::<u8>(), len) };

    buf.put_u32(len as u32);
    buf.put_slice(encoded);
}

pub(crate) fn restore_zlib_state(buf: &mut Bytes) -> Option<Box<InflateState>> {
    if buf.get_u32() != Z_MAGIC {
        error!("Invalid magic number while reading zlib state");
        return None;
    }

    let len = buf.get_u32() as usize;
    if len != size_of::<InflateState>() {
        error!("Invalid InflateState size while reading zlib state: {len}");
        return None;
    }

    let encoded = buf.copy_to_bytes(len);
    let mut state = Box::<InflateState>::new(unsafe { std::mem::zeroed() });

    unsafe {
        ptr::copy_nonoverlapping(
            encoded.as_ptr(),
            (&mut *state as *mut InflateState).cast::<u8>(),
            len,
        );
    }

    Some(state)
}
