#![allow(unused)]

//! Instruction-Parallel-Network Unstable Sort, ipnsort by Lukas Bergdoll

use core::cmp::{self, Ordering};
use core::intrinsics;
use core::mem::{self, MaybeUninit};
use core::ptr;

sort_impl!("rust_ipnsort_unstable");

/// Sorts the slice, but might not preserve the order of equal elements.
///
/// This sort is unstable (i.e., may reorder equal elements), in-place
/// (i.e., does not allocate), and *O*(*n* \* log(*n*)) worst-case.
///
/// # Current implementation
///
/// The current algorithm is based on [pattern-defeating quicksort][pdqsort] by Orson Peters,
/// which combines the fast average case of randomized quicksort with the fast worst case of
/// heapsort, while achieving linear time on slices with certain patterns. It uses some
/// randomization to avoid degenerate cases, but with a fixed seed to always provide
/// deterministic behavior.
///
/// It is typically faster than stable sorting, except in a few special cases, e.g., when the
/// slice consists of several concatenated sorted sequences.
///
/// # Examples
///
/// ```
/// let mut v = [-5, 4, 1, -3, 2];
///
/// v.sort_unstable();
/// assert!(v == [-5, -3, 1, 2, 4]);
/// ```
///
/// [pdqsort]: https://github.com/orlp/pdqsort
#[inline(always)]
pub fn sort<T>(arr: &mut [T])
where
    T: Ord,
{
    quicksort(arr, |a, b| a.lt(b));
}

/// Sorts the slice with a comparator function, but might not preserve the order of equal
/// elements.
///
/// This sort is unstable (i.e., may reorder equal elements), in-place
/// (i.e., does not allocate), and *O*(*n* \* log(*n*)) worst-case.
///
/// The comparator function must define a total ordering for the elements in the slice. If
/// the ordering is not total, the order of the elements is unspecified. An order is a
/// total order if it is (for all `a`, `b` and `c`):
///
/// * total and antisymmetric: exactly one of `a < b`, `a == b` or `a > b` is true, and
/// * transitive, `a < b` and `b < c` implies `a < c`. The same must hold for both `==` and `>`.
///
/// For example, while [`f64`] doesn't implement [`Ord`] because `NaN != NaN`, we can use
/// `partial_cmp` as our sort function when we know the slice doesn't contain a `NaN`.
///
/// ```
/// let mut floats = [5f64, 4.0, 1.0, 3.0, 2.0];
/// floats.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
/// assert_eq!(floats, [1.0, 2.0, 3.0, 4.0, 5.0]);
/// ```
///
/// # Current implementation
///
/// The current algorithm is based on [pattern-defeating quicksort][pdqsort] by Orson Peters,
/// which combines the fast average case of randomized quicksort with the fast worst case of
/// heapsort, while achieving linear time on slices with certain patterns. It uses some
/// randomization to avoid degenerate cases, but with a fixed seed to always provide
/// deterministic behavior.
///
/// It is typically faster than stable sorting, except in a few special cases, e.g., when the
/// slice consists of several concatenated sorted sequences.
///
/// # Examples
///
/// ```
/// let mut v = [5, 4, 1, 3, 2];
/// v.sort_unstable_by(|a, b| a.cmp(b));
/// assert!(v == [1, 2, 3, 4, 5]);
///
/// // reverse sorting
/// v.sort_unstable_by(|a, b| b.cmp(a));
/// assert!(v == [5, 4, 3, 2, 1]);
/// ```
///
/// [pdqsort]: https://github.com/orlp/pdqsort
#[inline(always)]
pub fn sort_by<T, F>(arr: &mut [T], mut compare: F)
where
    F: FnMut(&T, &T) -> Ordering,
{
    quicksort(arr, |a, b| compare(a, b) == Ordering::Less);
}

// --- IMPL ---

/// Sorts `v` using pattern-defeating quicksort, which is *O*(*n* \* log(*n*)) worst-case.
#[cfg_attr(feature = "no_inline_sub_functions", inline(never))]
pub fn quicksort<T, F>(v: &mut [T], mut is_less: F)
where
    F: FnMut(&T, &T) -> bool,
{
    // Sorting has no meaningful behavior on zero-sized types.
    if const { mem::size_of::<T>() == 0 } {
        return;
    }

    let len = v.len();

    // This path is critical for very small inputs. Always pick insertion sort for these inputs,
    // without any other analysis. This is perf critical for small inputs, in cold code.
    const MAX_LEN_ALWAYS_INSERTION_SORT: usize = 20;

    // Instrumenting the standard library showed that 90+% of the calls to sort by rustc are either
    // of size 0 or 1. Make this path extra fast by assuming the branch is likely.
    if intrinsics::likely(len < 2) {
        return;
    }

    // It's important to differentiate between small-sort performance for small slices and
    // small-sort performance sorting small sub-slices as part of the main quicksort loop. For the
    // former, testing showed that the representative benchmarks for real-world performance are cold
    // CPU state and not single-size hot benchmarks. For the latter the CPU will call them many
    // times, so hot benchmarks are fine and more realistic. And it's worth it to optimize sorting
    // small sub-slices with more sophisticated solutions than insertion sort.

    if intrinsics::likely(len <= MAX_LEN_ALWAYS_INSERTION_SORT) {
        // More specialized and faster options, extending the range of allocation free sorting
        // are possible but come at a great cost of additional code, which is problematic for
        // compile-times.
        insertion_sort_shift_left(v, 1, &mut is_less);

        return;
    }

    let (streak_end, was_reversed) = find_streak(v, &mut is_less);
    if streak_end == len {
        if was_reversed {
            v.reverse();
        }

        // TODO if streak_end >= len / 2 | quicksort the rest and merge via rotation merge.

        return;
    }

    // Limit the number of imbalanced partitions to `2 * floor(log2(len))`.
    // The binary OR by one is used to eliminate the zero-check in the logarithm.
    let limit = 2 * (len | 1).ilog2();

    recurse(v, &mut is_less, None, limit);
}

/// Finds a streak of presorted elements starting at the beginning of the slice. Returns the first
/// value that is not part of said streak, and a bool denoting wether the streak was reversed.
/// Streaks can be increasing or decreasing.
fn find_streak<T, F>(v: &[T], is_less: &mut F) -> (usize, bool)
where
    F: FnMut(&T, &T) -> bool,
{
    let len = v.len();

    if len < 2 {
        return (len, false);
    }

    let mut end = 2;

    // SAFETY: See below specific.
    unsafe {
        // SAFETY: We checked that len >= 2, so 0 and 1 are valid indices.
        let assume_reverse = is_less(v.get_unchecked(1), v.get_unchecked(0));

        // SAFETY: We know end >= 2 and check end < len.
        // From that follows that accessing v at end and end - 1 is safe.
        if assume_reverse {
            while end < len && is_less(v.get_unchecked(end), v.get_unchecked(end - 1)) {
                end += 1;
            }

            (end, true)
        } else {
            while end < len && !is_less(v.get_unchecked(end), v.get_unchecked(end - 1)) {
                end += 1;
            }
            (end, false)
        }
    }
}

/// Sorts `v` using heapsort, which guarantees *O*(*n* \* log(*n*)) worst-case.
///
/// Never inline this, it sits the main hot-loop in `recurse` and is meant as unlikely algorithmic
/// fallback.
#[inline(never)]
pub fn heapsort<T, F>(v: &mut [T], is_less: &mut F)
where
    F: FnMut(&T, &T) -> bool,
{
    // This binary heap respects the invariant `parent >= child`.
    let mut sift_down = |v: &mut [T], mut node| {
        loop {
            // Children of `node`.
            let mut child = 2 * node + 1;
            if child >= v.len() {
                break;
            }

            // Choose the greater child.
            if child + 1 < v.len() {
                // We need a branch to be sure not to out-of-bounds index,
                // but it's highly predictable.  The comparison, however,
                // is better done branchless, especially for primitives.
                child += is_less(&v[child], &v[child + 1]) as usize;
            }

            // Stop if the invariant holds at `node`.
            if !is_less(&v[node], &v[child]) {
                break;
            }

            // Swap `node` with the greater child, move one step down, and continue sifting.
            v.swap(node, child);
            node = child;
        }
    };

    // Build the heap in linear time.
    for i in (0..v.len() / 2).rev() {
        sift_down(v, i);
    }

    // Pop maximal elements from the heap.
    for i in (1..v.len()).rev() {
        v.swap(0, i);
        sift_down(&mut v[..i], 0);
    }
}

