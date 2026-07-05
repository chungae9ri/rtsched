// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Generic intrusive red-black tree.
//!
//! The tree stores links directly inside caller-owned entities and does not
//! allocate. Each entity type supplies accessors for its embedded `RbNode` and
//! its own ordering key.

use core::cmp::Ordering;
use core::marker::PhantomData;
use core::ptr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Color {
    Red,
    Black,
}

/// Intrusive red-black tree links embedded inside an owning entity.
#[repr(C)]
pub struct RbNode {
    parent: *mut RbNode,
    left: *mut RbNode,
    right: *mut RbNode,
    color: Color,
}

impl RbNode {
    pub const fn new() -> Self {
        Self {
            parent: ptr::null_mut(),
            left: ptr::null_mut(),
            right: ptr::null_mut(),
            color: Color::Red,
        }
    }

    pub fn reset_links(&mut self) {
        self.parent = ptr::null_mut();
        self.left = ptr::null_mut();
        self.right = ptr::null_mut();
        self.color = Color::Red;
    }

    #[allow(dead_code)]
    pub fn is_linked(&self) -> bool {
        !self.parent.is_null() || !self.left.is_null() || !self.right.is_null()
    }
}

impl Default for RbNode {
    fn default() -> Self {
        Self::new()
    }
}

/// Entity contract for using `RBTree`.
///
/// # Safety
///
/// Implementors must return the address of the embedded `RbNode` that belongs
/// to the provided entity, and must recover the original entity pointer from a
/// node pointer produced by that accessor.
pub(crate) unsafe trait RBTreeNode: Sized {
    fn node(entity: *mut Self) -> *mut RbNode;
    fn entity_of(node: *mut RbNode) -> *mut Self;
    fn entity_of_const(node: *const RbNode) -> *const Self;

    /// Compare two entities. Equal keys must be resolved with a strict
    /// tie-breaker, usually the entity address, so tree ordering is total.
    unsafe fn cmp(a: *const Self, b: *const Self) -> Ordering;
}

/// Intrusive red-black tree over caller-owned entity type `T`.
pub(crate) struct RBTree<T: RBTreeNode> {
    root: *mut RbNode,
    len: usize,
    _entity: PhantomData<T>,
}

impl<T: RBTreeNode> Default for RBTree<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: RBTreeNode> RBTree<T> {
    pub const fn new() -> Self {
        Self {
            root: ptr::null_mut(),
            len: 0,
            _entity: PhantomData,
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.root.is_null()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[allow(dead_code)]
    pub fn root(&self) -> *mut T {
        T::entity_of(self.root)
    }

    pub fn first(&self) -> *mut T {
        T::entity_of(Self::minimum(self.root))
    }

    #[allow(dead_code)]
    pub fn last(&self) -> *mut T {
        T::entity_of(Self::maximum(self.root))
    }

    pub fn next(&self, entity: *mut T) -> *mut T {
        if entity.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            let mut node = T::node(entity);

            if !(*node).right.is_null() {
                return T::entity_of(Self::minimum((*node).right));
            }

            let mut parent = (*node).parent;
            while !parent.is_null() && node == (*parent).right {
                node = parent;
                parent = (*parent).parent;
            }

            T::entity_of(parent)
        }
    }

    pub fn contains(&self, entity: *const T) -> bool {
        let mut current = self.first();

        while !current.is_null() {
            if ptr::eq(current.cast_const(), entity) {
                return true;
            }
            current = self.next(current);
        }

        false
    }

    /// Insert a detached entity into the tree.
    ///
    /// # Safety
    ///
    /// The caller must ensure `entity` is valid for mutation and is not
    /// simultaneously linked into another tree.
    pub unsafe fn insert(&mut self, entity: *mut T) {
        debug_assert!(!entity.is_null());
        debug_assert!(
            !self.contains(entity.cast_const()),
            "entity is already linked into this tree"
        );

        unsafe {
            let node = T::node(entity);
            (*node).reset_links();

            let mut parent = ptr::null_mut();
            let mut current = self.root;

            while !current.is_null() {
                parent = current;
                match Self::cmp_nodes(node, current) {
                    Ordering::Less => current = (*current).left,
                    Ordering::Greater => current = (*current).right,
                    Ordering::Equal => unreachable!("entity ordering must be strict"),
                }
            }

            (*node).parent = parent;
            if parent.is_null() {
                self.root = node;
            } else if Self::cmp_nodes(node, parent) == Ordering::Less {
                (*parent).left = node;
            } else {
                (*parent).right = node;
            }

            self.insert_fixup(node);
            self.len += 1;
        }
    }

    /// Remove an entity from the tree.
    ///
    /// # Safety
    ///
    /// The caller must ensure `entity` currently belongs to this tree.
    pub unsafe fn remove(&mut self, entity: *mut T) -> *mut T {
        if entity.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            let node = T::node(entity);
            let mut y = node;
            let mut y_original_color = (*y).color;
            let x;
            let x_parent;

            if (*node).left.is_null() {
                x = (*node).right;
                x_parent = (*node).parent;
                self.transplant(node, (*node).right);
            } else if (*node).right.is_null() {
                x = (*node).left;
                x_parent = (*node).parent;
                self.transplant(node, (*node).left);
            } else {
                y = Self::minimum((*node).right);
                y_original_color = (*y).color;
                x = (*y).right;

                if (*y).parent == node {
                    x_parent = y;
                    if !x.is_null() {
                        (*x).parent = y;
                    }
                } else {
                    x_parent = (*y).parent;
                    self.transplant(y, (*y).right);
                    (*y).right = (*node).right;
                    (*(*y).right).parent = y;
                }

                self.transplant(node, y);
                (*y).left = (*node).left;
                (*(*y).left).parent = y;
                (*y).color = (*node).color;
            }

            if y_original_color == Color::Black {
                self.remove_fixup(x, x_parent);
            }

            (*node).reset_links();
            self.len -= 1;
            entity
        }
    }

