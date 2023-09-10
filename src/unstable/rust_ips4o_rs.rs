use std::{cmp::Ordering, fmt::Debug};

sort_impl!("rust_ips4o_rs");

trait Ips4oSort: Sized {
    fn sort(data: &mut [Self]);
}

trait Ips4oSortBy<T>: Sized {
    fn sort_by(data: &mut [T], compare: Self);
}

trait Ips4oComparator<T> {}
impl <T, F: Fn(&T, &T) -> Ordering> Ips4oComparator<T> for F {}

impl<T> Ips4oSort for T {
    default fn sort(_data: &mut [Self]) {
        panic!("Type not supported.");
    }
}

impl<T, F> Ips4oSortBy<T> for F {
    default fn sort_by(_data: &mut [T], _compare: Self) {
        panic!("Type not supported.");
    }
}

impl<T: Ord + Clone + Debug + Default + Copy + Send + Sync> Ips4oSort for T {
    fn sort(data: &mut [Self]) {
        ips4o_rs::sort(data);
    }
}

impl<T: Clone + Debug + Default + Copy, F: Fn(&T, &T) -> Ordering> Ips4oSortBy<T> for F {
    fn sort_by(data: &mut [T], compare: Self) {
        // ips4o_rs::sort_by(data, compare);
    }
} 

pub fn sort<T: Ord>(data: &mut [T]) {
    <T as Ips4oSort>::sort(data);
}

pub fn sort_by<T, F: FnMut(&T, &T) -> Ordering>(data: &mut [T], compare: F) {
    <F as Ips4oSortBy<T>>::sort_by(data, compare);
}