/// TODO explain
#[cfg_attr(feature = "no_inline_sub_functions", inline(never))]
#[inline(always)]
unsafe fn swap_elements_between_blocks<T>(
    l_ptr: *mut T,
    r_ptr: *mut T,
    mut l_offsets_ptr: *const u8,
    mut r_offsets_ptr: *const u8,
    count: usize,
) -> (*const u8, *const u8) {
    macro_rules! left {
        () => {
            l_ptr.add(*l_offsets_ptr as usize)
        };
    }
    macro_rules! right {
        () => {
            r_ptr.sub(*r_offsets_ptr as usize + 1)
        };
    }

    if count <= 1 {
        if count == 1 {
            // SAFETY: TODO
            unsafe {
                ptr::swap_nonoverlapping(left!(), right!(), 1);
                l_offsets_ptr = l_offsets_ptr.add(1);
                r_offsets_ptr = r_offsets_ptr.add(1);
            }
        }

        return (l_offsets_ptr, r_offsets_ptr);
    }

    // Instead of swapping one pair at the time, it is more efficient to perform a cyclic
    // permutation. This is not strictly equivalent to swapping, but produces a similar
    // result using fewer memory operations.
    //
    // Example cyclic permutation to swap A,B,C,D with W,X,Y,Z
    //
    // A -> TMP
    // Z -> A   | Z,B,C,D ___ W,X,Y,Z
    //
    // Loop iter 1
    // B -> Z   | Z,B,C,D ___ W,X,Y,B
    // Y -> B   | Z,Y,C,D ___ W,X,Y,B
    //
    // Loop iter 2
    // C -> Y   | Z,Y,C,D ___ W,X,C,B
    // X -> C   | Z,Y,X,D ___ W,X,C,B
    //
    // Loop iter 3
    // D -> X   | Z,Y,X,D ___ W,D,C,B
    // W -> D   | Z,Y,X,W ___ W,D,C,B
    //
    // TMP -> W | Z,Y,X,W ___ A,D,C,B

    // SAFETY: The use of `ptr::read` is valid because there is at least one element in
    // both `offsets_l` and `offsets_r`, so `left!` is a valid pointer to read from.
    //
    // The uses of `left!` involve calls to `offset` on `l`, which points to the
    // beginning of `v`. All the offsets pointed-to by `l_offsets_ptr` are at most `block_l`, so
    // these `offset` calls are safe as all reads are within the block. The same argument
    // applies for the uses of `right!`.
    //
    // The calls to `l_offsets_ptr.offset` are valid because there are at most `count-1` of them,
    // plus the final one at the end of the unsafe block, where `count` is the minimum number
    // of collected offsets in `offsets_l` and `offsets_r`, so there is no risk of there not
    // being enough elements. The same reasoning applies to the calls to `r_offsets_ptr.offset`.
    //
    // The calls to `copy_nonoverlapping` are safe because `left!` and `right!` are guaranteed
    // not to overlap, and are valid because of the reasoning above.
    unsafe {
        let tmp = ptr::read(left!());
        ptr::copy_nonoverlapping(right!(), left!(), 1);

        for _ in 1..count {
            l_offsets_ptr = l_offsets_ptr.add(1);
            ptr::copy_nonoverlapping(left!(), right!(), 1);
            r_offsets_ptr = r_offsets_ptr.add(1);
            ptr::copy_nonoverlapping(right!(), left!(), 1);
        }

        ptr::copy_nonoverlapping(&tmp, right!(), 1);
        mem::forget(tmp);
        l_offsets_ptr = l_offsets_ptr.add(1);
        r_offsets_ptr = r_offsets_ptr.add(1);
    }

    (l_offsets_ptr, r_offsets_ptr)
}

/// Partitions `v` into elements smaller than `pivot`, followed by elements greater than or equal
/// to `pivot`.
///
/// Returns the number of elements smaller than `pivot`.
///
/// Partitioning is performed block-by-block in order to minimize the cost of branching operations.
/// This idea is presented in the [BlockQuicksort][pdf] paper.
///
/// [pdf]: https://drops.dagstuhl.de/opus/volltexte/2016/6389/pdf/LIPIcs-ESA-2016-38.pdf
#[cfg_attr(feature = "no_inline_sub_functions", inline(never))]
fn partition_in_blocks<T, F>(v: &mut [T], pivot: &T, is_less: &mut F) -> usize
where
    F: FnMut(&T, &T) -> bool,
{
    // Number of elements in a typical block.
    const BLOCK: usize = 2usize.pow(u8::BITS);

    // The partitioning algorithm repeats the following steps until completion:
    //
    // 1. Trace a block from the left side to identify elements greater than or equal to the pivot.
    // 2. Trace a block from the right side to identify elements smaller than the pivot.
    // 3. Exchange the identified elements between the left and right side.
    //
    // We keep the following variables for a block of elements:
    //
    // 1. `block` - Number of elements in the block.
    // 2. `start` - Start pointer into the `offsets` array.
    // 3. `end` - End pointer into the `offsets` array.
    // 4. `offsets - Indices of out-of-order elements within the block.

    // The current block on the left side (from `l` to `l.add(block_l)`).
    let mut l = v.as_mut_ptr();
    let mut block_l = BLOCK;
    let mut start_l = ptr::null_mut();
    let mut end_l = ptr::null_mut();
    let mut offsets_l = [MaybeUninit::<u8>::uninit(); BLOCK];

    // The current block on the right side (from `r.sub(block_r)` to `r`).
    // SAFETY: The documentation for .add() specifically mention that `vec.as_ptr().add(vec.len())` is always safe`
    let mut r = unsafe { l.add(v.len()) };
    let mut block_r = BLOCK;
    let mut start_r = ptr::null_mut();
    let mut end_r = ptr::null_mut();
    let mut offsets_r = [MaybeUninit::<u8>::uninit(); BLOCK];

    // FIXME: When we get VLAs, try creating one array of length `min(v.len(), 2 * BLOCK)` rather
    // than two fixed-size arrays of length `BLOCK`. VLAs might be more cache-efficient.

    // Returns the number of elements between pointers `l` (inclusive) and `r` (exclusive).
    fn width<T>(l: *const T, r: *const T) -> usize {
        debug_assert!(r.addr() >= l.addr());

        unsafe { r.sub_ptr(l) }
    }

    loop {
        // We are done with partitioning block-by-block when `l` and `r` get very close. Then we do
        // some patch-up work in order to partition the remaining elements in between.
        let is_done = width(l, r) <= 2 * BLOCK;

        if is_done {
            // Number of remaining elements (still not compared to the pivot).
            let mut rem = width(l, r);
            if start_l < end_l || start_r < end_r {
                rem -= BLOCK;
            }

            // Adjust block sizes so that the left and right block don't overlap, but get perfectly
            // aligned to cover the whole remaining gap.
            if start_l < end_l {
                block_r = rem;
            } else if start_r < end_r {
                block_l = rem;
            } else {
                // There were the same number of elements to switch on both blocks during the last
                // iteration, so there are no remaining elements on either block. Cover the remaining
                // items with roughly equally-sized blocks.
                block_l = rem / 2;
                block_r = rem - block_l;
            }
            debug_assert!(block_l <= BLOCK && block_r <= BLOCK);
            debug_assert!(width(l, r) == block_l + block_r);
        }

        if start_l == end_l {
            // Trace `block_l` elements from the left side.
            start_l = MaybeUninit::slice_as_mut_ptr(&mut offsets_l);
            end_l = start_l;
            let mut elem = l;

            for i in 0..block_l {
                // SAFETY: The unsafety operations below involve the usage of the `offset`.
                //         According to the conditions required by the function, we satisfy them because:
                //         1. `offsets_l` is stack-allocated, and thus considered separate allocated object.
                //         2. The function `is_less` returns a `bool`.
                //            Casting a `bool` will never overflow `isize`.
                //         3. We have guaranteed that `block_l` will be `<= BLOCK`.
                //            Plus, `end_l` was initially set to the begin pointer of `offsets_` which was declared on the stack.
                //            Thus, we know that even in the worst case (all invocations of `is_less` returns false) we will only be at most 1 byte pass the end.
                //        Another unsafety operation here is dereferencing `elem`.
                //        However, `elem` was initially the begin pointer to the slice which is always valid.
                unsafe {
                    // Branchless comparison.
                    *end_l = i as u8;
                    end_l = end_l.wrapping_add(!is_less(&*elem, pivot) as usize);
                    elem = elem.add(1);
                }
            }
        }

        if start_r == end_r {
            // Trace `block_r` elements from the right side.
            start_r = MaybeUninit::slice_as_mut_ptr(&mut offsets_r);
            end_r = start_r;
            let mut elem = r;

            for i in 0..block_r {
                // SAFETY: The unsafety operations below involve the usage of the `offset`.
                //         According to the conditions required by the function, we satisfy them because:
                //         1. `offsets_r` is stack-allocated, and thus considered separate allocated object.
                //         2. The function `is_less` returns a `bool`.
                //            Casting a `bool` will never overflow `isize`.
                //         3. We have guaranteed that `block_r` will be `<= BLOCK`.
                //            Plus, `end_r` was initially set to the begin pointer of `offsets_` which was declared on the stack.
                //            Thus, we know that even in the worst case (all invocations of `is_less` returns true) we will only be at most 1 byte pass the end.
                //        Another unsafety operation here is dereferencing `elem`.
                //        However, `elem` was initially `1 * sizeof(T)` past the end and we decrement it by `1 * sizeof(T)` before accessing it.
                //        Plus, `block_r` was asserted to be less than `BLOCK` and `elem` will therefore at most be pointing to the beginning of the slice.
                unsafe {
                    // Branchless comparison.
                    elem = elem.sub(1);
                    *end_r = i as u8;
                    end_r = end_r.wrapping_add(is_less(&*elem, pivot) as usize);
                }
            }
        }

        // Number of out-of-order elements to swap between the left and right side.
        let count = cmp::min(width(start_l, end_l), width(start_r, end_r));

        // SAFETY: TODO
        unsafe {
            (start_l, start_r) = mem::transmute::<(*const u8, *const u8), (*mut u8, *mut u8)>(
                swap_elements_between_blocks(l, r, start_l, start_r, count),
            );
        }

        if start_l == end_l {
            // All out-of-order elements in the left block were moved. Move to the next block.

            // block-width-guarantee
            // SAFETY: if `!is_done` then the slice width is guaranteed to be at least `2*BLOCK` wide. There
            // are at most `BLOCK` elements in `offsets_l` because of its size, so the `offset` operation is
            // safe. Otherwise, the debug assertions in the `is_done` case guarantee that
            // `width(l, r) == block_l + block_r`, namely, that the block sizes have been adjusted to account
            // for the smaller number of remaining elements.
            l = unsafe { l.add(block_l) };
        }

        if start_r == end_r {
            // All out-of-order elements in the right block were moved. Move to the previous block.

            // SAFETY: Same argument as [block-width-guarantee]. Either this is a full block `2*BLOCK`-wide,
            // or `block_r` has been adjusted for the last handful of elements.
            r = unsafe { r.sub(block_r) };
        }

        if is_done {
            break;
        }
    }

    // All that remains now is at most one block (either the left or the right) with out-of-order
    // elements that need to be moved. Such remaining elements can be simply shifted to the end
    // within their block.

    if start_l < end_l {
        // The left block remains.
        // Move its remaining out-of-order elements to the far right.
        debug_assert_eq!(width(l, r), block_l);
        while start_l < end_l {
            // remaining-elements-safety
            // SAFETY: while the loop condition holds there are still elements in `offsets_l`, so it
            // is safe to point `end_l` to the previous element.
            //
            // The `ptr::swap` is safe if both its arguments are valid for reads and writes:
            //  - Per the debug assert above, the distance between `l` and `r` is `block_l`
            //    elements, so there can be at most `block_l` remaining offsets between `start_l`
            //    and `end_l`. This means `r` will be moved at most `block_l` steps back, which
            //    makes the `r.offset` calls valid (at that point `l == r`).
            //  - `offsets_l` contains valid offsets into `v` collected during the partitioning of
            //    the last block, so the `l.offset` calls are valid.
            unsafe {
                end_l = end_l.sub(1);
                ptr::swap(l.add(*end_l as usize), r.sub(1));
                r = r.sub(1);
            }
        }
        width(v.as_mut_ptr(), r)
    } else if start_r < end_r {
        // The right block remains.
        // Move its remaining out-of-order elements to the far left.
        debug_assert_eq!(width(l, r), block_r);
        while start_r < end_r {
            // SAFETY: See the reasoning in [remaining-elements-safety].
            unsafe {
                end_r = end_r.sub(1);
                ptr::swap(l, r.sub(*end_r as usize + 1));
                l = l.add(1);
            }
        }
        width(v.as_mut_ptr(), l)
    } else {
        // Nothing else to do, we're done.
        width(v.as_mut_ptr(), l)
    }
}