    /// Remove and return the left-most entity in the tree.
    pub unsafe fn pop_first(&mut self) -> Option<&mut T> {
        let first = self.first();
        if first.is_null() {
            return None;
        }

        unsafe {
            self.remove(first);
            Some(&mut *first)
        }
    }

    fn color_of(node: *mut RbNode) -> Color {
        if node.is_null() {
            Color::Black
        } else {
            unsafe { (*node).color }
        }
    }

    fn minimum(mut node: *mut RbNode) -> *mut RbNode {
        unsafe {
            while !node.is_null() && !(*node).left.is_null() {
                node = (*node).left;
            }
        }
        node
    }

    #[allow(dead_code)]
    fn maximum(mut node: *mut RbNode) -> *mut RbNode {
        unsafe {
            while !node.is_null() && !(*node).right.is_null() {
                node = (*node).right;
            }
        }
        node
    }

    fn cmp_nodes(a: *const RbNode, b: *const RbNode) -> Ordering {
        unsafe { T::cmp(T::entity_of_const(a), T::entity_of_const(b)) }
    }

    unsafe fn left_rotate(&mut self, x: *mut RbNode) {
        unsafe {
            let y = (*x).right;
            debug_assert!(!y.is_null());

            (*x).right = (*y).left;
            if !(*y).left.is_null() {
                (*(*y).left).parent = x;
            }

            (*y).parent = (*x).parent;
            if (*x).parent.is_null() {
                self.root = y;
            } else if x == (*(*x).parent).left {
                (*(*x).parent).left = y;
            } else {
                (*(*x).parent).right = y;
            }

            (*y).left = x;
            (*x).parent = y;
        }
    }

    unsafe fn right_rotate(&mut self, y: *mut RbNode) {
        unsafe {
            let x = (*y).left;
            debug_assert!(!x.is_null());

            (*y).left = (*x).right;
            if !(*x).right.is_null() {
                (*(*x).right).parent = y;
            }

            (*x).parent = (*y).parent;
            if (*y).parent.is_null() {
                self.root = x;
            } else if y == (*(*y).parent).right {
                (*(*y).parent).right = x;
            } else {
                (*(*y).parent).left = x;
            }

            (*x).right = y;
            (*y).parent = x;
        }
    }

