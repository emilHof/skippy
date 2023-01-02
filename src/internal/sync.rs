use core::{borrow::Borrow, fmt::Debug, marker::Sync, sync::atomic::Ordering};
use std::{ptr::NonNull, sync::atomic::AtomicPtr};

use haphazard::{raw::Reclaim, Global, HazardPointer, Domain};

#[allow(unused_imports)]
use crate::{
    internal::utils::{skiplist_basics, GeneratesHeight, Levels, Node, HEIGHT},
    skiplist,
};

pub(crate) mod tagged;

skiplist_basics!(SkipList);

impl<'domain, K, V> SkipList<'domain, K, V>
where
    K: Ord + Send + Sync,
    V: Send + Sync,
{
    /// Inserts a value in the list given a key.
    pub fn insert(&self, key: K, val: V) -> Option<V> {
        // After this check, whether we are holding the head or a regular Node will
        // not impact the operation.
        unsafe {
            let insertion_point = self.find(&key, false);

            match insertion_point {
                SearchResult {
                    target: Some(_target),
                    ..
                } => {
                    // TODO Swap the old val with the new one without incurring race conditions.
                    /*
                    std::mem::swap(&mut (*target.as_ptr()).val, &mut val);
                    drop(hazard);
                    */
                    Some(val)
                }
                SearchResult {
                    mut prev,
                    ..
                } => {
                    let new_node_raw = Node::new_rand_height(key, val, self);

                    // Protects the new_node so concurrent removals do not invalidate our pointer.
                    let new_node = NodeRef::from_raw(new_node_raw);

                    let mut starting_height = 0;

                    // The node should not be in build stage!
                    // assert!(new_node.set_build_begin().is_ok());

                    while let Err(starting) =
                        self.link_nodes(&new_node, prev, starting_height)
                    {
                        (prev, starting_height) = {
                            let SearchResult {
                                prev,
                                target,
                            } = self.find(&new_node.key, false);
                            if let Some(_target) = target {
                                if starting == 0 {
                                    // TODO Implement something like this in a concurrency-safe
                                    // way!

                                    self.retire_node(new_node.node.as_ptr());

                                    return None;
                                }
                            }
                            (prev, starting)
                        };
                    }

                    // The node should still be in build stage!
                    // assert!(new_node.set_build_done().is_ok());

                    self.state.len.fetch_add(1, Ordering::Relaxed);

                    None
                }
            }
        }
    }

    /// This function is unsafe, as it does not check whether new_node or link node are valid
    /// pointers.
    ///
    /// # Safety
    ///
    /// 1. `new_node` cannot be null
    /// 2. A tower of sufficient height must eventually be reached, the list head can be this tower
    unsafe fn link_nodes<'a>(
        &self,
        new_node: &'a NodeRef<'a, K, V>,
        previous_nodes: [(NodeRef<'a, K, V>, Option<NodeRef<'a, K, V>>); HEIGHT],
        start_height: usize,
    ) -> Result<(), usize> {
        // iterate over all the levels in the new nodes pointer tower
        for i in start_height..new_node.height() {
            let (prev, next) = &previous_nodes[i];
            let next_ptr = next.as_ref().map_or(core::ptr::null_mut(), |n| n.as_ptr());

            /*
            let next = NodeRef::from_maybe_tagged(&prev.levels[i]);
            let next_ptr = next.as_ref().map_or(core::ptr::null_mut(), |n| n.as_ptr());

            if !core::ptr::eq(*expected_next, next_ptr) {
                println!("error inserting, nexts are not equal; {:?}, {:?}, {:?}", expected_next, next_ptr, i);
                return Err(i)
            }
            */

            if core::ptr::eq(next_ptr, new_node.as_ptr()) {
                // println!("THE SAME!! next: {:?}, new: {:?}", next_ptr, new_node.as_ptr());
            }

            // we check if the next node is actually lower in key than our current node.
            if next.as_ref()
                .and_then(|n| if n.key <= new_node.key && !new_node.removed() {
                    Some(())
                } else {
                    None
                }).is_some()
            {
                /*
                println!("{} or {}",!next.is_null()
                && (*next).key <= (*new_node).key
                && !(*next).removed(), prev.as_ref().removed() );
                println!("prev: {:?}, new: {:?}, next: {:?}, level: {}", prev.node.as_ptr(), new_node.node, next, i);
                if !next.is_null() && !self.is_head(prev.node.as_ptr()) {
                    println!(
                        "prev: {:?}, new: {:?}, next: {:?}, level: {}", 
                        (*(prev.node.as_ptr() as *mut Node<u8, ()>)).key, 
                        (*(new_node.node.as_ptr() as *mut Node<u8, ()>)).key, 
                        (*(next as *mut Node<u8, ()>)).key, i
                    );
                }
                println!("error inserting, failed to swap");
                */
                return Err(i);
            }
            
            // Swap the previous' next node into the new_node's level
            // It could be the case that we link ourselves to the previous node, but just as we do
            // this `next` attempts to unlink itself and fails. So while we succeeded, `next`
            // repeats its search and finds that we are the next
            new_node.levels[i].store_ptr(next_ptr);

            // Swap the new_node into the previous' level. If the previous' level has changed since
            // the search, we repeat the search from this level.
            if let Err((_other, _tag)) = prev.levels[i].compare_exchange(
                next_ptr, 
                new_node.as_ptr()
            ) {
                let _ = prev.levels[i].compare_exchange_with_tag(next_ptr, 2, next_ptr, 0);
                // println!("failed to swap on link; next_ptr: {:?}, tagged: {:?}, tag: {}", next_ptr, _other, _tag);
                return Err(i);
            }
        }
        Ok(())
    }

    #[allow(unused_assignments)]
    pub fn remove(&self, key: &K) -> Option<(K, V)>
    where
        K: Send,
        V: Send,
    {
    match self.find(key, false) {
        SearchResult {
                target: Some(target),
                mut prev,
            } => {
                // println!("found target: {:?}", target);

                // Set the target state to being removed
                // If this errors, it is already being removed by someone else
                // and thus we exit early.
                // println!("checking if {:?} has been removed", target);

                if target.set_removed().is_err() {
                    return None;
                }

                // println!("{:?} has not been removed yet", target);
                
                // # Safety:
                // 1. `key` and `val` will not be tempered with.
                // TODO This works for now, yet once `Atomic` is used
                // this may need to change.
                let (key, val, mut height) = unsafe {
                    (
                        core::ptr::read(&target.key),
                        core::ptr::read(&target.val),
                        target.height()
                    )
                };

                // #Safety:
                // 1. The height we got from the `node` guarantees it is a valid height for levels.
                'unlink: while let Err(new_height) = unsafe { 
                    self.unlink(&target, height, prev) 
                } {
                    (height, prev) = {
                        loop {
                            if let SearchResult {
                                target: Some(new_target),
                                prev,
                            } = self.find(&key, true)
                            {
                                if new_target.as_ptr() == target.as_ptr() {
                                    break (new_height, prev)
                                }
                                // println!("ON search, not equal!!");
                                // println!("retried search for: {:?}, prev is now {:?}", target.as_ptr(), &prev[0..target.as_ref().height()]);
                            } else {
                                break 'unlink;
                            }
                        }
                    };
                }

                // TODO Ensure the safety of this!
                // #Safety:
                //
                // we are the only thread that has permission to drop this node.
                self.retire_node(target.as_ptr());

                // We see if we can drop some pointers in the list.
                self.garbage.domain.eager_reclaim();

                self.state.len.fetch_sub(1, Ordering::Relaxed);

                Some((key, val))
            }
            _ => None,
        }
    }

    /// Logically removes the node from the list by linking its adjacent nodes to one-another.
    ///
    /// # Safety
    /// 1. All indices in [0, height) are valid indices for `node.levels`.
    unsafe fn unlink<'a>(
        &self,
        node: &'a NodeRef<'a, K, V>,
        height: usize,
        previous_nodes: [(NodeRef<'a, K, V>, Option<NodeRef<'a, K, V>>); HEIGHT],
    ) -> Result<(), usize> {
        // safety check against UB caused by unlinking the head
        if self.is_head(node.as_ptr()) {
            panic!()
        }

        if node.build() {
            return Err(height);
        }

        // # Safety
        //
        // 1.-3. Some as method and covered by method caller.
        // 4. We are not unlinking the head. - Covered by previous safety check.
        for (i, (prev, next)) in previous_nodes.iter().enumerate().take(height).rev() {
            let (new_next, _tag) = node.levels[i].load_decomposed();
            let next_ptr = next.as_ref().map_or(core::ptr::null_mut(), |n| n.as_ptr());

            // We check if the previous node is being removed after we have already unlinked
            // from it as the prev nodes expects us to do this.
            // We still need to stop the unlink here, as we will have to relink to the actual,
            // lively previous node at this level as well.
            /*
            if prev.removed() {
                // println!("error, prev is removed");
                return Err(i + 1);
            }
            */

            // If someone has already unlinked the node at this level, we simply continue.
            /*
            if core::ptr::eq(prev.levels[i].load_ptr(), new_next) {
                println!("NODE ALREADY BUILT!");
                continue;
            } 
            */

            if !core::ptr::eq(node.as_ptr(), next_ptr) {
                // println!("NEXT is NOT node! {:?} vs {:?}", node.as_ptr(), next_ptr);
                // println!("failed at ptr equals: {:?} vs {:?}", node.as_ptr(), *next);
                return Err(i + 1);
            }

            // tag the pointer as removed
            if let Err(_) = node.levels[i].compare_exchange_with_tag(new_next, _tag, new_next, 1) {
                // println!("FAILED to TAG!");
                return Err(i + 1);
            };
            // println!("ptr: {:?}, tag: {:?}", node.levels[i].load_decomposed().0, node.levels[i].load_decomposed().1);

            // Performs a compare_exchange, expecting the old value of the pointer to be the current
            // node. If it is not, we cannot make any reasonable progress, so we search again.
            if let Err((_other, _tag)) = prev.levels[i].compare_exchange(
                node.as_ptr(),
                new_next,
            ) {
                let _ = prev.levels[i].compare_exchange_with_tag(node.as_ptr(), 2, node.as_ptr(), 0);
                // println!("error unlinking at swap; expected: {:?}, actual: {:?}, tag: {}", node.as_ptr(), _other, _tag);
                return Err(i + 1);
            }

            if let Err(_) = node.levels[i].compare_exchange_with_tag(new_next, 1, new_next, 2) {
                // println!("FAILED to TAG with 2!");
            };
        }

        Ok(())
    }

    /// Unlink [Node](Node) `curr` at the given level of [Node](Node) `prev` by exchanging the pointer for `next`.
    ///
    /// # Safety
    ///
    /// 1. `prev`, `curr`, are protected accesses.
    #[allow(unused)]
    unsafe fn unlink_level<'a>(
        prev: NodeRef<'a, K, V>,
        curr: NodeRef<'a, K, V>,
        next: NodeRef<'a, K, V>,
        level: usize,
    ) -> Result<NodeRef<'a, K, V>, ()> {
        // The pointer to `next` is tagged to signal unlinking. 
        curr.levels[level]
            .try_tag(next.as_ptr(), 1)
            .and(prev.levels[level].compare_exchange(
                    curr.as_ptr(), 
                    next.as_ptr()
                )
                .map(|s| s.0)
                .map_err(|e| e.0)
            )
            .and(curr.levels[level].try_tag(next.as_ptr(), 2))
            .map(|s| next)
            .map_err(|e| ())
    }

    fn retire_node(&self, node_ptr: *mut Node<K, V>) {
        unsafe {
            self.garbage
                .domain
                .retire_ptr_with(node_ptr, |ptr: *mut dyn Reclaim| {
                    Node::<K, V>::dealloc(ptr as *mut Node<K, V>);
                });
        }
    }

    fn find<'a>(&'a self, key: &K, search_removed: bool) -> SearchResult<'a, K, V> {
        let head = unsafe { &(*self.head.as_ptr()) };

        let mut prev = unsafe {
            let mut prev: [core::mem::MaybeUninit<(NodeRef<'a, K, V>, Option<NodeRef<'a, K, V>>)>; HEIGHT] 
                = core::mem::MaybeUninit::uninit().assume_init();

            for (i, level) in prev.iter_mut().enumerate() {
                core::ptr::write(
                    level.as_mut_ptr(), 
                    (NodeRef::from_raw(self.head.cast::<Node<K, V>>().as_ptr()), NodeRef::from_maybe_tagged(&self.head.as_ref().levels[i]))
                )
            }

            core::mem::transmute::<_, [(NodeRef<'a, K, V>, Option<NodeRef<'a, K, V>>); HEIGHT]>(prev)
        };


        '_search: loop {
            let mut level = self.state.max_height.load(Ordering::Relaxed);
            // Find the first and highest node tower
            while level > 1 && head.levels[level - 1].load_ptr().is_null() {
                level -= 1;
            }

            // We need not protect the head, as it will always be valid, as long as we are in a sane
            // state.
            let mut curr = NodeRef::from_raw(self.head.as_ptr().cast::<Node<K, V>>());

            // steps:
            // 1. Go through each level until we reach a node with a key GEQ to ours or that is null
            //     1.1 If we are equal, then the node must either be marked as removed or removed nodes
            //       are allowed in this search.
            //       Should this be the case, then we drop down a level while also protecting a pointer
            //       to the current node, in order to keep the `Level` valid in our `prev` array.
            //     1.2 If we the `next` node is less or equal but removed and removed nodes are
            //       disallowed, then we set our current node to the next node.
            while level > 0 {
                let next = NodeRef::from_maybe_tagged(&curr.levels[level - 1]);

            /*
                unsafe {
                    next.as_ref().map(|n| println!(
                            "accessing next: {:?}({}), from: {:?}({}) at level: {}, while searching for {}", 
                            n.as_ptr(),
                            (*(n.as_ptr() as *mut Node<u8, ()>)).key, 
                            curr.as_ptr(),
                            (*(curr.as_ptr() as *mut Node<u8, ()>)).key, 
                            level - 1,
                            *(key as *const _ as *const u8)
                    ));
                }
                        "accessing next: {:?}, from: {:?} at level: {}, while searching for {}", 
                        (*(*n as *mut Node<u8, ()>)).key, 
                        (*(curr as *mut Node<u8, ()>)).key, 
                        level - 1,
                        *(&key as *const _ as *const u8)
                */

                match next {
                    Some(next) 
                        // This check should ensure that we always get a non-removed node, if there
                        // is one, of our target key, as long as allow removed is set to false.
                        if (*next).key < *key || ((*next).key == *key && !(*next).removed() && search_removed) => {
                        // If the current node is being removed, we try to help unlinking it at this level.
                        /*
                        if (*curr).removed() {
                            if let Err(_) = Self::unlink_level(prev[level - 1].0.as_ptr(), curr, next, level - 1) {
                                continue 'search;
                            }
                            println!("successfully unlinked at level");
                        }
                        */
                        
                        // Update previous_nodes.
                        prev[level - 1] = (curr, Some(next.clone()));

                        curr = next;
                    },
                    next => {
                        /*
                        if (*curr).removed() {
                            if let Err(_) = Self::unlink_level(prev[level - 1].0.as_ptr(), curr, next, level - 1) {
                                continue 'search;
                            }
                            println!("helped unlink at level");
                        }
                        */
                        // Update previous_nodes.
                        prev[level - 1] = (curr.clone(), next);

                        level -= 1;
                    }
                }
            }

            return match NodeRef::from_maybe_tagged(&prev[0].0.as_ref().levels[0]) {
                Some(next) if next.key == *key && (search_removed || !next.removed()) => SearchResult { prev, target: Some(next) },
                _ => SearchResult { prev, target: None }
            }
        }
    }

    pub fn get<'a>(&'a self, key: &K) -> Option<Entry<'a, K, V>> {
        if self.is_empty() {
            return None;
        }

        // Perform safety check for whether we are dealing with the head.
        match self.find(key, false) {
            SearchResult {
                target: Some(target),
                ..
            } => Some(Entry::from(target)),
            _ => None,
        }
    }

    fn is_head(&self, ptr: *const Node<K, V>) -> bool {
        std::ptr::eq(ptr, self.head.as_ptr().cast())
    }

    pub fn get_first<'a>(&'a self) -> Option<Entry<'a, K, V>> {
        if self.is_empty() {
            return None;
        }

        let mut curr = unsafe {
            NodeRef::from_ptr(self.head.as_ref().levels[0].as_std())?
        };

        while let Some(next) = NodeRef::from_maybe_tagged(&curr.levels[0]) {
            curr = next;

            if !curr.removed() {
                return Some(Entry::from(curr));
            }
        }

        None
    }

    pub fn get_last<'a>(&'a self) -> Option<Entry<'a, K, V>> {
        if self.is_empty() {
            return None;
        }

        'last: loop {
            let mut curr =
                unsafe { NodeRef::from_maybe_tagged(&self.head.as_ref().levels[0])? };

            while let Some(next) = NodeRef::from_maybe_tagged(&curr.levels[0]) {
                curr = next;
            }

            if curr.removed() {
                continue 'last;
            }

            return Some(Entry::from(curr))
        }
    }
}