struct FulcrumState<T> {
    r_ptr: *mut T,
    x_ptr: *mut T,
    elem_i: usize,
}

#[inline(always)]
unsafe fn fulcrum_rotate<T, F>(
    arr_ptr: *mut T,
    state: &mut FulcrumState<T>,
    offset_val: isize,
    loop_len: usize,
    pivot: &T,
    is_less: &mut F,
) where
    F: FnMut(&T, &T) -> bool,
{
    for _ in 0..loop_len {
        let is_l = is_less(&*state.x_ptr, pivot);
        let target_ptr = if is_l {
            arr_ptr.add(state.elem_i)
        } else {
            state.r_ptr.add(state.elem_i)
        };
        ptr::copy(state.x_ptr, target_ptr, 1);
        state.elem_i += is_l as usize;
        state.x_ptr = state.x_ptr.wrapping_offset(offset_val);
        state.r_ptr = state.r_ptr.wrapping_sub(1);
    }
}

// Inspired by Igor van den Hoven and his work in quadsort/crumsort.
// TODO document.
fn fulcrum_partition_impl<T, F, const ROTATION_ELEMS: usize>(
    v: &mut [T],
    pivot: &T,
    is_less: &mut F,
) -> usize
where
    T: Freeze,
    F: FnMut(&T, &T) -> bool,
{
    // TODO explain ideas. and panic safety. cleanup.
    let len = v.len();

    const SWAP_SIZE: usize = 64;

    assert!(len >= (ROTATION_ELEMS * 2) && ROTATION_ELEMS <= 32);

    let advance_left = |a_ptr: *const T, arr_ptr: *const T, elem_i: usize| -> bool {
        // SAFETY: TODO
        unsafe { (a_ptr.sub_ptr(arr_ptr) - elem_i) <= ROTATION_ELEMS }
    };

    let mut swap = MaybeUninit::<[T; SWAP_SIZE]>::uninit();
    let swap_ptr = swap.as_mut_ptr() as *mut T;

    let arr_ptr = v.as_mut_ptr();

    // SAFETY: TODO
    unsafe {
        ptr::copy_nonoverlapping(arr_ptr, swap_ptr, ROTATION_ELEMS);
        ptr::copy_nonoverlapping(
            arr_ptr.add(len - ROTATION_ELEMS),
            swap_ptr.add(ROTATION_ELEMS),
            ROTATION_ELEMS,
        );

        let mut state = FulcrumState {
            r_ptr: arr_ptr.add(len - 1),
            x_ptr: ptr::null_mut(),
            elem_i: 0,
        };

        let mut a_ptr = arr_ptr.add(ROTATION_ELEMS);
        let mut t_ptr = arr_ptr.add(len - (ROTATION_ELEMS + 1));

        for _ in 0..((len / ROTATION_ELEMS) - 2) {
            let loop_len = ROTATION_ELEMS;
            if advance_left(a_ptr, arr_ptr, state.elem_i) {
                state.x_ptr = a_ptr;
                fulcrum_rotate(arr_ptr, &mut state, 1, loop_len, pivot, is_less);
                a_ptr = state.x_ptr;
            } else {
                state.x_ptr = t_ptr;
                fulcrum_rotate(arr_ptr, &mut state, -1, loop_len, pivot, is_less);
                t_ptr = state.x_ptr;
            }
        }

        let loop_len = len % ROTATION_ELEMS;
        if advance_left(a_ptr, arr_ptr, state.elem_i) {
            state.x_ptr = a_ptr;
            fulcrum_rotate(arr_ptr, &mut state, 1, loop_len, pivot, is_less);
        } else {
            state.x_ptr = t_ptr;
            fulcrum_rotate(arr_ptr, &mut state, -1, loop_len, pivot, is_less);
        }

        let loop_len = ROTATION_ELEMS * 2;
        state.x_ptr = swap_ptr;
        fulcrum_rotate(arr_ptr, &mut state, 1, loop_len, pivot, is_less);

        state.elem_i
    }
}