    unsafe fn insert_fixup(&mut self, mut z: *mut RbNode) {
        unsafe {
            while Self::color_of((*z).parent) == Color::Red {
                let parent = (*z).parent;
                let grandparent = (*parent).parent;

                if parent == (*grandparent).left {
                    let uncle = (*grandparent).right;
                    if Self::color_of(uncle) == Color::Red {
                        (*parent).color = Color::Black;
                        (*uncle).color = Color::Black;
                        (*grandparent).color = Color::Red;
                        z = grandparent;
                    } else {
                        if z == (*parent).right {
                            z = parent;
                            self.left_rotate(z);
                        }

                        let parent = (*z).parent;
                        let grandparent = (*parent).parent;
                        (*parent).color = Color::Black;
                        (*grandparent).color = Color::Red;
                        self.right_rotate(grandparent);
                    }
                } else {
                    let uncle = (*grandparent).left;
                    if Self::color_of(uncle) == Color::Red {
                        (*parent).color = Color::Black;
                        (*uncle).color = Color::Black;
                        (*grandparent).color = Color::Red;
                        z = grandparent;
                    } else {
                        if z == (*parent).left {
                            z = parent;
                            self.right_rotate(z);
                        }

                        let parent = (*z).parent;
                        let grandparent = (*parent).parent;
                        (*parent).color = Color::Black;
                        (*grandparent).color = Color::Red;
                        self.left_rotate(grandparent);
                    }
                }
            }

            if !self.root.is_null() {
                (*self.root).color = Color::Black;
            }
        }
    }

    unsafe fn transplant(&mut self, u: *mut RbNode, v: *mut RbNode) {
        unsafe {
            if (*u).parent.is_null() {
                self.root = v;
            } else if u == (*(*u).parent).left {
                (*(*u).parent).left = v;
            } else {
                (*(*u).parent).right = v;
            }

            if !v.is_null() {
                (*v).parent = (*u).parent;
            }
        }
    }

    unsafe fn remove_fixup(&mut self, mut x: *mut RbNode, mut parent: *mut RbNode) {
        unsafe {
            while x != self.root && Self::color_of(x) == Color::Black {
                if x == parent_left(parent) {
                    let mut w = parent_right(parent);

                    if Self::color_of(w) == Color::Red {
                        (*w).color = Color::Black;
                        (*parent).color = Color::Red;
                        self.left_rotate(parent);
                        w = parent_right(parent);
                    }

                    if Self::color_of(left_of(w)) == Color::Black
                        && Self::color_of(right_of(w)) == Color::Black
                    {
                        if !w.is_null() {
                            (*w).color = Color::Red;
                        }
                        x = parent;
                        parent = parent_of(x);
                    } else {
                        if Self::color_of(right_of(w)) == Color::Black {
                            let left = left_of(w);
                            if !left.is_null() {
                                (*left).color = Color::Black;
                            }
                            if !w.is_null() {
                                (*w).color = Color::Red;
                                self.right_rotate(w);
                            }
                            w = parent_right(parent);
                        }

                        if !w.is_null() {
                            (*w).color = (*parent).color;
                        }
                        (*parent).color = Color::Black;
                        let right = right_of(w);
                        if !right.is_null() {
                            (*right).color = Color::Black;
                        }
                        self.left_rotate(parent);
                        x = self.root;
                        parent = ptr::null_mut();
                    }
                } else {
                    let mut w = parent_left(parent);

                    if Self::color_of(w) == Color::Red {
                        (*w).color = Color::Black;
                        (*parent).color = Color::Red;
                        self.right_rotate(parent);
                        w = parent_left(parent);
                    }

                    if Self::color_of(right_of(w)) == Color::Black
                        && Self::color_of(left_of(w)) == Color::Black
                    {
                        if !w.is_null() {
                            (*w).color = Color::Red;
                        }
                        x = parent;
                        parent = parent_of(x);
                    } else {
                        if Self::color_of(left_of(w)) == Color::Black {
                            let right = right_of(w);
                            if !right.is_null() {
                                (*right).color = Color::Black;
                            }
                            if !w.is_null() {
                                (*w).color = Color::Red;
                                self.left_rotate(w);
                            }
                            w = parent_left(parent);
                        }

                        if !w.is_null() {
                            (*w).color = (*parent).color;
                        }
                        (*parent).color = Color::Black;
                        let left = left_of(w);
                        if !left.is_null() {
                            (*left).color = Color::Black;
                        }
                        self.right_rotate(parent);
                        x = self.root;
                        parent = ptr::null_mut();
                    }
                }
            }

            if !x.is_null() {
                (*x).color = Color::Black;
            }
        }
    }
}

