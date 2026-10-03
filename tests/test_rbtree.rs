use rtsched::test_support::rbtree;

#[test]
fn insert_keeps_entities_in_key_order() {
    rbtree::insert_keeps_entities_in_key_order();
}

#[test]
fn contains_reports_tree_membership_including_root() {
    rbtree::contains_reports_tree_membership_including_root();
}

#[test]
#[should_panic(expected = "entity is already linked into this tree")]
fn inserting_same_entity_twice_panics_in_debug_builds() {
    rbtree::inserting_same_entity_twice_panics_in_debug_builds();
}

#[test]
fn equal_keys_are_ordered_by_entity_address() {
    rbtree::equal_keys_are_ordered_by_entity_address();
}

#[test]
fn remove_detaches_entity_and_preserves_order() {
    rbtree::remove_detaches_entity_and_preserves_order();
}

#[test]
fn pop_first_removes_entities_in_order() {
    rbtree::pop_first_removes_entities_in_order();
}

#[test]
fn repeated_insert_remove_preserves_rb_invariants() {
    rbtree::repeated_insert_remove_preserves_rb_invariants();
}