fn fulcrum_partition<T, F>(v: &mut [T], pivot: &T, is_less: &mut F) -> usize
where
    T: Freeze,
    F: FnMut(&T, &T) -> bool,
{
    // TODO explain.
    if v.len() < 256 {
        fulcrum_partition_impl::<T, F, 16>(v, pivot, is_less)
    } else {
        fulcrum_partition_impl::<T, F, 32>(v, pivot, is_less)
    }
}

// Disabled by default because it currently has panic safety issues.
const FULCRUM_ENABLED: bool = false;

/// Partitions `v` into elements smaller than `v[pivot]`, followed by elements greater than or
/// equal to `v[pivot]`.
///
/// Returns a tuple of:
///
/// 1. Number of elements smaller than `v[pivot]`.
/// 2. True if `v` was already partitioned.
#[cfg_attr(feature = "no_inline_sub_functions", inline(never))]
fn partition<T, F>(v: &mut [T], pivot: usize, is_less: &mut F) -> usize
where
    F: FnMut(&T, &T) -> bool,
{
    let mid = {
        // Place the pivot at the beginning of slice.
        v.swap(0, pivot);
        let (pivot, v) = v.split_at_mut(1);
        let pivot = &mut pivot[0];

        // Read the pivot into a stack-allocated variable for efficiency. If a following comparison
        // operation panics, the pivot will be automatically written back into the slice.

        // SAFETY: `pivot` is a reference to the first element of `v`, so `ptr::read` is safe.
        let tmp = mem::ManuallyDrop::new(unsafe { ptr::read(pivot) });
        let _pivot_guard = InsertionHole {
            src: &*tmp,
            dest: pivot,
        };
        let pivot = &*tmp;

        // type DebugT = i32;
        // let v_as_x = unsafe { mem::transmute::<&[T], &[DebugT]>(v) };
        // let pivot_as_x = unsafe { mem::transmute::<&T, &DebugT>(pivot) };

        // println!("pivot: {}", pivot_as_x);
        // println!("before: {v_as_x:?}");
        // let is_less_count = <crate::other::partition::fulcrum_partition_revised::PartitionImpl as crate::other::partition::Partition>::partition_by(v, pivot, is_less);
        // println!("after:  {v_as_x:?}");
        // println!("sub: {:?}\n", &v_as_x[..is_less_count]);

        // for val in &v_as_x[is_less_count..] {
        //     if val < pivot_as_x {
        //         println!("wrong val: {val}");
        //         panic!();
        //     }
        // }

        let is_less_count = <T as UnstableSortTypeImpl>::partition(v, pivot, is_less);

        is_less_count

        // pivot quality measurement.
        // println!("len: {} is_less: {}", v.len(), l + is_less_count);

        // `_pivot_guard` goes out of scope and writes the pivot (which is a stack-allocated
        // variable) back into the slice where it originally was. This step is critical in ensuring
        // safety!
    };

    // Place the pivot between the two partitions.
    v.swap(0, mid);

    mid
}

/// Partitions `v` into elements equal to `v[pivot]` followed by elements greater than `v[pivot]`.
///
/// Returns the number of elements equal to the pivot. It is assumed that `v` does not contain
/// elements smaller than the pivot.
#[cfg_attr(feature = "no_inline_sub_functions", inline(never))]
fn partition_equal<T, F>(v: &mut [T], pivot: usize, is_less: &mut F) -> usize
where
    F: FnMut(&T, &T) -> bool,
{
    partition(v, pivot, &mut |a, b| !is_less(b, a))
}

/// Sorts `v` recursively.
///
/// If the slice had a predecessor in the original array, it is specified as `ancestor_pivot`.
///
/// `limit` is the number of allowed imbalanced partitions before switching to `heapsort`. If zero,
/// this function will immediately switch to heapsort.
#[cfg_attr(feature = "no_inline_sub_functions", inline(never))]
fn recurse<'a, T, F>(
    mut v: &'a mut [T],
    is_less: &mut F,
    mut ancestor_pivot: Option<&'a T>,
    mut limit: u32,
) where
    F: FnMut(&T, &T) -> bool,
{
    loop {
        // println!("len: {}", v.len());

        if <T as UnstableSortTypeImpl>::small_sort(v, is_less) {
            return;
        }

        // If too many bad pivot choices were made, simply fall back to heapsort in order to
        // guarantee `O(n * log(n))` worst-case.
        if limit == 0 {
            heapsort(v, is_less);
            return;
        }

        limit -= 1;

        // Choose a pivot and try guessing whether the slice is already sorted.
        let pivot = choose_pivot(v, is_less);

        // If the chosen pivot is equal to the predecessor, then it's the smallest element in the
        // slice. Partition the slice into elements equal to and elements greater than the pivot.
        // This case is usually hit when the slice contains many duplicate elements.
        if let Some(p) = ancestor_pivot {
            if !is_less(p, &v[pivot]) {
                let mid = partition_equal(v, pivot, is_less);

                // Continue sorting elements greater than the pivot. We know that mid contains the
                // pivot. So we can continue after mid.
                v = &mut v[(mid + 1)..];
                ancestor_pivot = None;
                continue;
            }
        }

        // Partition the slice.
        let mid = partition(v, pivot, is_less);

        // Split the slice into `left`, `pivot`, and `right`.
        let (left, right) = v.split_at_mut(mid);
        let (pivot, right) = right.split_at_mut(1);
        let pivot = &pivot[0];

        // Recurse into the shorter side only in order to minimize the total number of recursive
        // calls and consume less stack space. Then just continue with the longer side (this is
        // akin to tail recursion).
        if left.len() < right.len() {
            recurse(left, is_less, ancestor_pivot, limit);
            v = right;
            ancestor_pivot = Some(pivot);
        } else {
            recurse(right, is_less, Some(pivot), limit);
            v = left;
        }
    }
}

// Use a trait to focus code-gen on only the parts actually relevant for the type. Avoid generating
// LLVM-IR for the sorting-network and median-networks for types that don't qualify.
trait UnstableSortTypeImpl: Sized {
    /// Sorts `v` using strategies optimized for small sizes.
    fn small_sort<F>(v: &mut [Self], is_less: &mut F) -> bool
    where
        F: FnMut(&Self, &Self) -> bool;

    /// Partitions `v` into elements smaller than `pivot`, followed by elements greater than or
    /// equal to `pivot`.
    ///
    /// Returns the number of elements smaller than `pivot`.
    fn partition<F>(v: &mut [Self], pivot: &Self, is_less: &mut F) -> usize
    where
        F: FnMut(&Self, &Self) -> bool;
}

impl<T> UnstableSortTypeImpl for T {
    default fn small_sort<F>(v: &mut [Self], is_less: &mut F) -> bool
    where
        F: FnMut(&Self, &Self) -> bool,
    {
        const MAX_LEN_INSERTION_SORT: usize = 20;

        let len = v.len();

        if intrinsics::likely(len <= MAX_LEN_INSERTION_SORT) {
            if intrinsics::likely(len >= 2) {
                insertion_sort_shift_left(v, 1, is_less);
            }

            true
        } else {
            false
        }
    }

    default fn partition<F>(v: &mut [Self], pivot: &Self, is_less: &mut F) -> usize
    where
        F: FnMut(&Self, &Self) -> bool,
    {
        partition_in_blocks(v, pivot, is_less)
    }
}

const PSEUDO_MEDIAN_REC_THRESHOLD: usize = 64;

/// Selects a pivot from left, right.
///
/// Idea taken from glidesort by Orson Peters.
///
/// This chooses a pivot by sampling an adaptive amount of points, mimicking the median quality of
/// median of square root.
fn choose_pivot<T, F>(v: &[T], is_less: &mut F) -> usize
where
    F: FnMut(&T, &T) -> bool,
{
    let len = v.len();

    // It's a logic bug if this get's called on slice that would be small-sorted.
    debug_assert!(len > max_len_small_sort::<T>());

    let len_div_2 = len / 2;
    let arr_ptr = v.as_ptr();

    let median_guess_ptr = if len < PSEUDO_MEDIAN_REC_THRESHOLD {
        // For small sizes it's crucial to pick a good median, just doing median3 is not great.
        let start = len_div_2 - 3;
        median7_approx(&v[start..(start + 7)], is_less)
    } else {
        // SAFETY: TODO
        unsafe {
            let len_div_8 = len / 8;
            let a = arr_ptr;
            let b = arr_ptr.add(len_div_8 * 4);
            let c = arr_ptr.add(len_div_8 * 7);

            median3_rec(a, b, c, len_div_8, is_less)
        }
    };

    // SAFETY: median_guess_ptr is part of v if median7_approx and median3_rec work as expected.
    unsafe { median_guess_ptr.sub_ptr(arr_ptr) }
}