impl<'domain, K, V> Default for SkipList<'domain, K, V>
where
    K: Sync,
    V: Sync,
{
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl<'domain, K, V> Send for SkipList<'domain, K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
}

unsafe impl<'domain, K, V> Sync for SkipList<'domain, K, V>
where
    K: Send + Sync,
    V: Send + Sync,
{
}

impl<'domain, K, V> skiplist::SkipList<K, V> for SkipList<'domain, K, V>
where
    K: Ord + Send + Sync,
    V: Send + Sync,
{
    type Entry<'a> = Entry<'a, K, V> where K: 'a, V: 'a, Self: 'a;

    fn new() -> Self {
        SkipList::new()
    }

    fn insert(&self, key: K, value: V) -> Option<V> {
        self.insert(key, value)
    }

    fn remove(&self, key: &K) -> Option<(K, V)> {
        self.remove(key)
    }

    fn get<'a>(&'a self, key: &K) -> Option<Self::Entry<'a>> {
        self.get(key)
    }

    fn last<'a>(&'a self) -> Option<Self::Entry<'a>> {
        self.get_first()
    }

    fn front<'a>(&'a self) -> Option<Self::Entry<'a>> {
        self.get_last()
    }

    fn len(&self) -> usize {
        self.len()
    }
}

// TODO Make sure this is sound!
impl<'domain, K, V> From<super::skiplist::SkipList<'domain, K, V>> for SkipList<'domain, K, V>
where
    K: Sync,
    V: Sync,
{
    fn from(list: super::skiplist::SkipList<'domain, K, V>) -> Self {
        unsafe { core::mem::transmute(list) }
    }
}

