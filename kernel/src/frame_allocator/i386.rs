//! i386 implementation of the frame allocator.
//!
//! We define a frame as the same size as a page, to make things easy for us.
//!
//! It keeps tracks of the allocated frames by mean of a giant bitmap mapping every
//! physical memory frame in the address space to a bit representing if it is free or not.
//! This works because the address space in 32 bits is only 4GB, so ~1 million frames only
//!
//! During init we initialize the bitmap by parsing the information that the bootloader gives us and
//! marking some physical memory regions as reserved, either because of BIOS or MMIO.
//!
//! We also reserve everything that is mapped in KernelLand, assuming the bootstrap mapped it there
//! for us, and we don't want to overwrite it.
//!
//! We do not distinguish between reserved and occupied frames.

use super::{PhysicalMemRegion, PhysicalMemRegionIter,
            FrameAllocatorTrait, FrameAllocatorTraitPrivate};

use multiboot2::BootInformation;
use sync::SpinLock;
use alloc::vec::Vec;
use bit_field::BitArray;
use utils::{bit_array_first_one, BitArrayExt};
use mem::PhysicalAddress;
use mem::{round_to_page, round_to_page_upper, count_pages};
use paging::kernel_memory::get_kernel_memory;
use error::KernelError;
use failure::Backtrace;

/// A memory frame is the same size as a page
pub const MEMORY_FRAME_SIZE: usize = ::paging::PAGE_SIZE;

const FRAME_OFFSET_MASK: usize = 0xFFF;              // The offset part in a frame
const FRAME_BASE_MASK:   usize = !FRAME_OFFSET_MASK; // The base part in a frame

const FRAME_BASE_LOG: usize = 12; // frame_number = addr >> 12

/// The size of the frames_bitmap (~128ko)
const FRAMES_BITMAP_SIZE: usize = usize::max_value() / MEMORY_FRAME_SIZE / 8 + 1;

/// Gets the frame number from a physical address
#[inline]
fn addr_to_frame(addr: usize) -> usize {
    addr >> FRAME_BASE_LOG
}

/// Gets the physical address from a frame number
#[inline]
fn frame_to_addr(frame: usize) -> usize {
    frame << FRAME_BASE_LOG
}


pub struct FrameAllocatori386 {
    /// A big bitmap denoting for every frame if it is free or not
    ///
    /// 1 is free, 0 is already allocated/reserved
    /// This may seem backward, but this way when we start the array is filled with 0(reserved)
    /// and it can be put in the bss by the compiler
    memory_bitmap: [u8; FRAMES_BITMAP_SIZE],

    /// All operations have to check that the Allocator has been initialized
    initialized: bool
}

const FRAME_FREE:     bool = true;
const FRAME_OCCUPIED: bool = false;

/// A physical memory manger to allocate and free memory frames
static FRAME_ALLOCATOR : SpinLock<FrameAllocatori386> = SpinLock::new(FrameAllocatori386::new());

impl FrameAllocatori386 {
    pub const fn new() -> Self {
        FrameAllocatori386 {
            // 0 is allocated/reserved
            memory_bitmap: [0x00; FRAMES_BITMAP_SIZE],
            initialized: false
        }
    }
}

pub struct FrameAllocator;

impl FrameAllocatorTraitPrivate for FrameAllocator {
    /// Frees an allocated frame.
    ///
    /// # Panic
    ///
    /// Panics if the frame was not allocated
    /// Panics if FRAME_ALLOCATOR was not initialized
    fn free_region(region: &PhysicalMemRegion) {
        // don't bother taking the lock if there is no frames to free
        if region.frames > 0 {
            debug!("Freeing {:?}", region);
            assert!(Self::check_is_allocated(region), "PhysMemRegion beeing freed was not allocated");
            let mut allocator = FRAME_ALLOCATOR.lock();
            assert!(allocator.initialized, "The frame allocator was not initialized");
            allocator.memory_bitmap.set_bits_area(
                addr_to_frame(region.address().addr())
                    ..
                addr_to_frame(region.address().addr() + region.size()),
                FRAME_FREE);
        }
    }

    /// # Panic
    ///
    /// Panics if FRAME_ALLOCATOR was not initialized
    fn check_is_allocated(region: &PhysicalMemRegion) -> bool {
        let allocator = FRAME_ALLOCATOR.lock();
        assert!(allocator.initialized, "The frame allocator was not initialized");
        region.into_iter().all(|frame: PhysicalAddress| {
            let frame_index = addr_to_frame(frame.addr());
            allocator.memory_bitmap.get_bit(frame_index) == FRAME_OCCUPIED
        })
    }

    /// # Panic
    ///
    /// Panics if FRAME_ALLOCATOR was not initialized
    fn check_is_reserved(region: &PhysicalMemRegion) -> bool {
        // we have no way to distinguish between 'allocated' and 'reserved'
        Self::check_is_allocated(region)
    }
}