// Never inline this function to avoid code bloat. It still optimizes nicely and has practically no
// performance impact.
#[inline(never)]
fn median7_approx<T, F>(v: &[T], is_less: &mut F) -> *const T
where
    F: FnMut(&T, &T) -> bool,
{
    // SAFETY: caller must ensure v.len() >= 7.
    assert!(v.len() == 7);

    let arr_ptr = v.as_ptr();

    // We checked the len.
    unsafe {
        let lower_median3 = median3(arr_ptr.add(0), arr_ptr.add(1), arr_ptr.add(2), is_less);
        let upper_median3 = median3(arr_ptr.add(4), arr_ptr.add(5), arr_ptr.add(6), is_less);

        let median_approx_ptr = median3(lower_median3, arr_ptr.add(3), upper_median3, is_less);
        median_approx_ptr
    }
}

/// Calculates an approximate median of 3 elements from sections a, b, c, or recursively from an
/// approximation of each, if they're large enough. By dividing the size of each section by 8 when
/// recursing we have logarithmic recursion depth and overall sample from
/// f(n) = 3*f(n/8) -> f(n) = O(n^(log(3)/log(8))) ~= O(n^0.528) elements.
///
/// SAFETY: a, b, c must point to the start of initialized regions of memory of
/// at least n elements.
#[inline(never)]
unsafe fn median3_rec<T, F>(
    mut a: *const T,
    mut b: *const T,
    mut c: *const T,
    n: usize,
    is_less: &mut F,
) -> *const T
where
    F: FnMut(&T, &T) -> bool,
{
    // SAFETY: TODO
    unsafe {
        if n * 8 >= PSEUDO_MEDIAN_REC_THRESHOLD {
            let n8 = n / 8;
            a = median3_rec(a, a.add(n8 * 4), a.add(n8 * 7), n8, is_less);
            b = median3_rec(b, b.add(n8 * 4), b.add(n8 * 7), n8, is_less);
            c = median3_rec(c, c.add(n8 * 4), c.add(n8 * 7), n8, is_less);
        }
        median3(a, b, c, is_less)
    }
}

/// Calculates the median of 3 elements.
///
/// SAFETY: a, b, c must be valid initialized elements.
unsafe fn median3<T, F>(a: *const T, b: *const T, c: *const T, is_less: &mut F) -> *const T
where
    F: FnMut(&T, &T) -> bool,
{
    // SAFETY: TODO
    //
    // Compiler tends to make this branchless when sensible, and avoids the
    // third comparison when not.
    unsafe {
        let x = is_less(&*a, &*b);
        let y = is_less(&*a, &*c);
        if x == y {
            // If x=y=0 then b, c <= a. In this case we want to return max(b, c).
            // If x=y=1 then a < b, c. In this case we want to return min(b, c).
            // By toggling the outcome of b < c using XOR x we get this behavior.
            let z = is_less(&*b, &*c);

            if z ^ x {
                c
            } else {
                b
            }
        } else {
            // Either c <= a < b or b <= a < c, thus a is our median.
            a
        }
    }
}

impl<T: Freeze> UnstableSortTypeImpl for T {
    fn small_sort<F>(v: &mut [Self], is_less: &mut F) -> bool
    where
        F: FnMut(&Self, &Self) -> bool,
    {
        let len = v.len();

        if intrinsics::likely(len <= max_len_small_sort::<T>()) {
            // I suspect that generalized efficient indirect branchless sorting constructs like
            // sort4_indirect for larger sizes exist. But finding them is an open research problem.
            // And even then it's not clear that they would be better than in-place sorting-networks
            // as used in small_sort_network.
            if const { has_efficient_in_place_swap::<T>() } {
                small_sort_network(v, is_less);
            } else {
                small_sort_general(v, is_less);
            }

            true
        } else {
            false
        }
    }

    fn partition<F>(v: &mut [Self], pivot: &Self, is_less: &mut F) -> usize
    where
        F: FnMut(&Self, &Self) -> bool,
    {
        // The code-gen properties that make in-place swapping inefficient for types larger than u64
        // also apply to fulcrum partitioning.
        if const { FULCRUM_ENABLED && has_efficient_in_place_swap::<T>() } {
            fulcrum_partition(v, pivot, is_less)
        } else {
            partition_in_blocks(v, pivot, is_less)
        }
    }
}

// --- Insertion sorts ---

// TODO merge with local variants

// When dropped, copies from `src` into `dest`.
struct InsertionHole<T> {
    src: *const T,
    dest: *mut T,
}

impl<T> Drop for InsertionHole<T> {
    fn drop(&mut self) {
        unsafe {
            ptr::copy_nonoverlapping(self.src, self.dest, 1);
        }
    }
}

/// Inserts `v[v.len() - 1]` into pre-sorted sequence `v[..v.len() - 1]` so that whole `v[..]`
/// becomes sorted.
unsafe fn insert_tail<T, F>(v: &mut [T], is_less: &mut F)
where
    F: FnMut(&T, &T) -> bool,
{
    debug_assert!(v.len() >= 2);

    let arr_ptr = v.as_mut_ptr();
    let i = v.len() - 1;

    // SAFETY: caller must ensure v is at least len 2.
    unsafe {
        // See insert_head which talks about why this approach is beneficial.
        let i_ptr = arr_ptr.add(i);

        // It's important that we use i_ptr here. If this check is positive and we continue,
        // We want to make sure that no other copy of the value was seen by is_less.
        // Otherwise we would have to copy it back.
        if is_less(&*i_ptr, &*i_ptr.sub(1)) {
            // It's important, that we use tmp for comparison from now on. As it is the value that
            // will be copied back. And notionally we could have created a divergence if we copy
            // back the wrong value.
            let tmp = mem::ManuallyDrop::new(ptr::read(i_ptr));
            // Intermediate state of the insertion process is always tracked by `hole`, which
            // serves two purposes:
            // 1. Protects integrity of `v` from panics in `is_less`.
            // 2. Fills the remaining hole in `v` in the end.
            //
            // Panic safety:
            //
            // If `is_less` panics at any point during the process, `hole` will get dropped and
            // fill the hole in `v` with `tmp`, thus ensuring that `v` still holds every object it
            // initially held exactly once.
            let mut hole = InsertionHole {
                src: &*tmp,
                dest: i_ptr.sub(1),
            };
            ptr::copy_nonoverlapping(hole.dest, i_ptr, 1);

            // SAFETY: We know i is at least 1.
            for j in (0..(i - 1)).rev() {
                let j_ptr = arr_ptr.add(j);
                if !is_less(&*tmp, &*j_ptr) {
                    break;
                }

                ptr::copy_nonoverlapping(j_ptr, hole.dest, 1);
                hole.dest = j_ptr;
            }
            // `hole` gets dropped and thus copies `tmp` into the remaining hole in `v`.
        }
    }
}

/// Sort `v` assuming `v[..offset]` is already sorted.
fn insertion_sort_shift_left<T, F>(v: &mut [T], offset: usize, is_less: &mut F)
where
    F: FnMut(&T, &T) -> bool,
{
    let len = v.len();

    // Using assert here improves performance.
    assert!(offset != 0 && offset <= len);

    // Shift each element of the unsorted region v[i..] as far left as is needed to make v sorted.
    for i in offset..len {
        // SAFETY: we tested that `offset` must be at least 1, so this loop is only entered if len
        // >= 2.
        unsafe {
            insert_tail(&mut v[..=i], is_less);
        }
    }
}