#[allow(dead_code)]
pub struct Entry<'a, K: 'a, V: 'a> {
    node: core::ptr::NonNull<Node<K, V>>,
    _hazard: haphazard::HazardPointer<'a, Global>,
}

impl<'a, K, V> skiplist::Entry<'a, K, V> for Entry<'a, K, V> {
    fn val(&self) -> &V {
        // #Safety
        //
        // Our `HazardPointer` ensures that our pointers is valid.
        unsafe { &self.node.as_ref().val }
    }

    fn key(&self) -> &K {
        // #Safety
        //
        // Our `HazardPointer` ensures that our pointers is valid.
        unsafe { &self.node.as_ref().key }
    }
}

struct SearchResult<'a, K, V> {
    prev: [(NodeRef<'a, K, V>, Option<NodeRef<'a, K, V>>); HEIGHT],
    target: Option<NodeRef<'a, K, V>>,
}

impl<'a, K, V> Debug for SearchResult<'a, K, V>
where
    K: Debug + Default,
    V: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchResult")
            .field("target", &self.target)
            .finish()
    }
}

impl<'a, K, V> Borrow<K> for Entry<'a, K, V> {
    fn borrow(&self) -> &K {
        unsafe { &self.node.as_ref().key }
    }
}

impl<'a, K, V> AsRef<V> for Entry<'a, K, V> {
    fn as_ref(&self) -> &V {
        unsafe { &self.node.as_ref().val }
    }
}

