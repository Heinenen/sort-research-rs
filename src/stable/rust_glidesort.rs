use std::cmp::Ordering;
use std::mem::MaybeUninit;

use glidesort;

sort_impl!("rust_glidesort_stable");

pub fn sort<T: Ord>(data: &mut [T]) {

    let mut buffer: [MaybeUninit<T>; 131072] = unsafe { MaybeUninit::uninit().assume_init() };
	// let mut buffer: [MaybeUninit<T>; 8192] = [MaybeUninit::uninit(); 8192];
    glidesort::sort_with_buffer(data, &mut buffer[..]);
}

pub fn sort_by<T, F: FnMut(&T, &T) -> Ordering>(data: &mut [T], compare: F) {
    glidesort::sort_by(data, compare);
}