#[inline(always)]
unsafe fn merge_up<T, F>(
    mut src_left: *const T,
    mut src_right: *const T,
    mut dest_ptr: *mut T,
    is_less: &mut F,
) -> (*const T, *const T, *mut T)
where
    F: FnMut(&T, &T) -> bool,
{
    // This is a branchless merge utility function.
    // The equivalent code with a branch would be:
    //
    // if !is_less(&*src_right, &*src_left) {
    //     ptr::copy_nonoverlapping(src_left, dest_ptr, 1);
    //     src_left = src_left.wrapping_add(1);
    // } else {
    //     ptr::copy_nonoverlapping(src_right, dest_ptr, 1);
    //     src_right = src_right.wrapping_add(1);
    // }
    // dest_ptr = dest_ptr.add(1);

    // SAFETY: The caller must guarantee that `src_left`, `src_right` are valid to read and
    // `dest_ptr` is valid to write, while not aliasing.
    unsafe {
        let is_l = !is_less(&*src_right, &*src_left);
        let copy_ptr = if is_l { src_left } else { src_right };
        ptr::copy_nonoverlapping(copy_ptr, dest_ptr, 1);
        src_right = src_right.wrapping_add(!is_l as usize);
        src_left = src_left.wrapping_add(is_l as usize);
        dest_ptr = dest_ptr.add(1);
    }

    (src_left, src_right, dest_ptr)
}

#[inline(always)]
unsafe fn merge_down<T, F>(
    mut src_left: *const T,
    mut src_right: *const T,
    mut dest_ptr: *mut T,
    is_less: &mut F,
) -> (*const T, *const T, *mut T)
where
    F: FnMut(&T, &T) -> bool,
{
    // This is a branchless merge utility function.
    // The equivalent code with a branch would be:
    //
    // if !is_less(&*src_right, &*src_left) {
    //     ptr::copy_nonoverlapping(src_right, dest_ptr, 1);
    //     src_right = src_right.wrapping_sub(1);
    // } else {
    //     ptr::copy_nonoverlapping(src_left, dest_ptr, 1);
    //     src_left = src_left.wrapping_sub(1);
    // }
    // dest_ptr = dest_ptr.sub(1);

    // SAFETY: The caller must guarantee that `src_left`, `src_right` are valid to read and
    // `dest_ptr` is valid to write, while not aliasing.
    unsafe {
        let is_l = !is_less(&*src_right, &*src_left);
        let copy_ptr = if is_l { src_right } else { src_left };
        ptr::copy_nonoverlapping(copy_ptr, dest_ptr, 1);
        src_right = src_right.wrapping_sub(is_l as usize);
        src_left = src_left.wrapping_sub(!is_l as usize);
        dest_ptr = dest_ptr.sub(1);
    }

    (src_left, src_right, dest_ptr)
}

/// Merge v assuming the len is even and v[..len / 2] and v[len / 2..] are sorted.
///
/// Original idea for bi-directional merging by Igor van den Hoven (quadsort), adapted to only use
/// merge up and down. In contrast to the original parity_merge function, it performs 2 writes
/// instead of 4 per iteration. Ord violation detection was added.
unsafe fn bi_directional_merge_even<T, F>(v: &[T], dest_ptr: *mut T, is_less: &mut F)
where
    T: Freeze,
    F: FnMut(&T, &T) -> bool,
{
    // SAFETY: the caller must guarantee that `dest_ptr` is valid for v.len() writes.
    // Also `v.as_ptr` and `dest_ptr` must not alias.
    //
    // The caller must guarantee that T cannot modify itself inside is_less.
    // merge_up and merge_down read left and right pointers and potentially modify the stack value
    // they point to, if T has interior mutability. This may leave one or two potential writes to
    // the stack value un-observed when dest is copied onto of src.

    // It helps to visualize the merge:
    //
    // Initial:
    //
    //  |ptr_data (in dest)
    //  |ptr_left           |ptr_right
    //  v                   v
    // [xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx]
    //                     ^                   ^
    //                     |t_ptr_left         |t_ptr_right
    //                                         |t_ptr_data (in dest)
    //
    // After:
    //
    //                      |ptr_data (in dest)
    //        |ptr_left     |           |ptr_right
    //        v             v           v
    // [xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx]
    //       ^             ^           ^
    //       |t_ptr_left   |           |t_ptr_right
    //                     |t_ptr_data (in dest)
    //
    //
    // Note, the pointers that have been written, are now one past where they were read and
    // copied. written == incremented or decremented + copy to dest.

    let len = v.len();
    let src_ptr = v.as_ptr();

    let len_div_2 = len / 2;

    // SAFETY: No matter what the result of the user-provided comparison function is, all 4 read
    // pointers will always be in-bounds. Writing `ptr_data` and `t_ptr_data` will always be in
    // bounds if the caller guarantees that `dest_ptr` is valid for `v.len()` writes.
    unsafe {
        let mut ptr_left = src_ptr;
        let mut ptr_right = src_ptr.wrapping_add(len_div_2);
        let mut ptr_data = dest_ptr;

        let mut t_ptr_left = src_ptr.wrapping_add(len_div_2 - 1);
        let mut t_ptr_right = src_ptr.wrapping_add(len - 1);
        let mut t_ptr_data = dest_ptr.wrapping_add(len - 1);

        for _ in 0..len_div_2 {
            (ptr_left, ptr_right, ptr_data) = merge_up(ptr_left, ptr_right, ptr_data, is_less);
            (t_ptr_left, t_ptr_right, t_ptr_data) =
                merge_down(t_ptr_left, t_ptr_right, t_ptr_data, is_less);
        }

        let left_diff = (ptr_left as usize).wrapping_sub(t_ptr_left as usize);
        let right_diff = (ptr_right as usize).wrapping_sub(t_ptr_right as usize);

        if !(left_diff == mem::size_of::<T>() && right_diff == mem::size_of::<T>()) {
            panic_on_ord_violation();
        }
    }
}

// Slices of up to this length get sorted using optimized sorting for small slices.
const fn max_len_small_sort<T>() -> usize {
    if <T as IsFreeze>::value() && has_efficient_in_place_swap::<T>() {
        36
    } else {
        20
    }
}

// // #[rustc_unsafe_specialization_marker]
// trait Freeze {}

// Can the type have interior mutability, this is checked by testing if T is Freeze. If the type can
// have interior mutability it may alter itself during comparison in a way that must be observed
// after the sort operation concludes. Otherwise a type like Mutex<Option<Box<str>>> could lead to
// double free.
unsafe auto trait Freeze {}

impl<T: ?Sized> !Freeze for core::cell::UnsafeCell<T> {}
unsafe impl<T: ?Sized> Freeze for core::marker::PhantomData<T> {}
unsafe impl<T: ?Sized> Freeze for *const T {}
unsafe impl<T: ?Sized> Freeze for *mut T {}
unsafe impl<T: ?Sized> Freeze for &T {}
unsafe impl<T: ?Sized> Freeze for &mut T {}

#[const_trait]
trait IsFreeze {
    fn value() -> bool;
}

impl<T> const IsFreeze for T {
    default fn value() -> bool {
        false
    }
}

impl<T: Freeze> const IsFreeze for T {
    fn value() -> bool {
        true
    }
}

#[must_use]
const fn has_efficient_in_place_swap<T>() -> bool {
    mem::size_of::<T>() <= mem::size_of::<u64>()
}

#[test]
fn type_info() {
    assert!(has_efficient_in_place_swap::<i32>());
    assert!(has_efficient_in_place_swap::<u64>());
    assert!(!has_efficient_in_place_swap::<u128>());
    assert!(!has_efficient_in_place_swap::<String>());
}

// --- Branchless sorting (less branches not zero) ---

/// Swap two values in array pointed to by a_ptr and b_ptr if b is less than a.
#[inline(always)]
pub unsafe fn branchless_swap<T>(a_ptr: *mut T, b_ptr: *mut T, should_swap: bool) {
    // SAFETY: the caller must guarantee that `a_ptr` and `b_ptr` are valid for writes
    // and properly aligned, and part of the same allocation, and do not alias.

    // This is a branchless version of swap if.
    // The equivalent code with a branch would be:
    //
    // if should_swap {
    //     ptr::swap_nonoverlapping(a_ptr, b_ptr, 1);
    // }

    // Give ourselves some scratch space to work with.
    // We do not have to worry about drops: `MaybeUninit` does nothing when dropped.
    let mut tmp = MaybeUninit::<T>::uninit();

    // The goal is to generate cmov instructions here.
    let a_swap_ptr = if should_swap { b_ptr } else { a_ptr };
    let b_swap_ptr = if should_swap { a_ptr } else { b_ptr };

    ptr::copy_nonoverlapping(b_swap_ptr, tmp.as_mut_ptr(), 1);
    ptr::copy(a_swap_ptr, a_ptr, 1);
    ptr::copy_nonoverlapping(tmp.as_ptr(), b_ptr, 1);
}

