use haphazard::{Domain, Global, HazardPointer, HazardPointerArray};

use core::{
    ops::{Deref, DerefMut},
    sync::atomic::AtomicUsize,
};

mod node;
mod padded;

pub(crate) use node::{Head, Levels, Node};
pub(crate) use padded::Padded;

pub(crate) const HEIGHT_BITS: usize = 5;

pub(crate) const HEIGHT: usize = 1 << HEIGHT_BITS;
pub(crate) const HEIGHT_MASK: usize = (1 << (HEIGHT_BITS + 1)) - 1;

/// The garbage collection of the list
/// Utilizes Hazard Pointers under the hood to prevent use-after-frees and
/// the ABA problem.
pub(crate) struct Can<'domain> {
    pub(crate) domain: &'domain Domain<Global>,
    pub(crate) hp: HazardPointerArray<'domain, Global, 2>,
}

impl<'domain> Can<'domain> {
    pub(crate) fn new() -> Self {
        Can {
            domain: Domain::global(),
            hp: HazardPointer::many(),
        }
    }
}

impl<'domain> Clone for Can<'domain> {
    fn clone(&self) -> Self {
        Can {
            domain: self.domain,
            hp: HazardPointer::many_in_domain(self.domain),
        }
    }
}

impl<'domain> Deref for Can<'domain> {
    type Target = HazardPointerArray<'domain, Global, 2>;
    fn deref(&self) -> &Self::Target {
        &self.hp
    }
}

impl<'domain> DerefMut for Can<'domain> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.hp
    }
}

pub(crate) trait GeneratesHeight {
    fn gen_height(&self) -> usize;
}

pub(crate) struct ListState {
    pub(crate) len: AtomicUsize,
    pub(crate) max_height: AtomicUsize,
    pub(crate) seed: AtomicUsize,
}

impl ListState {
    pub(crate) fn new() -> Self {
        ListState {
            len: AtomicUsize::new(0),
            max_height: AtomicUsize::new(1),
            seed: AtomicUsize::new(rand::random()),
        }
    }
}

/// This macro allows us to define a basic `SkipList`. We only implement the methods that should be
/// the same for all variations (non-sync, sync, ...) and let the user implement all the other
/// methods themselves.
macro_rules! skiplist_basics {
    ($my_list: ident) => {
        pub struct $my_list<'domain, K, V> {
            pub(crate) head: core::ptr::NonNull<crate::internal::utils::Head<K, V>>,
            pub(crate) state: crate::internal::utils::Padded<crate::internal::utils::ListState>,
            #[allow(dead_code)]
            pub(crate) garbage: crate::internal::utils::Can<'domain>,
        }

        impl<'domain, K, V> $my_list<'domain, K, V> {
            pub fn new() -> Self {
                $my_list {
                    head: crate::internal::utils::Head::new(),
                    state: crate::internal::utils::Padded::new(
                        crate::internal::utils::ListState::new(),
                    ),
                    garbage: crate::internal::utils::Can::new(),
                }
            }

            pub fn len(&self) -> usize {
                self.state.len.load(Ordering::Relaxed)
            }

            pub fn is_empty(&self) -> bool {
                self.state.len.load(Ordering::Relaxed) < 1
            }

            fn gen_height(&self) -> usize {
                let mut seed = self.state.seed.load(Ordering::Relaxed);
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;

                self.state.seed.store(seed, Ordering::Relaxed);

                let mut height = std::cmp::min(
                    crate::internal::utils::HEIGHT,
                    seed.trailing_zeros() as usize + 1,
                );

                let head = unsafe { &(*self.head.as_ptr()) };

                while height >= 4 && head.levels[height - 2].load_ptr().is_null() {
                    height -= 1;
                }

                if height > self.state.max_height.load(Ordering::Relaxed) {
                    self.state.max_height.store(height, Ordering::Relaxed);
                }

                height
            }
        }

        /// Need this trait for our [Node](Node)s to be generated with random heights.
        impl<'domain, K, V> GeneratesHeight for $my_list<'domain, K, V> {
            fn gen_height(&self) -> usize {
                self.gen_height()
            }
        }

        // TODO Verify this is sound for all variants of SkipList
        /// Manual `Drop` implementation for all `SkipList`s
        impl<'domain, K, V> Drop for $my_list<'domain, K, V> {
            fn drop(&mut self) {
                // To ensure this is safe, clear all `HazardPointer`s in the domain.
                // We do not want to drop a node twice!
                self.garbage.domain.eager_reclaim();
                let mut node = unsafe { (*self.head.as_ptr()).levels[0].load_ptr() };

                // # Safety
                //
                // We have an exclusive reference to `SkipList`.
                unsafe {
                    while !node.is_null() {
                        let temp = node;
                        node = (*temp).levels[0].load_ptr();
                        crate::internal::utils::Node::<K, V>::drop(temp);
                    }

                    crate::internal::utils::Head::<K, V>::drop(self.head);
                }
            }
        }
    };
}

pub(crate) use skiplist_basics;
