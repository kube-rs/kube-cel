//! Go's `sort.Slice`, the sort behind cel-go's `sort()` and `sortBy()`.
//!
//! cel-go v0.26 sorts a list by `sort.Slice` over the indices of its elements,
//! comparing their keys. `sort.Slice` is not stable: elements with equal keys
//! end up in an order that depends on the algorithm, a pattern-defeating
//! quicksort, from 13 elements on (below that it is an insertion sort, which
//! keeps them in order). This is a port of go1.26.2's `pdqsort_func`
//! (`src/sort/zsortfunc.go`, with `Slice` from `slice.go` and `xorshift` from
//! `sort.go`), the toolchain kube-apiserver v1.36.1 is built with, so the
//! order is the apiserver's, ties included. Any other pdqsort, or breaking ties
//! by index, orders equal keys differently. Keep it line for line with the Go
//! source when porting a newer release.

/// Sorts `data` as go1.26.2's `sort.Slice(data, less)` does, where `less`
/// compares two elements (Go's `less(i, j)` reads `data[i]` and `data[j]`).
pub(crate) fn go_sort_slice<T>(data: &mut [T], less: impl FnMut(&T, &T) -> bool) {
    let length = data.len();
    let limit = bit_len(length as u64);
    let mut data = LessSwap { data, less };
    pdqsort(&mut data, 0, length, limit);
}

/// Go's `lessSwap`: `Less(i, j)` and `Swap(i, j)` on the slice being sorted.
struct LessSwap<'a, T, F> {
    data: &'a mut [T],
    less: F,
}

impl<T, F: FnMut(&T, &T) -> bool> LessSwap<'_, T, F> {
    fn less(&mut self, i: usize, j: usize) -> bool {
        (self.less)(&self.data[i], &self.data[j])
    }

    fn swap(&mut self, i: usize, j: usize) {
        self.data.swap(i, j);
    }
}

/// `bits.Len(uint(x))`: the bits needed to write `x`, 0 for 0.
fn bit_len(x: u64) -> usize {
    (u64::BITS - x.leading_zeros()) as usize
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortedHint {
    Unknown,
    Increasing,
    Decreasing,
}

/// Go's `xorshift`, a `uint64` state.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn next_power_of_two(length: usize) -> u64 {
    1 << bit_len(length as u64)
}