fn parent_of(node: *mut RbNode) -> *mut RbNode {
    if node.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*node).parent }
    }
}

fn left_of(node: *mut RbNode) -> *mut RbNode {
    if node.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*node).left }
    }
}

fn right_of(node: *mut RbNode) -> *mut RbNode {
    if node.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*node).right }
    }
}

fn parent_left(node: *mut RbNode) -> *mut RbNode {
    left_of(node)
}

fn parent_right(node: *mut RbNode) -> *mut RbNode {
    right_of(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;
    use std::vec::Vec;

    #[repr(C)]
    struct TestEntity {
        key: u32,
        node: RbNode,
    }

    impl TestEntity {
        const fn new(key: u32) -> Self {
            Self {
                key,
                node: RbNode::new(),
            }
        }
    }

    unsafe impl RBTreeNode for TestEntity {
        fn node(entity: *mut Self) -> *mut RbNode {
            if entity.is_null() {
                ptr::null_mut()
            } else {
                unsafe { ptr::addr_of_mut!((*entity).node) }
            }
        }

        fn entity_of(node: *mut RbNode) -> *mut Self {
            if node.is_null() {
                ptr::null_mut()
            } else {
                unsafe {
                    (node as *mut u8)
                        .sub(offset_of!(TestEntity, node))
                        .cast::<TestEntity>()
                }
            }
        }

        fn entity_of_const(node: *const RbNode) -> *const Self {
            if node.is_null() {
                ptr::null()
            } else {
                unsafe {
                    (node as *const u8)
                        .sub(offset_of!(TestEntity, node))
                        .cast::<TestEntity>()
                }
            }
        }

        unsafe fn cmp(a: *const Self, b: *const Self) -> Ordering {
            unsafe {
                match (*a).key.cmp(&(*b).key) {
                    Ordering::Equal => (a as usize).cmp(&(b as usize)),
                    other => other,
                }
            }
        }
    }

    fn collect_keys(tree: &RBTree<TestEntity>) -> Vec<u32> {
        let mut keys = Vec::new();
        let mut current = tree.first();

        while !current.is_null() {
            unsafe {
                keys.push((*current).key);
            }
            current = tree.next(current);
        }

        keys
    }

    fn assert_rb_invariants(tree: &RBTree<TestEntity>) {
        unsafe {
            if tree.root.is_null() {
                assert_eq!(tree.len(), 0);
                return;
            }

            assert_eq!((*tree.root).color, Color::Black);
            assert_subtree_invariants(tree.root, None, None);
        }
    }

    unsafe fn assert_subtree_invariants(
        node: *mut RbNode,
        min: Option<*const TestEntity>,
        max: Option<*const TestEntity>,
    ) -> usize {
        if node.is_null() {
            return 1;
        }

        unsafe {
            let entity = TestEntity::entity_of_const(node);

            if let Some(min) = min {
                assert_eq!(TestEntity::cmp(min, entity), Ordering::Less);
            }
            if let Some(max) = max {
                assert_eq!(TestEntity::cmp(entity, max), Ordering::Less);
            }

            if (*node).color == Color::Red {
                assert_eq!(color_of((*node).left), Color::Black);
                assert_eq!(color_of((*node).right), Color::Black);
            }

            if !(*node).left.is_null() {
                assert_eq!((*(*node).left).parent, node);
            }
            if !(*node).right.is_null() {
                assert_eq!((*(*node).right).parent, node);
            }

            let left_black_height = assert_subtree_invariants((*node).left, min, Some(entity));
            let right_black_height = assert_subtree_invariants((*node).right, Some(entity), max);

            assert_eq!(left_black_height, right_black_height);
            left_black_height + usize::from((*node).color == Color::Black)
        }
    }

    fn color_of(node: *mut RbNode) -> Color {
        RBTree::<TestEntity>::color_of(node)
    }

    #[test]
    fn insert_keeps_entities_in_key_order() {
        let mut tree = RBTree::<TestEntity>::new();
        let mut entities = [
            TestEntity::new(7),
            TestEntity::new(3),
            TestEntity::new(9),
            TestEntity::new(1),
            TestEntity::new(5),
        ];

        for entity in &mut entities {
            unsafe {
                tree.insert(entity);
            }
            assert_rb_invariants(&tree);
        }

        assert_eq!(tree.len(), entities.len());
        assert_eq!(collect_keys(&tree), [1, 3, 5, 7, 9]);
        unsafe {
            assert_eq!((*tree.first()).key, 1);
            assert_eq!((*tree.last()).key, 9);
        }
    }

    #[test]
    fn contains_reports_tree_membership_including_root() {
        let mut tree = RBTree::<TestEntity>::new();
        let mut root = TestEntity::new(10);
        let mut child = TestEntity::new(5);
        let detached = TestEntity::new(7);

        unsafe {
            tree.insert(&mut root);
            assert!(tree.contains(&root));
            assert!(!tree.contains(&child));

            tree.insert(&mut child);
            assert!(tree.contains(&root));
            assert!(tree.contains(&child));
            assert!(!tree.contains(&detached));
        }
    }

    #[test]
    #[should_panic(expected = "entity is already linked into this tree")]
    fn inserting_same_entity_twice_panics_in_debug_builds() {
        let mut tree = RBTree::<TestEntity>::new();
        let mut entity = TestEntity::new(10);

        unsafe {
            tree.insert(&mut entity);
            tree.insert(&mut entity);
        }
    }

    #[test]
    fn equal_keys_are_ordered_by_entity_address() {
        let mut tree = RBTree::<TestEntity>::new();
        let mut first = TestEntity::new(4);
        let mut second = TestEntity::new(4);

        unsafe {
            tree.insert(&mut first);
            tree.insert(&mut second);
        }

        assert_rb_invariants(&tree);
        assert_eq!(tree.len(), 2);
        assert_eq!(collect_keys(&tree), [4, 4]);
    }

    #[test]
    fn remove_detaches_entity_and_preserves_order() {
        let mut tree = RBTree::<TestEntity>::new();
        let mut entities = [
            TestEntity::new(10),
            TestEntity::new(4),
            TestEntity::new(14),
            TestEntity::new(2),
            TestEntity::new(6),
            TestEntity::new(12),
            TestEntity::new(16),
        ];

        for entity in &mut entities {
            unsafe {
                tree.insert(entity);
            }
        }

        let removed = unsafe { tree.remove(&mut entities[1]) };

        assert!(ptr::eq(removed, &mut entities[1]));
        assert!(!entities[1].node.is_linked());
        assert_eq!(tree.len(), 6);
        assert_eq!(collect_keys(&tree), [2, 6, 10, 12, 14, 16]);
        assert_rb_invariants(&tree);
    }

    #[test]
    fn pop_first_removes_entities_in_order() {
        let mut tree = RBTree::<TestEntity>::new();
        let mut entities = [
            TestEntity::new(8),
            TestEntity::new(3),
            TestEntity::new(11),
            TestEntity::new(1),
        ];

        for entity in &mut entities {
            unsafe {
                tree.insert(entity);
            }
        }

        let mut popped = Vec::new();
        while let Some(entity) = unsafe { tree.pop_first() } {
            popped.push(entity.key);
            assert!(!entity.node.is_linked());
            assert_rb_invariants(&tree);
        }

        assert_eq!(popped, [1, 3, 8, 11]);
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
    }

    #[test]
    fn repeated_insert_remove_preserves_rb_invariants() {
        let mut tree = RBTree::<TestEntity>::new();
        let mut entities = [
            TestEntity::new(20),
            TestEntity::new(5),
            TestEntity::new(15),
            TestEntity::new(30),
            TestEntity::new(25),
            TestEntity::new(10),
            TestEntity::new(35),
            TestEntity::new(1),
            TestEntity::new(18),
        ];

        for entity in &mut entities {
            unsafe {
                tree.insert(entity);
            }
            assert_rb_invariants(&tree);
        }

        for index in [3, 1, 7, 0, 5] {
            unsafe {
                tree.remove(&mut entities[index]);
            }
            assert_rb_invariants(&tree);
        }

        assert_eq!(collect_keys(&tree), [15, 18, 25, 35]);
    }
}
