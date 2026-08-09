// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Generic intrusive FIFO list.
//!
//! The list stores links directly inside caller-owned entities and does not
//! allocate. Each entity type supplies accessors for its embedded `ListLink`.

use core::marker::PhantomData;
use core::ptr;

/// Intrusive singly-linked-list link embedded inside an owning entity.
#[repr(C)]
pub(crate) struct ListLink {
    next: *mut ListLink,
}

impl ListLink {
    pub const fn new() -> Self {
        Self {
            next: ptr::null_mut(),
        }
    }

    pub fn reset_links(&mut self) {
        self.next = ptr::null_mut();
    }
}

impl Default for ListLink {
    fn default() -> Self {
        Self::new()
    }
}

/// Entity contract for using `List`.
///
/// # Safety
///
/// Implementors must return the address of the embedded `ListLink` that
/// belongs to the provided entity, and must recover the original entity pointer
/// from a link pointer produced by that accessor.
pub(crate) unsafe trait ListNode: Sized {
    fn node(entity: *mut Self) -> *mut ListLink;
    fn entity_of(node: *mut ListLink) -> *mut Self;
    fn entity_of_const(node: *const ListLink) -> *const Self;
}

/// Intrusive FIFO list over caller-owned entity type `T`.
pub(crate) struct List<T: ListNode> {
    head: *mut ListLink,
    tail: *mut ListLink,
    len: usize,
    _entity: PhantomData<T>,
}

impl<T: ListNode> Default for List<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ListNode> List<T> {
    pub const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            len: 0,
            _entity: PhantomData,
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[allow(dead_code)]
    pub fn first(&self) -> *mut T {
        T::entity_of(self.head)
    }

    #[allow(dead_code)]
    pub fn next(&self, entity: *mut T) -> *mut T {
        if entity.is_null() {
            return ptr::null_mut();
        }

        unsafe { T::entity_of((*T::node(entity)).next) }
    }

    pub fn contains(&self, entity: *const T) -> bool {
        let mut cursor = self.head;

        while !cursor.is_null() {
            if ptr::eq(T::entity_of_const(cursor.cast_const()), entity) {
                return true;
            }

            unsafe {
                cursor = (*cursor).next;
            }
        }

        false
    }

    /// Append a detached entity to the back of the list.
    ///
    /// # Safety
    ///
    /// `entity` must be non-null, valid for mutation, and backed by storage
    /// that outlives its list membership. It must not be linked into this or
    /// any other list. The caller must hold exclusive access to this list for
    /// the duration of the insertion.
    pub unsafe fn push_back(&mut self, entity: *mut T) {
        debug_assert!(!entity.is_null());
        debug_assert!(
            !self.contains(entity.cast_const()),
            "entity is already linked into this list"
        );

        unsafe {
            let node = T::node(entity);
            (*node).reset_links();

            if self.tail.is_null() {
                self.head = node;
            } else {
                (*self.tail).next = node;
            }

            self.tail = node;
            self.len += 1;
        }
    }

    /// Remove and return the front entity in the list.
    ///
    /// # Safety
    ///
    /// Every entity linked into this list must remain valid for mutation, and
    /// the caller must hold exclusive access to the list for the duration of
    /// the removal and returned mutable borrow.
    pub unsafe fn pop_front(&mut self) -> Option<&mut T> {
        unsafe {
            if self.head.is_null() {
                return None;
            }

            let node = self.head;
            self.head = (*node).next;
            if self.head.is_null() {
                self.tail = ptr::null_mut();
            }

            (*node).reset_links();
            self.len -= 1;
            Some(&mut *T::entity_of(node))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    struct Entry {
        value: u32,
        link: ListLink,
    }

    impl Entry {
        const fn new(value: u32) -> Self {
            Self {
                value,
                link: ListLink::new(),
            }
        }
    }

    unsafe impl ListNode for Entry {
        fn node(entity: *mut Self) -> *mut ListLink {
            if entity.is_null() {
                ptr::null_mut()
            } else {
                unsafe { ptr::addr_of_mut!((*entity).link) }
            }
        }

        fn entity_of(node: *mut ListLink) -> *mut Self {
            if node.is_null() {
                ptr::null_mut()
            } else {
                unsafe {
                    (node as *mut u8)
                        .sub(offset_of!(Entry, link))
                        .cast::<Entry>()
                }
            }
        }

        fn entity_of_const(node: *const ListLink) -> *const Self {
            if node.is_null() {
                ptr::null()
            } else {
                unsafe {
                    (node as *const u8)
                        .sub(offset_of!(Entry, link))
                        .cast::<Entry>()
                }
            }
        }
    }

    #[test]
    fn list_pushes_and_pops_in_fifo_order() {
        let mut list = List::new();
        let mut first = Entry::new(1);
        let mut second = Entry::new(2);
        let mut third = Entry::new(3);

        unsafe {
            list.push_back(&mut first);
            list.push_back(&mut second);
            list.push_back(&mut third);
        }

        assert_eq!(list.len(), 3);
        assert!(list.contains(&first));
        assert!(list.contains(&second));
        assert!(list.contains(&third));

        unsafe {
            assert_eq!(list.pop_front().map(|entry| entry.value), Some(1));
            assert_eq!(list.pop_front().map(|entry| entry.value), Some(2));
            assert_eq!(list.pop_front().map(|entry| entry.value), Some(3));
            assert!(list.pop_front().is_none());
        }

        assert!(list.is_empty());
    }

    #[test]
    fn next_traverses_linked_entries() {
        let mut list = List::new();
        let mut first = Entry::new(1);
        let mut second = Entry::new(2);

        unsafe {
            list.push_back(&mut first);
            list.push_back(&mut second);
        }

        let first_ptr = list.first();
        let second_ptr = list.next(first_ptr);

        unsafe {
            assert_eq!((*first_ptr).value, 1);
            assert_eq!((*second_ptr).value, 2);
        }
        assert!(list.next(second_ptr).is_null());
    }
}
