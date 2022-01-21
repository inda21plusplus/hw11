#![feature(once_cell)]

use libc::{self, c_int, c_void};
use spin::mutex::Mutex;
use std::alloc::{GlobalAlloc, Layout};
use std::{lazy::OnceCell, mem, ops::Range, ptr, slice};

pub struct FreelistAllocator;

struct AllocContext<'a> {
    alloc_list: MappingList<'a>,
    page_size: usize,
    alloc_size: usize,
}

#[repr(C)]
struct MappedMemory {
    // the pointer to the start of this mapped region
    start: *mut u8,
    // the total length
    len: usize,
    // the size of the largest sub-region in this mapping
    largest: usize,
    // start of the freelist
    first_free: *mut RegionHeader,
}

const REGION_START: [u8; 8] = *b"REGSTART";
const REG_END: [u8; 6] = *b"REGEND";

#[repr(C)]
struct RegionHeader {
    magic: [u8; 8],
    // previous region header, null if it's the first region in its
    // mapped region
    prev: *mut RegionHeader,
    // the next region, null if this is the last one
    next: *mut RegionHeader,
    // the size of the region. Includes the region header
    size: usize,
}

const TAG_START: [u8; 8] = *b"TAGSTART";
const TAG_END: [u8; 8] = *b"\0\0TAGEND";

#[repr(C)]
struct AllocationTag {
    // the byte string O C C U P I E D
    tag_start: [u8; 8],
    prev_free: *mut RegionHeader,
    size: usize,
    tag_end: [u8; 8],
}

struct AllocationLayout {
    start_ptr: *mut u8,
    tag_ptr: *mut AllocationTag,
    allocation_ptr: *mut u8,
    next_region_ptr: *mut RegionHeader,
}

unsafe fn mmap_new(size: usize) -> *mut u8 {
    libc::mmap(
        ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
        -1,
        0,
    ) as *mut u8
}

unsafe fn map_new_region(alloc_size: usize) -> MappedMemory {
    let ptr = mmap_new(alloc_size);

    make_region(
        ptr::null_mut(),
        ptr::null_mut(),
        ptr as *mut u8,
        (ptr as *mut u8).offset(alloc_size as isize),
    );

    MappedMemory {
        start: ptr,
        len: alloc_size,
        largest: alloc_size,
        first_free: ptr as *mut RegionHeader,
    }
}

unsafe fn make_allocation(region: *mut RegionHeader, layout: Layout) -> AllocationLayout {
    if region.is_null() {
        panic!("Passed null region header");
    }

    let RegionHeader {
        magic,
        next: next_reg,
        prev: prev_free,
        size: reg_size,
    } = *region;

    if magic != REGION_START {
        panic!("start of region didn't have the correct magic constant");
    }

    let tag_ptr = {
        let tag_padding = (region as *mut u8).align_offset(mem::align_of::<AllocationTag>());
        (region as *mut u8).offset(tag_padding as isize) as *mut AllocationTag
    };

    let alloc_start = (tag_ptr as *mut u8).offset(mem::size_of::<AllocationTag>() as isize);
    let allocation_ptr = {
        let offset = alloc_start.align_offset(layout.align());
        alloc_start.offset(offset as isize)
    };

    let next_reg_start = allocation_ptr.offset(layout.size() as isize);
    let next_region_ptr = {
        let offset = next_reg_start.align_offset(mem::align_of::<RegionHeader>());
        next_reg_start.offset(offset as isize) as *mut RegionHeader
    };

    let size = next_region_ptr as usize - tag_ptr as usize;
    *tag_ptr = AllocationTag {
        tag_start: REGION_START,
        prev_free,
        size,
        tag_end: TAG_END,
    };

    AllocationLayout {
        start_ptr: region as *mut u8,
        tag_ptr,
        allocation_ptr,
        next_region_ptr,
    }
}

// Safety: start has to be aligned the same as RegionHeader
unsafe fn make_region(
    prev: *mut RegionHeader,
    next: *mut RegionHeader,
    start: *mut u8,
    end: *mut u8,
) -> *mut RegionHeader {
    let size = end as usize - start as usize;
    *(start as *mut RegionHeader) = RegionHeader {
        magic: REGION_START,
        prev,
        next,
        size,
    };

    (end.offset(-6) as *mut [u8; 6]).write_unaligned(REG_END);

    start as *mut RegionHeader
}

fn min_region_size() -> usize {
    mem::size_of::<RegionHeader>() + mem::size_of_val(&REG_END)
}

unsafe fn init_alloc_context() -> AllocContext<'static> {
    let page_size = libc::sysconf(libc::_SC_PAGESIZE) as usize;
    let mut alloc_size = page_size;
    // 16 MiB
    while alloc_size < 16 * 1024 * 1024 {
        alloc_size *= 2;
    }

    // allocate memory for the allocation list
    let allocations_list = mmap_new(page_size);

    let list = slice::from_raw_parts_mut(allocations_list as *mut u8, page_size);

    list.fill(0);

    let (_, list, _) = list.align_to_mut::<MappedMemory>();

    // create allocation list
    let alloc_list = MappingList {
        next_list: ptr::null_mut(),
        list,
    };

    AllocContext {
        alloc_list,
        alloc_size,
        page_size,
    }
}

#[repr(C)]
struct MappingList<'a> {
    next_list: *mut MappingList<'a>,
    list: &'a mut [MappedMemory],
}

static mut CONTEXT: OnceCell<Mutex<AllocContext<'static>>> = OnceCell::new();

unsafe impl GlobalAlloc for FreelistAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut context = CONTEXT
            .get_or_init(|| Mutex::new(init_alloc_context()))
            .lock();

        let alloc_size = context.alloc_size;

        let mut selected_region = None;

        for mapping in context.alloc_list.list.iter_mut() {
            if mapping.start.is_null() {
                *mapping =
                    map_new_region(std::cmp::max(alloc_size, layout.size().next_power_of_two()));
                selected_region = Some(mapping)
            } else if mapping.largest > layout.size() {
                selected_region = Some(mapping);
                break;
            }
        }

        let mut selected_region = if let Some(reg) = selected_region {
            reg.first_free
        } else {
            return ptr::null_mut();
        };

        let RegionHeader {
            prev,
            next,
            size: region_size,
            ..
        } = *selected_region;
        let mut alloc_layout = make_allocation(selected_region, layout);
        if (alloc_layout.next_region_ptr as usize - alloc_layout.start_ptr as usize)
            < min_region_size()
        {
            (*alloc_layout.tag_ptr).size = region_size;
        } else {
            make_region(
                prev,
                next,
                alloc_layout.next_region_ptr as *mut u8,
                (selected_region as *mut u8).offset(region_size as isize),
            );
        }

        alloc_layout.allocation_ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        println!("buy more memory lol");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