#[allow(dead_code)]
struct NodeRef<'a, K, V> {
    node: NonNull<Node<K, V>>,
    _hazard: HazardPointer<'a>
}

impl<'a, K, V> NodeRef<'a, K, V> {
    fn from_raw_in(ptr: *mut Node<K, V>, domain: &'a Domain<Global>) -> Self {
        let mut _hazard = HazardPointer::new_in_domain(domain);
        _hazard.protect_raw(ptr);
        unsafe {
            NodeRef { node: NonNull::new_unchecked(ptr), _hazard }
        }
    }

    fn from_raw(ptr: *mut Node<K, V>) -> Self {
        Self::from_raw_in(ptr, Domain::global())
    }

    fn from_ptr_in(ptr: &AtomicPtr<Node<K, V>>, domain: &'a Domain<Global>) -> Option<Self> {
        let mut _hazard = HazardPointer::new_in_domain(domain);
        let node = _hazard.protect_ptr(ptr)?.0;

        Some(NodeRef { node, _hazard })
    }

    fn from_ptr(ptr: &AtomicPtr<Node<K, V>>) -> Option<Self> {
        Self::from_ptr_in(ptr, Domain::global())
    }

    fn as_ptr(&self) -> *mut Node<K, V> {
        self.node.as_ptr()
    }
}