impl FrameAllocatorTrait for FrameAllocator {
    /// Allocates a single PhysicalMemRegion.
    /// Frames are physically consecutive.
    ///
    /// # Panic
    ///
    /// Panics if nr_frames == 0.
    /// Panics if FRAME_ALLOCATOR was not initialized.
    fn allocate_region(nr_frames: usize) -> Result<PhysicalMemRegion, KernelError> {
        let mut allocator = FRAME_ALLOCATOR.lock();
        assert!(nr_frames > 0, "The frame allocator cannot allocate zero-size region");
        assert!(allocator.initialized, "The frame allocator was not initialized");

        let mut start_index = 0usize;
        while start_index + nr_frames < allocator.memory_bitmap.bit_length() {
            let mut temp_len = 0usize;
            loop {
                match allocator.memory_bitmap.get_bit(start_index + temp_len) {
                    FRAME_OCCUPIED => {
                        // hole wasn't big enough, jump to its end
                        start_index += temp_len + 1;
                        temp_len = 0;
                        break;
                    }
                    FRAME_FREE => {
                        // hole is good til now, keep considering it
                        temp_len += 1;
                        if temp_len == nr_frames {
                            // the hole was big enough, allocate all of its frames, and return it
                            allocator.memory_bitmap.set_bits_area(start_index..start_index+temp_len, FRAME_OCCUPIED);
                            let allocated = PhysicalMemRegion {
                                start_addr: frame_to_addr(start_index),
                                frames: nr_frames,
                                should_free_on_drop: true
                            };
                            debug!("Allocated physical region: {:?}", allocated);
                            return Ok(allocated);
                        }
                    }
                }
            }
        }
        info!("Failed physical allocation for {} consecutive frames", nr_frames);
        Err(KernelError::PhysicalMemoryExhaustion { backtrace: Backtrace::new() })
    }

    /// Allocates `nr` physical frames, possibly fragmented across several physical regions.
    ///
    /// # Panic
    ///
    /// Panics if nr_frames == 0.
    /// Panics if FRAME_ALLOCATOR was not initialized
    fn allocate_frames_fragmented(requested: usize) -> Result<Vec<PhysicalMemRegion>, KernelError> {
        let mut allocator_lock = FRAME_ALLOCATOR.lock();
        assert!(requested > 0, "The frame allocator cannot allocate zero-size region");
        assert!(allocator_lock.initialized, "The frame allocator was not initialized");

        let mut collected_frames = 0;
        let mut collected_regions = Vec::new();
        let mut current_hole = PhysicalMemRegion { start_addr: 0, frames: 0, should_free_on_drop: true };
        // while requested is still obtainable.
        while addr_to_frame(current_hole.start_addr) + (requested - collected_frames) < allocator_lock.memory_bitmap.bit_length() {
            while current_hole.frames < requested - collected_frames {
                // compute current hole's size
                let considered_frame = addr_to_frame(current_hole.start_addr) + current_hole.frames;
                if allocator_lock.memory_bitmap.get_bit(considered_frame) == FRAME_FREE {
                    // expand current hole
                    allocator_lock.memory_bitmap.set_bit(considered_frame, FRAME_OCCUPIED);
                    current_hole.frames += 1;
                } else {
                    // we reached current hole's end
                    break;
                }
            }
            // we reached the end of the hole
            let next_hole = PhysicalMemRegion { start_addr: current_hole.start_addr + (current_hole.frames + 1) * MEMORY_FRAME_SIZE,
                                                frames: 0, should_free_on_drop: true };
            if current_hole.frames > 0 {
                // add it to our collected regions

                // droping the lock here, in case pushing this region in the collected regions
                // causes a heap expansion. This is ok, since we marked considered frames as allocated,
                // we're in a stable state. This ensures heap expansion won't take one of those.
                drop(allocator_lock);
                collected_frames += current_hole.frames;
                collected_regions.push(current_hole);
                if collected_frames == requested {
                    // we collected enough frames ! Succeed
                    debug!("Allocated physical regions: {:?}", collected_regions);
                    return Ok(collected_regions)
                }
                // re-take the lock. Still in a stable state, if heap-expansion
                // happened frames were marked allocated, and won't be given by this allocation
                allocator_lock = FRAME_ALLOCATOR.lock();
            }
            current_hole = next_hole;
        }
        drop(allocator_lock);
        info!("Failed physical allocation for {} non consecutive frames", requested);
        // collected_regions is dropped, marking them free again
        Err(KernelError::PhysicalMemoryExhaustion { backtrace: Backtrace::new() })
    }
}