/// Swap two values in array pointed to by a_ptr and b_ptr if b is less than a.
#[inline(always)]
pub unsafe fn swap_if_less<T, F>(arr_ptr: *mut T, a: usize, b: usize, is_less: &mut F)
where
    F: FnMut(&T, &T) -> bool,
{
    // SAFETY: the caller must guarantee that `a` and `b` each added to `arr_ptr` yield valid
    // pointers into `arr_ptr`, and are properly aligned, and part of the same allocation, and do
    // not alias. `a` and `b` must be different numbers.
    debug_assert!(a != b);

    let a_ptr = arr_ptr.add(a);
    let b_ptr = arr_ptr.add(b);

    // PANIC SAFETY: if is_less panics, no scratch memory was created and the slice should still be
    // in a well defined state, without duplicates.

    // Important to only swap if it is more and not if it is equal. is_less should return false for
    // equal, so we don't swap.
    let should_swap = is_less(&*b_ptr, &*a_ptr);
    branchless_swap(a_ptr, b_ptr, should_swap);
}

// Never inline this function to avoid code bloat. It still optimizes nicely and has practically no
// performance impact.
#[inline(never)]
fn sort10_optimal<T, F>(v: &mut [T], is_less: &mut F)
where
    F: FnMut(&T, &T) -> bool,
{
    // SAFETY: caller must ensure v.len() >= 10.
    assert!(v.len() == 10);

    let arr_ptr = v.as_mut_ptr();

    // Optimal sorting network see:
    // https://bertdobbelaere.github.io/sorting_networks.html.

    // We checked the len.
    unsafe {
        swap_if_less(arr_ptr, 0, 8, is_less);
        swap_if_less(arr_ptr, 1, 9, is_less);
        swap_if_less(arr_ptr, 2, 7, is_less);
        swap_if_less(arr_ptr, 3, 5, is_less);
        swap_if_less(arr_ptr, 4, 6, is_less);
        swap_if_less(arr_ptr, 0, 2, is_less);
        swap_if_less(arr_ptr, 1, 4, is_less);
        swap_if_less(arr_ptr, 5, 8, is_less);
        swap_if_less(arr_ptr, 7, 9, is_less);
        swap_if_less(arr_ptr, 0, 3, is_less);
        swap_if_less(arr_ptr, 2, 4, is_less);
        swap_if_less(arr_ptr, 5, 7, is_less);
        swap_if_less(arr_ptr, 6, 9, is_less);
        swap_if_less(arr_ptr, 0, 1, is_less);
        swap_if_less(arr_ptr, 3, 6, is_less);
        swap_if_less(arr_ptr, 8, 9, is_less);
        swap_if_less(arr_ptr, 1, 5, is_less);
        swap_if_less(arr_ptr, 2, 3, is_less);
        swap_if_less(arr_ptr, 4, 8, is_less);
        swap_if_less(arr_ptr, 6, 7, is_less);
        swap_if_less(arr_ptr, 1, 2, is_less);
        swap_if_less(arr_ptr, 3, 5, is_less);
        swap_if_less(arr_ptr, 4, 6, is_less);
        swap_if_less(arr_ptr, 7, 8, is_less);
        swap_if_less(arr_ptr, 2, 3, is_less);
        swap_if_less(arr_ptr, 4, 5, is_less);
        swap_if_less(arr_ptr, 6, 7, is_less);
        swap_if_less(arr_ptr, 3, 4, is_less);
        swap_if_less(arr_ptr, 5, 6, is_less);
    }
}

// Never inline this function to avoid code bloat. It still optimizes nicely and has practically no
// performance impact.
#[inline(never)]
fn sort14_optimal<T, F>(v: &mut [T], is_less: &mut F)
where
    F: FnMut(&T, &T) -> bool,
{
    // SAFETY: caller must ensure v.len() >= 14.
    assert!(v.len() == 14);

    let arr_ptr = v.as_mut_ptr();

    // Optimal sorting network see:
    // https://bertdobbelaere.github.io/sorting_networks.html.

    // We checked the len.
    unsafe {
        swap_if_less(arr_ptr, 0, 1, is_less);
        swap_if_less(arr_ptr, 2, 3, is_less);
        swap_if_less(arr_ptr, 4, 5, is_less);
        swap_if_less(arr_ptr, 6, 7, is_less);
        swap_if_less(arr_ptr, 8, 9, is_less);
        swap_if_less(arr_ptr, 10, 11, is_less);
        swap_if_less(arr_ptr, 12, 13, is_less);
        swap_if_less(arr_ptr, 0, 2, is_less);
        swap_if_less(arr_ptr, 1, 3, is_less);
        swap_if_less(arr_ptr, 4, 8, is_less);
        swap_if_less(arr_ptr, 5, 9, is_less);
        swap_if_less(arr_ptr, 10, 12, is_less);
        swap_if_less(arr_ptr, 11, 13, is_less);
        swap_if_less(arr_ptr, 0, 10, is_less);
        swap_if_less(arr_ptr, 1, 6, is_less);
        swap_if_less(arr_ptr, 2, 11, is_less);
        swap_if_less(arr_ptr, 3, 13, is_less);
        swap_if_less(arr_ptr, 5, 8, is_less);
        swap_if_less(arr_ptr, 7, 12, is_less);
        swap_if_less(arr_ptr, 1, 4, is_less);
        swap_if_less(arr_ptr, 2, 8, is_less);
        swap_if_less(arr_ptr, 3, 6, is_less);
        swap_if_less(arr_ptr, 5, 11, is_less);
        swap_if_less(arr_ptr, 7, 10, is_less);
        swap_if_less(arr_ptr, 9, 12, is_less);
        swap_if_less(arr_ptr, 0, 1, is_less);
        swap_if_less(arr_ptr, 3, 9, is_less);
        swap_if_less(arr_ptr, 4, 10, is_less);
        swap_if_less(arr_ptr, 5, 7, is_less);
        swap_if_less(arr_ptr, 6, 8, is_less);
        swap_if_less(arr_ptr, 12, 13, is_less);
        swap_if_less(arr_ptr, 1, 5, is_less);
        swap_if_less(arr_ptr, 2, 4, is_less);
        swap_if_less(arr_ptr, 3, 7, is_less);
        swap_if_less(arr_ptr, 6, 10, is_less);
        swap_if_less(arr_ptr, 8, 12, is_less);
        swap_if_less(arr_ptr, 9, 11, is_less);
        swap_if_less(arr_ptr, 1, 2, is_less);
        swap_if_less(arr_ptr, 3, 5, is_less);
        swap_if_less(arr_ptr, 4, 6, is_less);
        swap_if_less(arr_ptr, 7, 9, is_less);
        swap_if_less(arr_ptr, 8, 10, is_less);
        swap_if_less(arr_ptr, 11, 12, is_less);
        swap_if_less(arr_ptr, 2, 3, is_less);
        swap_if_less(arr_ptr, 4, 5, is_less);
        swap_if_less(arr_ptr, 6, 7, is_less);
        swap_if_less(arr_ptr, 8, 9, is_less);
        swap_if_less(arr_ptr, 10, 11, is_less);
        swap_if_less(arr_ptr, 3, 4, is_less);
        swap_if_less(arr_ptr, 5, 6, is_less);
        swap_if_less(arr_ptr, 7, 8, is_less);
        swap_if_less(arr_ptr, 9, 10, is_less);
    }
}