impl<'a, K, V> AsRef<Node<K, V>> for NodeRef<'a, K, V> {
    fn as_ref(&self) -> &Node<K, V> {
        unsafe { &(*self.as_ptr()) }
    }
}

impl<'a, K, V> core::ops::Deref for NodeRef<'a, K, V> {
    type Target = Node<K, V>;
    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl<'a, K, V> core::ops::DerefMut for NodeRef<'a, K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut (*self.as_ptr()) }
    }
}

impl<'a, K, V> core::fmt::Debug for NodeRef<'a, K, V> 
where 
    K: Debug, 
    V: Debug 
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe {
            f.debug_struct("NodeRef").field("node", self.node.as_ref()).finish()
        }
    }
}

impl<'a, K, V> From<NodeRef<'a, K, V>> for Entry<'a, K, V> {
    fn from(value: NodeRef<'a, K, V>) -> Self {
        let NodeRef {
            node,
            _hazard
        } = value;

        Entry { node, _hazard }
    }
}

impl<'a, K, V> Clone for NodeRef<'a, K, V> {
    fn clone(&self) -> Self {
        let mut _hazard = HazardPointer::new();
        _hazard.protect_raw(self.node.as_ptr());

        NodeRef { node: self.node.clone(), _hazard }
    }
}

