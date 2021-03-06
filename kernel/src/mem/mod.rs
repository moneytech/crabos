use core::ptr;

use interface::SysError;

pub mod fault;
pub mod kalloc;
pub mod kvirt;
pub mod page;
pub mod phys;
pub mod user;

#[derive(Debug)]
pub struct MemoryExhausted;

impl From<MemoryExhausted> for SysError {
    fn from(_: MemoryExhausted) -> SysError {
        SysError::MemoryExhausted
    }
}

pub unsafe fn zero(ptr: *mut u8, bytes: usize) {
    ptr::write_bytes(ptr, 0, bytes);
}
