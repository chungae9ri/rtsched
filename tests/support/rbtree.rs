extern crate std;

use core::cmp::Ordering;
use core::mem::offset_of;
use core::ptr;

use std::vec::Vec;

use super::{Color, RBTree, RBTreeNode, RbNode};

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

pub fn insert_keeps_entities_in_key_order() {
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

pub fn contains_reports_tree_membership_including_root() {
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

pub fn inserting_same_entity_twice_panics_in_debug_builds() {
    let mut tree = RBTree::<TestEntity>::new();
    let mut entity = TestEntity::new(10);

    unsafe {
        tree.insert(&mut entity);
        tree.insert(&mut entity);
    }
}

pub fn equal_keys_are_ordered_by_entity_address() {
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

pub fn remove_detaches_entity_and_preserves_order() {
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

    assert!(ptr::eq(removed, &entities[1]));
    assert!(!entities[1].node.is_linked());
    assert_eq!(tree.len(), 6);
    assert_eq!(collect_keys(&tree), [2, 6, 10, 12, 14, 16]);
    assert_rb_invariants(&tree);
}

pub fn pop_first_removes_entities_in_order() {
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

pub fn repeated_insert_remove_preserves_rb_invariants() {
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