impl<'a, K, V> core::cmp::PartialEq for NodeRef<'a, K, V> {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self.node.as_ptr(), other.node.as_ptr())
    }
}

impl<'a, K, V> core::cmp::Eq for NodeRef<'a, K, V> {}

#[repr(transparent)]
struct OwnedNode<K, V>(*mut Node<K, V>);

unsafe impl<K, V> Send for OwnedNode<K, V> 
where K: Send + Sync,
      V: Send + Sync
{
}

unsafe impl<K, V> Sync for OwnedNode<K, V> 
where K: Send + Sync,
      V: Send + Sync
{
}

impl<K, V> From<*mut Node<K, V>> for OwnedNode<K, V> {
    fn from(node: *mut Node<K, V>) -> Self {
        OwnedNode(node)
    }
}

impl<K, V> Drop for OwnedNode<K, V> {
    fn drop(&mut self) {
        unsafe {
            Node::dealloc(self.0)
        }
    }
}

impl<K, V> core::ops::Deref for OwnedNode<K, V> {
    type Target = Node<K, V>;

    fn deref(&self) -> &Self::Target {
        unsafe { &(*self.0) }
    }
}

impl<K, V> core::ops::DerefMut for OwnedNode<K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {&mut (*self.0)}
    }
}

#[cfg(test)]
mod sync_test {
    use rand::Rng;

    use super::*;

    #[test]
    fn test_new_node_sync() {
        let node = Node::new(100, "hello", 1);
        let other = Node::new(100, "hello", 1);
        unsafe { println!("node 1: {:?},", *node) };
        unsafe { println!("node 2: {:?},", *other) };
        let other = unsafe {
            let node = Node::alloc(1);
            core::ptr::write(&mut (*node).key, 100);
            core::ptr::write(&mut (*node).val, "hello");
            node
        };

        unsafe { println!("node 1: {:?}, node 2: {:?}", *node, *other) };

        unsafe { assert_eq!(*node, *other) };
    }

    #[test]
    fn test_new_list_sync() {
        let _: SkipList<'_, usize, usize> = SkipList::new();
    }

    #[test]
    fn test_insert_sync() {
        let list = SkipList::new();
        let mut rng: u16 = rand::random();

        for _ in 0..100_000 {
            rng ^= rng << 3;
            rng ^= rng >> 12;
            rng ^= rng << 7;
            list.insert(rng, "hello there!");
        }
    }

    #[test]
    fn test_rand_height_sync() {
        let mut list: SkipList<'_, i32, i32> = SkipList::new();
        let node = Node::new_rand_height("Hello", "There!", &mut list);

        assert!(!node.is_null());
        let height = unsafe { (*node).levels.pointers.len() };

        println!("height: {}", height);

        unsafe {
            println!("{}", *node);
        }

        unsafe {
            let _ = Box::from_raw(node);
        }
    }

    #[test]
    fn test_insert_verbose_sync() {
        let list = SkipList::new();

        list.insert(1, 1);
        unsafe {
            let mut node = &(*list.head.as_ptr()).levels[0];
            while !node.as_std().load(Ordering::Relaxed).is_null() {
                println!(
                    "{:?}-key: {:?}, val: {:?}----------------------------------------------",
                    node.as_std(),
                    (*node.as_std().load(Ordering::Relaxed)).key,
                    (*node.as_std().load(Ordering::Relaxed)).key
                );
                print!("                                ");
                for level in 0..(*node.as_std().load(Ordering::Relaxed)).height() {
                    let ref left = (*node.as_std().load(Ordering::Relaxed)).levels[level];
                    print!("{:?} | ", left.as_std());
                }
                println!();
                node = &(*node.as_std().load(Ordering::Relaxed)).levels[0];
            }
        }

        println!("/////////////////////////////////////////////////////////////////////////");
        println!("/////////////////////////////////////////////////////////////////////////");

        list.insert(2, 2);

        unsafe {
            let mut node = &(*list.head.as_ptr()).levels[0];
            while !node.as_std().load(Ordering::Relaxed).is_null() {
                println!(
                    "{:?}-key: {:?}, val: {:?}----------------------------------------------",
                    node.as_std(),
                    (*node.as_std().load(Ordering::Relaxed)).key,
                    (*node.as_std().load(Ordering::Relaxed)).key
                );
                print!("                                ");
                for level in 0..(*node.as_std().load(Ordering::Relaxed)).height() {
                    let ref left = (*node.as_std().load(Ordering::Relaxed)).levels[level];
                    print!("{:?} | ", left.as_std());
                }
                println!();
                node = &(*node.as_std().load(Ordering::Relaxed)).levels[0];
            }
        }

        println!("/////////////////////////////////////////////////////////////////////////");
        println!("/////////////////////////////////////////////////////////////////////////");

        list.insert(5, 3);

        unsafe {
            let mut node = &(*list.head.as_ptr()).levels[0];
            while !node.as_std().load(Ordering::Relaxed).is_null() {
                println!(
                    "{:?}-key: {:?}, val: {:?}----------------------------------------------",
                    node.as_std(),
                    (*node.as_std().load(Ordering::Relaxed)).key,
                    (*node.as_std().load(Ordering::Relaxed)).key
                );
                print!("                                ");
                for level in 0..(*node.as_std().load(Ordering::Relaxed)).height() {
                    let ref left = (*node.as_std().load(Ordering::Relaxed)).levels[level];
                    print!("{:?} | ", left.as_std());
                }
                println!();
                node = &(*node.as_std().load(Ordering::Relaxed)).levels[0];
            }
        }

        println!("trying to drop");
    }