fn insertion_sort<T, F: FnMut(&T, &T) -> bool>(data: &mut LessSwap<'_, T, F>, a: usize, b: usize) {
    for i in a + 1..b {
        let mut j = i;
        while j > a && data.less(j, j - 1) {
            data.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn sift_down<T, F: FnMut(&T, &T) -> bool>(data: &mut LessSwap<'_, T, F>, lo: usize, hi: usize, first: usize) {
    let mut root = lo;
    loop {
        let mut child = 2 * root + 1;
        if child >= hi {
            break;
        }
        if child + 1 < hi && data.less(first + child, first + child + 1) {
            child += 1;
        }
        if !data.less(first + root, first + child) {
            return;
        }
        data.swap(first + root, first + child);
        root = child;
    }
}

fn heap_sort<T, F: FnMut(&T, &T) -> bool>(data: &mut LessSwap<'_, T, F>, a: usize, b: usize) {
    let first = a;
    let lo = 0;
    let hi = b - a;

    // Build heap with greatest element at top.
    for i in (0..=(hi - 1) / 2).rev() {
        sift_down(data, i, hi, first);
    }

    // Pop elements, largest first, into end of data.
    for i in (0..hi).rev() {
        data.swap(first, first + i);
        sift_down(data, lo, i, first);
    }
}

/// `pdqsort_func`: sorts `data[a..b]`; `limit` is the number of bad pivots
/// allowed before falling back to heapsort.
fn pdqsort<T, F: FnMut(&T, &T) -> bool>(
    data: &mut LessSwap<'_, T, F>,
    mut a: usize,
    mut b: usize,
    mut limit: usize,
) {
    const MAX_INSERTION: usize = 12;

    let mut was_balanced = true; // whether the last partitioning was reasonably balanced
    let mut was_partitioned = true; // whether the slice was already partitioned

    loop {
        let length = b - a;

        if length <= MAX_INSERTION {
            insertion_sort(data, a, b);
            return;
        }

        // Fall back to heapsort if too many bad choices were made.
        if limit == 0 {
            heap_sort(data, a, b);
            return;
        }

        // If the last partitioning was imbalanced, we need to breaking patterns.
        if !was_balanced {
            break_patterns(data, a, b);
            limit -= 1;
        }

        let (mut pivot, mut hint) = choose_pivot(data, a, b);
        if hint == SortedHint::Decreasing {
            reverse_range(data, a, b);
            // The chosen pivot was pivot-a elements after the start of the array.
            // After reversing it is pivot-a elements before the end of the array.
            pivot = (b - 1) - (pivot - a);
            hint = SortedHint::Increasing;
        }

        // The slice is likely already sorted.
        if was_balanced
            && was_partitioned
            && hint == SortedHint::Increasing
            && partial_insertion_sort(data, a, b)
        {
            return;
        }

        // Probably the slice contains many duplicate elements, partition the slice into
        // elements equal to and elements greater than the pivot.
        if a > 0 && !data.less(a - 1, pivot) {
            let mid = partition_equal(data, a, b, pivot);
            a = mid;
            continue;
        }

        let (mid, already_partitioned) = partition(data, a, b, pivot);
        was_partitioned = already_partitioned;

        let (left_len, right_len) = (mid - a, b - mid);
        let balance_threshold = length / 8;
        if left_len < right_len {
            was_balanced = left_len >= balance_threshold;
            pdqsort(data, a, mid, limit);
            a = mid + 1;
        } else {
            was_balanced = right_len >= balance_threshold;
            pdqsort(data, mid + 1, b, limit);
            b = mid;
        }
    }
}

/// `partition_func`: one quicksort partition around `data[pivot]`; returns
/// its new index and whether `data[a..b]` was already partitioned.
fn partition<T, F: FnMut(&T, &T) -> bool>(
    data: &mut LessSwap<'_, T, F>,
    a: usize,
    b: usize,
    pivot: usize,
) -> (usize, bool) {
    data.swap(a, pivot);
    // i and j are inclusive of the elements remaining to be partitioned. j can
    // go one below a + 1 (to a), so it stays signed as in Go.
    let (mut i, mut j) = (a as isize + 1, b as isize - 1);
    let a_ = a as isize;

    while i <= j && data.less(i as usize, a) {
        i += 1;
    }
    while i <= j && !data.less(j as usize, a) {
        j -= 1;
    }
    if i > j {
        data.swap(j as usize, a);
        return (j as usize, true);
    }
    data.swap(i as usize, j as usize);
    i += 1;
    j -= 1;

    loop {
        while i <= j && data.less(i as usize, a) {
            i += 1;
        }
        while i <= j && !data.less(j as usize, a) {
            j -= 1;
        }
        if i > j {
            break;
        }
        data.swap(i as usize, j as usize);
        i += 1;
        j -= 1;
    }
    debug_assert!(j >= a_);
    data.swap(j as usize, a);
    (j as usize, false)
}

/// `partitionEqual_func`: partitions `data[a..b]`, which holds nothing smaller
/// than `data[pivot]`, into the elements equal to it followed by the greater.
fn partition_equal<T, F: FnMut(&T, &T) -> bool>(
    data: &mut LessSwap<'_, T, F>,
    a: usize,
    b: usize,
    pivot: usize,
) -> usize {
    data.swap(a, pivot);
    let (mut i, mut j) = (a as isize + 1, b as isize - 1);

    loop {
        while i <= j && !data.less(a, i as usize) {
            i += 1;
        }
        while i <= j && data.less(a, j as usize) {
            j -= 1;
        }
        if i > j {
            break;
        }
        data.swap(i as usize, j as usize);
        i += 1;
        j -= 1;
    }
    i as usize
}

/// `partialInsertionSort_func`: partially sorts `data[a..b]`; true if it is
/// sorted at the end. As in Go, the shift to the left runs down to index 1,
/// not to `a`.
fn partial_insertion_sort<T, F: FnMut(&T, &T) -> bool>(
    data: &mut LessSwap<'_, T, F>,
    a: usize,
    b: usize,
) -> bool {
    const MAX_STEPS: usize = 5; // maximum number of adjacent out-of-order pairs that will get shifted
    const SHORTEST_SHIFTING: usize = 50; // don't shift any elements on short arrays

    let mut i = a + 1;
    for _ in 0..MAX_STEPS {
        while i < b && !data.less(i, i - 1) {
            i += 1;
        }

        if i == b {
            return true;
        }

        if b - a < SHORTEST_SHIFTING {
            return false;
        }

        data.swap(i, i - 1);

        // Shift the smaller one to the left.
        if i - a >= 2 {
            let mut j = i - 1;
            while j >= 1 {
                if !data.less(j, j - 1) {
                    break;
                }
                data.swap(j, j - 1);
                j -= 1;
            }
        }
        // Shift the greater one to the right.
        if b - i >= 2 {
            for j in i + 1..b {
                if !data.less(j, j - 1) {
                    break;
                }
                data.swap(j, j - 1);
            }
        }
    }
    false
}

/// `breakPatterns_func`: scatters a few elements to break patterns that make
/// quicksort partitions imbalanced. Seeded by the length, so deterministic.
fn break_patterns<T, F: FnMut(&T, &T) -> bool>(data: &mut LessSwap<'_, T, F>, a: usize, b: usize) {
    let length = b - a;
    if length >= 8 {
        let mut random = Xorshift(length as u64);
        let modulus = next_power_of_two(length);

        for idx in a + (length / 4) * 2 - 1..=a + (length / 4) * 2 + 1 {
            let mut other = (random.next() & (modulus - 1)) as usize;
            if other >= length {
                other -= length;
            }
            data.swap(idx, a + other);
        }
    }
}

/// `choosePivot_func`: a static pivot below 8 elements, the median of three
/// below 50, Tukey's ninther from 50 on.
fn choose_pivot<T, F: FnMut(&T, &T) -> bool>(
    data: &mut LessSwap<'_, T, F>,
    a: usize,
    b: usize,
) -> (usize, SortedHint) {
    const SHORTEST_NINTHER: usize = 50;
    const MAX_SWAPS: usize = 4 * 3;

    let l = b - a;

    let mut swaps = 0;
    let mut i = a + l / 4;
    let mut j = a + l / 4 * 2;
    let mut k = a + l / 4 * 3;

    if l >= 8 {
        if l >= SHORTEST_NINTHER {
            // Tukey ninther method, the idea came from Rust's implementation.
            i = median_adjacent(data, i, &mut swaps);
            j = median_adjacent(data, j, &mut swaps);
            k = median_adjacent(data, k, &mut swaps);
        }
        // Find the median among i, j, k and stores it into j.
        j = median(data, i, j, k, &mut swaps);
    }

    match swaps {
        0 => (j, SortedHint::Increasing),
        MAX_SWAPS => (j, SortedHint::Decreasing),
        _ => (j, SortedHint::Unknown),
    }
}

/// `order2_func`: `(x, y)` with `data[x] <= data[y]`, of `a` and `b`.
fn order2<T, F: FnMut(&T, &T) -> bool>(
    data: &mut LessSwap<'_, T, F>,
    a: usize,
    b: usize,
    swaps: &mut usize,
) -> (usize, usize) {
    if data.less(b, a) {
        *swaps += 1;
        return (b, a);
    }
    (a, b)
}

/// `median_func`: the index of the median of `data[a]`, `data[b]`, `data[c]`.
fn median<T, F: FnMut(&T, &T) -> bool>(
    data: &mut LessSwap<'_, T, F>,
    a: usize,
    b: usize,
    c: usize,
    swaps: &mut usize,
) -> usize {
    let (a, b) = order2(data, a, b, swaps);
    let (b, _c) = order2(data, b, c, swaps);
    let (_a, b) = order2(data, a, b, swaps);
    b
}

/// `medianAdjacent_func`: the median of `data[a - 1]`, `data[a]`, `data[a + 1]`.
fn median_adjacent<T, F: FnMut(&T, &T) -> bool>(
    data: &mut LessSwap<'_, T, F>,
    a: usize,
    swaps: &mut usize,
) -> usize {
    median(data, a - 1, a, a + 1, swaps)
}

fn reverse_range<T, F: FnMut(&T, &T) -> bool>(data: &mut LessSwap<'_, T, F>, a: usize, b: usize) {
    let (mut i, mut j) = (a, b - 1);
    while i < j {
        data.swap(i, j);
        i += 1;
        j -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::go_sort_slice;

    /// The index permutation `sort.Slice` leaves for `keys`, compared by key.
    fn permutation(keys: &[i64]) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..keys.len()).collect();
        go_sort_slice(&mut idx, |&i, &j| keys[i] < keys[j]);
        idx
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 256, failure_persistence: None, ..Default::default() })]

        /// Whatever `less` answers, even inconsistently (a NaN's comparisons),
        /// the sort stays in bounds and leaves a permutation.
        #[test]
        fn any_less_leaves_a_permutation(
            keys in proptest::collection::vec(proptest::prop_oneof![proptest::num::f64::ANY, proptest::strategy::Just(f64::NAN)], 0..400),
            flip in proptest::prelude::any::<u64>(),
        ) {
            let mut idx: Vec<usize> = (0..keys.len()).collect();
            let mut calls = 0u64;
            go_sort_slice(&mut idx, |&i, &j| {
                calls += 1;
                // Every so often answer the opposite, breaking transitivity.
                (keys[i] < keys[j]) ^ ((flip >> (calls % 64)) & 1 == 1 && calls.is_multiple_of(7))
            });
            let mut seen = idx.clone();
            seen.sort_unstable();
            proptest::prop_assert_eq!(seen, (0..keys.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn ties_keep_their_order_up_to_twelve_elements() {
        let keys: Vec<i64> = (0..12).map(|i| i % 2).collect();
        assert_eq!(permutation(&keys), [0, 2, 4, 6, 8, 10, 1, 3, 5, 7, 9, 11]);
    }

    #[test]
    fn matches_go_from_thirteen_elements() {
        // go1.26.2 through cel-go v0.26: lists.range(n).sortBy(x, x % 2).
        let keys: Vec<i64> = (0..13).map(|i| i % 2).collect();
        assert_eq!(permutation(&keys), [8, 12, 2, 0, 4, 10, 6, 3, 7, 9, 5, 11, 1]);
        let keys: Vec<i64> = (0..20).map(|i| i % 2).collect();
        assert_eq!(permutation(&keys), [
            10, 18, 2, 16, 4, 0, 6, 14, 8, 12, 5, 11, 9, 13, 7, 15, 3, 17, 1, 19
        ]);
    }
}
