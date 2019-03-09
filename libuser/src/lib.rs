//! Userspace library
//!
//! Provides an allocator, various lang items.

#![no_std]
#![feature(global_asm, asm, start, lang_items, core_intrinsics, const_fn, alloc)]

// rustc warnings
#![warn(unused)]
#![warn(missing_debug_implementations)]
#![allow(unused_unsafe)]
#![allow(unreachable_code)]
#![allow(dead_code)]
#![cfg_attr(test, allow(unused_imports))]

#![allow(non_upper_case_globals)] // I blame roblabla.

// rustdoc warnings
#![warn(missing_docs)] // hopefully this will soon become deny(missing_docs)
#![deny(intra_doc_link_resolution_failure)]

#[macro_use]
extern crate alloc;


#[macro_use]
extern crate bitfield;


#[macro_use]
extern crate kfs_libutils;
use kfs_libkern;
#[macro_use]
extern crate failure;


#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;

pub mod caps;
pub mod syscalls;
pub mod types;
pub mod ipc;
pub mod sm;
pub mod vi;
pub mod error;
pub mod allocator;
pub mod terminal;
pub mod window;
mod log_impl;

pub use kfs_libutils::io;

use kfs_libutils as utils;
use crate::error::{Error, LibuserError};
 
// TODO: report #[cfg(not(test))] and #[global_allocator]
// BODY: `#[cfg(not(test))]` still compiles this item with cargo test,
// BODY: but `#[cfg(target_os = "none")] does not. I think this is a bug,
// BODY: we should report it.
/// Global allocator. Every implicit allocation in the rust liballoc library (for
/// instance for Vecs, Arcs, etc...) are allocated with this allocator.
#[cfg(target_os = "none")]
#[global_allocator]
static ALLOCATOR: allocator::Allocator = allocator::Allocator::new();

/// Finds a free memory zone of the given size and alignment in the current
/// process's virtual address space. Note that the address space is not reserved,
/// a call to map_memory to that address space might fail if another thread
/// maps to it first. It is recommended to use this function and the map syscall
/// in a loop.
///
/// # Panics
///
/// Panics on underflow when size = 0.
///
/// Panics on underflow when align = 0.
pub fn find_free_address(size: usize, align: usize) -> Result<usize, Error> {
    let mut addr = 0;
    // Go over the address space.
    loop {
        let (meminfo, _) = syscalls::query_memory(addr)?;
        if meminfo.memtype == kfs_libkern::MemoryType::Unmapped {
            let alignedbaseaddr = kfs_libutils::align_up_checked(meminfo.baseaddr, align).ok_or(LibuserError::AddressSpaceExhausted)?;

            let alignment = alignedbaseaddr - meminfo.baseaddr;
            if alignment.checked_add(size - 1).ok_or(LibuserError::AddressSpaceExhausted)? < meminfo.size {
                return Ok(alignedbaseaddr)
            }
        }
        addr = meminfo.baseaddr.checked_add(meminfo.size).ok_or(LibuserError::AddressSpaceExhausted)?;
    }
}

// Runtime functions
//
// Functions beyond this lines are required by the rust compiler when building
// for no_std. Care should be exercised when changing them, as lang items are
// extremely picky about how their types or implementation.

/// The exception handling personality function for use in the bootstrap.
///
/// We currently have no userspace exception handling, so make it do nothing.
#[cfg(target_os = "none")]
#[lang = "eh_personality"] #[no_mangle] pub extern fn eh_personality() {}

/// Function called on `panic!` invocation. Prints the panic information to the
/// kernel debug logger, and exits the process.
#[cfg(target_os = "none")]
#[panic_handler] #[no_mangle]
pub extern fn panic_fmt(p: &core::panic::PanicInfo<'_>) -> ! {
    let _ = syscalls::output_debug_string(&format!("{}", p));
    syscalls::exit_process();
}

use core::alloc::Layout;

// TODO: Don't panic in the oom handler, exit instead.
// BODY: Panicking may allocate, so calling panic in the OOM handler is a
// BODY: terrible idea.
/// OOM handler. Causes a panic.
#[cfg(target_os = "none")]
#[lang = "oom"]
#[no_mangle]
pub fn rust_oom(_: Layout) -> ! {
    panic!("OOM")
}

/// Executable entrypoint. Zeroes out the BSS, calls main, and finally exits the
/// process.
#[cfg(target_os = "none")]
#[no_mangle]
pub unsafe extern fn start() -> ! {
    asm!("
        // Memset the bss. Hopefully memset doesn't actually use the bss...
        lea eax, BSS_END
        lea ebx, BSS_START
        sub eax, ebx
        push eax
        push 0
        push ebx
        call memset
        add esp, 12
        " : : : : "intel", "volatile");

    extern {
        fn main(argc: isize, argv: *const *const u8) -> i32;
    }

    log_impl::init();
    let _ret = main(0, core::ptr::null());
    syscalls::exit_process();
}

/// A trait for implementing arbitrary return types in the `main` function.
///
/// The c-main function only supports to return integers as return type.
/// So, every type implementing the `Termination` trait has to be converted
/// to an integer.
///
/// The default implementations are returning 0 to indicate a successful
/// execution. In case of a failure, 1 is returned.
#[cfg(target_os = "none")]
#[lang = "termination"]
trait Termination {
    /// Is called to get the representation of the value as status code.
    /// This status code is returned to the operating system.
    fn report(self) -> i32;
}

#[cfg(target_os = "none")]
impl Termination for () {
    #[inline]
    fn report(self) -> i32 { 0 }
}

#[cfg(target_os = "none")]
#[lang = "start"]
#[allow(clippy::unit_arg)]
fn main<T: Termination>(main: fn(), _argc: isize, _argv: *const *const u8) -> isize {
    main().report() as isize
}