    #[test]
    fn test_remove() {
        let list = SkipList::new();
        let mut rng: u16 = rand::random();

        for _ in 0..100_000 {
            rng ^= rng << 3;
            rng ^= rng >> 12;
            rng ^= rng << 7;
            list.insert(rng, "hello there!");
        }
        for _ in 0..100_000 {
            rng ^= rng << 3;
            rng ^= rng >> 12;
            rng ^= rng << 7;
            list.remove(&rng);
        }
    }

    #[test]
    fn test_verbose_remove() {
        let list = SkipList::new();

        list.insert(1, 1);
        list.insert(2, 2);
        list.insert(2, 2);
        list.insert(5, 3);

        unsafe {
            let mut node = &(*list.head.as_ptr()).levels[0];
            while !node.as_std().load(Ordering::Relaxed).is_null() {
                println!(
                    "{:?}-key: {:?}, val: {:?}----------------------------------------------",
                    node.as_std(),
                    (*node.as_std().load(Ordering::Relaxed)).key,
                    (*node.as_std().load(Ordering::Relaxed)).key
                );
                print!("                                ");
                for level in 0..(*node.as_std().load(Ordering::Relaxed)).height() {
                    let ref left = (*node.as_std().load(Ordering::Relaxed)).levels[level];
                    print!("{:?} | ", left.as_std());
                }
                println!();
                node = &(*node.as_std().load(Ordering::Relaxed)).levels[0];
            }
        }

        assert!(list.remove(&1).is_some());

        println!("/////////////////////////////////////////////////////////////////////////");
        println!("/////////////////////////////////////////////////////////////////////////");

        unsafe {
            let mut node = &(*list.head.as_ptr()).levels[0];
            while !node.as_std().load(Ordering::Relaxed).is_null() {
                println!(
                    "{:?}-key: {:?}, val: {:?}----------------------------------------------",
                    node.as_std(),
                    (*node.as_std().load(Ordering::Relaxed)).key,
                    (*node.as_std().load(Ordering::Relaxed)).key
                );
                print!("                                ");
                for level in 0..(*node.as_std().load(Ordering::Relaxed)).height() {
                    let ref left = (*node.as_std().load(Ordering::Relaxed)).levels[level];
                    print!("{:?} | ", left.as_std());
                }
                println!();
                node = &(*node.as_std().load(Ordering::Relaxed)).levels[0];
            }
        }

        println!("removing 6");
        assert!(list.remove(&6).is_none());
        println!("removing 1");
        assert!(list.remove(&1).is_none());
        println!("removing 5");
        assert!(list.remove(&5).is_some());
        println!("removing 2");
        assert!(list.remove(&2).is_some());
        //list.remove(&2);

        println!("/////////////////////////////////////////////////////////////////////////");
        println!("/////////////////////////////////////////////////////////////////////////");

        unsafe {
            let mut node = &(*list.head.as_ptr()).levels[0];
            while !node.as_std().load(Ordering::Relaxed).is_null() {
                println!(
                    "{:?}-key: {:?}, val: {:?}----------------------------------------------",
                    node.as_std(),
                    (*node.as_std().load(Ordering::Relaxed)).key,
                    (*node.as_std().load(Ordering::Relaxed)).key
                );
                print!("                                ");
                for level in 0..(*node.as_std().load(Ordering::Relaxed)).height() {
                    let ref left = (*node.as_std().load(Ordering::Relaxed)).levels[level];
                    print!("{:?} | ", left.as_std());
                }
                println!();
                node = &(*node.as_std().load(Ordering::Relaxed)).levels[0];
            }
        }

        assert_eq!(list.len(), 0);
    }

