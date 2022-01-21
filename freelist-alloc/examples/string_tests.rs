use freelist_alloc::FreelistAllocator;
use std::{thread, time};

#[global_allocator]
static GLOBAL: FreelistAllocator = FreelistAllocator;

fn main() {
    let s = String::from("testing string");

    println!("{}", s);

    let ten_millis = time::Duration::from_millis(10000);
    let now = time::Instant::now();

    thread::sleep(ten_millis);
}