/// Initialize the FrameAllocator by parsing the multiboot information
/// and marking some memory areas as unusable
pub fn init(boot_info: &BootInformation) {
    let mut allocator = FRAME_ALLOCATOR.lock();

    let memory_map_tag = boot_info.memory_map_tag()
        .expect("GRUB, you're drunk. Give us our memory_map_tag.");
    for memarea in memory_map_tag.memory_areas() {
        if memarea.start_address() > u32::max_value() as u64 || memarea.end_address() > u32::max_value() as u64 {
            continue;
        }
        mark_area_free(&mut allocator.memory_bitmap,
                                       memarea.start_address() as usize,
                                       memarea.end_address() as usize);
    }

    // Reserve everything mapped in KernelLand
    drop(allocator); // prevent deadlock
    get_kernel_memory().reserve_kernel_land_frames();
    let mut allocator = FRAME_ALLOCATOR.lock(); // retake the mutex

    // Don't free the modules. We need to keep the kernel around so we get symbols in panics!
    for module in boot_info.module_tags() {
        mark_area_reserved(&mut allocator.memory_bitmap,
                                           module.start_address() as usize, module.end_address() as usize);
    }

    // Reserve the very first frame for null pointers when paging is off
    mark_area_reserved(&mut allocator.memory_bitmap,
                                       0x00000000,
                                       0x00000001);

    if log_enabled!(::log::Level::Info) {
        let mut cur = None;
        for (i, bitmap) in allocator.memory_bitmap.iter().enumerate() {
            for j in 0..8 {
                let curaddr = (i * 8 + j) * ::paging::PAGE_SIZE;
                if bitmap & (1 << j) != 0 {
                    // Area is available
                    match cur {
                        None => cur = Some((FRAME_FREE, curaddr)),
                        Some((FRAME_OCCUPIED, last)) => {
                            info!("{:#010x} - {:#010x} OCCUPIED", last, curaddr);
                            cur = Some((FRAME_FREE, curaddr));
                        },
                        _ => ()
                    }
                } else {
                    // Area is occupied
                    match cur {
                        None => cur = Some((FRAME_OCCUPIED, curaddr)),
                        Some((FRAME_FREE, last)) => {
                            info!("{:#010x} - {:#010x} AVAILABLE", last, curaddr);
                            cur = Some((FRAME_OCCUPIED, curaddr));
                        },
                        _ => ()
                    }
                }
            }
        }
        match cur {
            Some((FRAME_FREE, last)) => info!("{:#010x} - {:#010x} AVAILABLE", last, 0xFFFFFFFFu32),
            Some((FRAME_OCCUPIED, last)) => info!("{:#010x} - {:#010x} OCCUPIED", last, 0xFFFFFFFFu32),
            _ => ()
        }
    }
    allocator.initialized = true
}

/// Marks a physical memory area as reserved and will never give it when requesting a frame.
/// This is used to mark where memory holes are, or where the kernel was mapped
///
/// # Panic
///
/// Does not panic if it overwrites an existing reservation
fn mark_area_reserved(bitmap: &mut [u8],
                      start_addr: usize,
                      end_addr: usize) {
    info!("Setting {:#010x}..{:#010x} to reserved", round_to_page(start_addr), round_to_page_upper(end_addr));
    bitmap.set_bits_area(
        addr_to_frame(round_to_page(start_addr))
            ..
        addr_to_frame(round_to_page_upper(end_addr)),
        FRAME_OCCUPIED);
}

/// Marks a physical memory area as free for frame allocation
///
/// # Panic
///
/// Does not panic if it overwrites an existing reservation
fn mark_area_free(bitmap: &mut [u8],
                  start_addr: usize,
                  end_addr: usize) {
    info!("Setting {:#010x}..{:#010x} to available", round_to_page(start_addr), round_to_page_upper(end_addr));
    bitmap.set_bits_area(
        addr_to_frame(round_to_page_upper(start_addr))
            ..
        addr_to_frame(round_to_page(end_addr)),
        FRAME_FREE);
}

/// Marks a physical memory frame as already allocated
/// Currently used during init when paging marks KernelLand frames as alloc'ed by bootstrap
///
/// # Panic
///
/// Panics if it overwrites an existing reservation
pub fn mark_frame_bootstrap_allocated(addr: PhysicalAddress) {
    debug!("Setting {:#010x} to boostrap allocked", addr.addr());
    assert_eq!(addr.addr() & FRAME_OFFSET_MASK, 0x000);
    let bit = addr_to_frame(addr.addr());
    let mut allocator = FRAME_ALLOCATOR.lock();
    if allocator.memory_bitmap.get_bit(bit) != FRAME_FREE {
        panic!("Frame being marked reserved was already allocated");
    }
    allocator.memory_bitmap.set_bit(bit, FRAME_OCCUPIED);
}