    #[test]
    fn test_find_removed() {
        let list = SkipList::new();

        list.insert(3, ());

        list.insert(4, ());

        list.insert(5, ());

        unsafe {
            assert!(list.find(&3, false).target.is_some());
            assert!(list.find(&4, false).target.is_some());
        }

        // manually get reference to the nodes
        let node_3 = unsafe { &mut (*(*list.head.as_ptr()).levels[0].load_ptr()) };
        let node_4 =
            unsafe { &mut (*(*(*list.head.as_ptr()).levels[0].load_ptr()).levels[0].load_ptr()) };
        let node_5 = unsafe {
            &mut (*(*(*(*list.head.as_ptr()).levels[0].load_ptr()).levels[0].load_ptr()).levels[0]
                .load_ptr())
        };

        // make sure it is the right node
        assert_eq!(node_3.key, 3);
        println!("{:?}", node_3);
        assert_eq!(node_4.key, 4);
        println!("{:?}", node_4);
        assert_eq!(node_5.key, 5);
        println!("{:?}", node_5);

        // remove the node logically
        node_4.set_removed();

        unsafe {
            assert!(list.find(&4, false).target.is_none());

            assert!(list.find(&4, true).target.is_some());

            println!("{:?}", list.find(&3, false));
        }

        assert!(!node_3.removed());

        assert!(list.remove(&4).is_none());

        // remove the node logically
        node_4.height_and_removed.store(
            node_4.height_and_removed.load(Ordering::SeqCst) ^ (1 as u32) << 31,
            Ordering::SeqCst,
        );

        assert!(!node_4.removed());

        assert!(list.remove(&4).is_some());
    }

    #[test]
    fn test_sync_remove() {
        use std::sync::Arc;
        let list = Arc::new(SkipList::new());
        let mut rng = rand::thread_rng();

        for _ in 0..10_000 {
            list.insert(rng.gen::<u16>(), ());
        }
        let threads = (0..30)
            .map(|_| {
                let list = list.clone();
                std::thread::spawn(move || {
                    let mut rng = rand::thread_rng();
                    for _ in 0..1_000 {
                        let target = &rng.gen::<u16>();
                        let success = list.remove(&target);
                        if success.is_some() {
                            println!("{:?}", success);
                        }
                    }
                    println!("all done!");
                })
            })
            .collect::<Vec<_>>();

        for thread in threads {
            thread.join().unwrap()
        }

        unsafe {
            let mut node = (*list.head.as_ptr()).levels[0].load_ptr();
            while !node.is_null() {
                println!("{:?}", *node);
                node = (*node).levels[0].load_ptr();
            }
        }
    }

    #[test]
    fn test_sync_insert() {
        use std::sync::Arc;
        let list = Arc::new(SkipList::new());

        let threads = (0..30)
            .map(|_| {
                let list = list.clone();
                std::thread::spawn(move || {
                    let mut rng = rand::thread_rng();
                    for _ in 0..10_000 {
                        let target = rng.gen::<u8>();

                        list.insert(target, ());
                    }
                })
            })
            .collect::<Vec<_>>();

        for thread in threads {
            thread.join().unwrap()
        }

        unsafe {
            let mut node = (*list.head.as_ptr()).levels[0].load_ptr();
            while !node.is_null() {
                println!("{:?} - {:?}", node, *node);
                node = (*node).levels[0].load_ptr();
            }
        }
    }

    #[test]
    fn test_sync_inmove() {
        use std::sync::Arc;
        let list = Arc::new(SkipList::new());

        let threads = (0..20)
            .map(|_| {
                let list = list.clone();
                std::thread::spawn(move || {
                    let mut rng = rand::thread_rng();
                    for _ in 0..10_000 {
                        let target = rng.gen::<u8>();
                        if rng.gen::<u8>() % 5 == 0 {
                            list.remove(&target);
                        } else {
                            list.insert(target, ());
                        }
                    }
                })
            })
            .collect::<Vec<_>>();

        for thread in threads {
            thread.join().unwrap()
        }

        unsafe {
            let mut node = NodeRef::from_maybe_tagged(&list.head.as_ref().levels[0]);
            while let Some(next) = node {
                println!("{:?} - {:?}", next.as_ptr(), *next);
                node = NodeRef::from_maybe_tagged(&next.levels[0]);
            }
        }
    }
}