#[cfg_attr(feature = "no_inline_sub_functions", inline(never))]
fn sort14_plus<T, F>(v: &mut [T], is_less: &mut F)
where
    T: Freeze,
    F: FnMut(&T, &T) -> bool,
{
    let len = v.len();
    const MAX_BRANCHLESS_SMALL_SORT: usize = max_len_small_sort::<i32>();

    assert!(len >= 14 && len <= MAX_BRANCHLESS_SMALL_SORT);

    if len < 20 {
        sort14_optimal(&mut v[0..14], is_less);
        insertion_sort_shift_left(v, 14, is_less);
        return;
    }

    // This should optimize to a shift right https://godbolt.org/z/vYGsznPPW.
    let even_len = len - (len % 2 != 0) as usize;
    let len_div_2 = even_len / 2;

    let mid = if len < 28 {
        sort10_optimal(&mut v[0..10], is_less);
        sort10_optimal(&mut v[len_div_2..(len_div_2 + 10)], is_less);

        10
    } else {
        sort14_optimal(&mut v[0..14], is_less);
        sort14_optimal(&mut v[len_div_2..(len_div_2 + 14)], is_less);

        14
    };

    insertion_sort_shift_left(&mut v[0..len_div_2], mid, is_less);
    insertion_sort_shift_left(&mut v[len_div_2..], mid, is_less);

    let mut swap = MaybeUninit::<[T; MAX_BRANCHLESS_SMALL_SORT]>::uninit();
    let swap_ptr = swap.as_mut_ptr() as *mut T;

    // SAFETY: We checked that T is Freeze and thus observation safe.
    // Should is_less panic v was not modified in parity_merge and retains it's original input.
    // swap and v must not alias and swap has v.len() space.
    unsafe {
        bi_directional_merge_even(&mut v[..even_len], swap_ptr, is_less);
        ptr::copy_nonoverlapping(swap_ptr, v.as_mut_ptr(), even_len);
    }

    if len != even_len {
        // SAFETY: We know len >= 2.
        unsafe {
            insert_tail(v, is_less);
        }
    }
}

fn small_sort_network<T, F>(v: &mut [T], is_less: &mut F)
where
    T: Freeze,
    F: FnMut(&T, &T) -> bool,
{
    // This implementation is tuned to be efficient for integer types.

    let len = v.len();

    // Always sort assuming somewhat random distribution.
    // Patterns should have already been found by the other analysis steps.
    //
    // Small total slices are handled separately, see function quicksort.
    if len >= 14 {
        sort14_plus(v, is_less);
    } else if len >= 2 {
        let end = if len >= 10 {
            sort10_optimal(&mut v[0..10], is_less);
            10
        } else {
            1
        };

        insertion_sort_shift_left(v, end, is_less);
    }
}

fn small_sort_general<T, F>(v: &mut [T], is_less: &mut F)
where
    T: Freeze,
    F: FnMut(&T, &T) -> bool,
{
    // This implementation is tuned to be efficient for various types that are larger than u64.

    const MAX_SIZE: usize = max_len_small_sort::<String>();

    let len = v.len();

    let mut scratch = MaybeUninit::<[T; MAX_SIZE]>::uninit();
    let scratch_ptr = scratch.as_mut_ptr() as *mut T;

    if len >= 16 && len <= MAX_SIZE {
        let even_len = len - (len % 2);
        let len_div_2 = even_len / 2;

        // SAFETY: scratch_ptr is valid and has enough space. And we checked that both
        // v[..len_div_2] and v[len_div_2..] are at least 8 large.
        unsafe {
            let arr_ptr = v.as_mut_ptr();
            sort8_indirect(arr_ptr, scratch_ptr, is_less);
            sort8_indirect(arr_ptr.add(len_div_2), scratch_ptr, is_less);
        }

        insertion_sort_shift_left(&mut v[0..len_div_2], 8, is_less);
        insertion_sort_shift_left(&mut v[len_div_2..], 8, is_less);

        // SAFETY: We checked that T is Freeze and thus observation safe. Should is_less panic v
        // was not modified in parity_merge and retains it's original input. swap and v must not
        // alias and swap has v.len() space.
        unsafe {
            bi_directional_merge_even(&mut v[..even_len], scratch_ptr, is_less);
            ptr::copy_nonoverlapping(scratch_ptr, v.as_mut_ptr(), even_len);
        }

        if len != even_len {
            // SAFETY: We know len >= 2.
            unsafe {
                insert_tail(v, is_less);
            }
        }
    } else if len >= 2 {
        let offset = if len >= 8 {
            // SAFETY: scratch_ptr is valid and has enough space.
            unsafe {
                sort8_indirect(v.as_mut_ptr(), scratch_ptr, is_less);
            }

            8
        } else {
            1
        };

        insertion_sort_shift_left(v, offset, is_less);
    }
}

/// SAFETY: The caller MUST guarantee that `arr_ptr` is valid for 4 reads and `dest_ptr` is valid
/// for 4 writes.
pub unsafe fn sort4_indirect<T, F>(arr_ptr: *const T, dest_ptr: *mut T, is_less: &mut F)
where
    F: FnMut(&T, &T) -> bool,
{
    // By limiting select to picking pointers, we are guaranteed good cmov code-gen regardless of
    // type T layout. Further this only does 5 instead of 6 comparisons compared to a stable
    // transposition 4 element sorting-network. Also by only operating on pointers, we get optimal
    // element copy usage. Doing exactly 1 copy per element.

    // let arr_ptr = v.as_ptr();

    unsafe {
        // Stably create two pairs a <= b and c <= d.
        let c1 = is_less(&*arr_ptr.add(1), &*arr_ptr) as usize;
        let c2 = is_less(&*arr_ptr.add(3), &*arr_ptr.add(2)) as usize;
        let a = arr_ptr.add(c1);
        let b = arr_ptr.add(c1 ^ 1);
        let c = arr_ptr.add(2 + c2);
        let d = arr_ptr.add(2 + (c2 ^ 1));

        // Compare (a, c) and (b, d) to identify max/min. We're left with two
        // unknown elements, but because we are a stable sort we must know which
        // one is leftmost and which one is rightmost.
        // c3, c4 | min max unknown_left unknown_right
        //  0,  0 |  a   d    b         c
        //  0,  1 |  a   b    c         d
        //  1,  0 |  c   d    a         b
        //  1,  1 |  c   b    a         d
        let c3 = is_less(&*c, &*a);
        let c4 = is_less(&*d, &*b);
        let min = select(c3, c, a);
        let max = select(c4, b, d);
        let unknown_left = select(c3, a, select(c4, c, b));
        let unknown_right = select(c4, d, select(c3, b, c));

        // Sort the last two unknown elements.
        let c5 = is_less(&*unknown_right, &*unknown_left);
        let lo = select(c5, unknown_right, unknown_left);
        let hi = select(c5, unknown_left, unknown_right);

        ptr::copy_nonoverlapping(min, dest_ptr, 1);
        ptr::copy_nonoverlapping(lo, dest_ptr.add(1), 1);
        ptr::copy_nonoverlapping(hi, dest_ptr.add(2), 1);
        ptr::copy_nonoverlapping(max, dest_ptr.add(3), 1);
    }

    #[inline(always)]
    pub fn select<T>(cond: bool, if_true: *const T, if_false: *const T) -> *const T {
        if cond {
            if_true
        } else {
            if_false
        }
    }
}

/// SAFETY: The caller MUST guarantee that `arr_ptr` is valid for 8 reads and writes, and
/// `scratch_ptr` is valid for 8 writes.
#[inline(never)]
unsafe fn sort8_indirect<T, F>(arr_ptr: *mut T, scratch_ptr: *mut T, is_less: &mut F)
where
    T: Freeze,
    F: FnMut(&T, &T) -> bool,
{
    // SAFETY: The caller must guarantee that scratch_ptr is valid for 8 writes, and that arr_ptr is
    // valid for 8 reads.
    unsafe {
        sort4_indirect(arr_ptr, scratch_ptr, is_less);
        sort4_indirect(arr_ptr.add(4), scratch_ptr.add(4), is_less);
    }

    // SAFETY: We checked that T is Freeze and thus observation safe.
    // Should is_less panic v was not modified in parity_merge and retains its original input.
    // swap and v must not alias and swap has v.len() space.
    unsafe {
        // It's slightly faster to merge directly into v and copy over the 'safe' elements of swap
        // into v only if there was a panic. This technique is also known as ping-pong merge.
        let drop_guard = DropGuard {
            src: scratch_ptr,
            dest: arr_ptr,
        };
        bi_directional_merge_even(
            &*ptr::slice_from_raw_parts(scratch_ptr, 8),
            arr_ptr,
            is_less,
        );
        mem::forget(drop_guard);
    }

    struct DropGuard<T> {
        src: *const T,
        dest: *mut T,
    }

    impl<T> Drop for DropGuard<T> {
        fn drop(&mut self) {
            // SAFETY: `T` is not a zero-sized type, src must hold the original 8 elements of v in
            // any order. And dest must be valid to write 8 elements.
            //
            // Use black_box to emit memcpy instead of efficient direct copying. This reduces the
            // binary size, and this path will only be used if the comparison function panics.
            unsafe {
                ptr::copy_nonoverlapping(self.src, self.dest, core::hint::black_box(8));
            }
        }
    }
}

#[inline(never)]
fn panic_on_ord_violation() -> ! {
    panic!("Ord violation");
}
