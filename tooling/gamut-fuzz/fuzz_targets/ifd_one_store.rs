//! fuzz · law — one tag, one store, searched rather than sampled.
//!
//! These are the *same* functions `gamut_ifd::invariants` exposes to the pinned-seed `proptest`
//! property in the per-PR gate (#601). Nothing is restated here: a law written twice is a law that
//! can disagree with itself, and both disagreeing copies would keep passing.
//!
//! Two laws, over two readings of the same bytes:
//!
//! - [`setters_keep_one_store_per_tag`] takes each byte as one setter call on a flat directory,
//!   over the property's own four-tag alphabet, so the two tiers search the same shape of input;
//! - [`one_store_per_tag`] takes the bytes as a program that builds a directory *tree* through the
//!   same public setters, nesting children inside children. The flat law only ever checks a root
//!   whose groups hold empty children; this reading is what reaches the law's descent into them.
//!
//! The input space is small, so this target mainly keeps `docs/testing.md`'s "one law, two
//! drivers" contract whole rather than expecting new defects. A crash found here is **minimised
//! and promoted into a named deterministic test** in `gamut-ifd`'s own suite. The corpus is a
//! search aid, not the regression record.

#![no_main]

use gamut_ifd::invariants::{Edit, one_store_per_tag, setters_keep_one_store_per_tag};
use gamut_ifd::{Ifd, Value};
use libfuzzer_sys::fuzz_target;

/// The first tag of the alphabet: `SubIFDs` (330), a real pointer tag.
///
/// The property draws its tags from `330..334`; mirroring that keeps the two tiers on the same
/// alphabet, and four tags are few enough that the same tag is revisited by a different setter
/// often enough to draw every store-to-store transition.
const FIRST_TAG: u16 = 330;

/// How many tags the alphabet holds.
const TAGS: u8 = 4;

/// The deepest a built tree may nest. Bounds the build stack and the law's recursion, so an input
/// of nothing but `Open` bytes cannot turn into a stack overflow the law did not cause.
const MAX_DEPTH: usize = 8;

/// The alphabet tag a byte's high bits select.
fn tag(byte: u8, shift: u32) -> u16 {
    FIRST_TAG + u16::from((byte >> shift) % TAGS)
}

/// Reads one byte as one setter call: the low two bits pick the setter, the next two the tag.
fn edit(byte: u8) -> Edit {
    let tag = tag(byte, 2);
    match byte & 3 {
        0 => Edit::Set(tag),
        1 => Edit::SetSubIfd(tag),
        2 => Edit::AddSubIfd(tag),
        _ => Edit::Remove(tag),
    }
}

/// Builds a directory tree from `data` through the public setters only.
///
/// The low three bits of each byte are an operation on the innermost open directory, the next two
/// its tag. `Open` starts a new child; a close attaches the innermost child to its parent under the
/// byte's tag, through `set_sub_ifd` or `add_sub_ifd`. Children still open at the end are closed by
/// `add_sub_ifd` under [`FIRST_TAG`], so no built work is discarded.
fn build_tree(data: &[u8]) -> Ifd {
    let mut stack = vec![Ifd::new()];
    for &byte in data {
        let tag = tag(byte, 3);
        match byte & 7 {
            0 => top(&mut stack).set(tag, Value::Long(vec![u32::from(tag)])),
            1 => top(&mut stack).set_sub_ifd(tag, vec![Ifd::new()]),
            2 => top(&mut stack).add_sub_ifd(tag, Ifd::new()),
            3 => {
                top(&mut stack).remove(tag);
            }
            4 if stack.len() <= MAX_DEPTH => stack.push(Ifd::new()),
            5 if stack.len() > 1 => {
                let child = pop(&mut stack);
                top(&mut stack).set_sub_ifd(tag, vec![child]);
            }
            6 | 7 if stack.len() > 1 => {
                let child = pop(&mut stack);
                top(&mut stack).add_sub_ifd(tag, child);
            }
            // An `Open` past the depth bound, or a close with nothing open, does nothing.
            _ => {}
        }
    }
    while stack.len() > 1 {
        let child = pop(&mut stack);
        top(&mut stack).add_sub_ifd(FIRST_TAG, child);
    }
    pop(&mut stack)
}

/// The innermost open directory. The stack never empties while building: the root is pushed first
/// and only popped once every child above it is closed.
fn top(stack: &mut [Ifd]) -> &mut Ifd {
    match stack.last_mut() {
        Some(ifd) => ifd,
        None => unreachable!("the root stays on the stack until the build ends"),
    }
}

/// Closes the innermost open directory.
fn pop(stack: &mut Vec<Ifd>) -> Ifd {
    stack.pop().unwrap_or_default()
}

fuzz_target!(|data: &[u8]| {
    // Law 1: after any sequence of setter calls, each tag sits in the store its last setter named.
    let edits: Vec<Edit> = data.iter().copied().map(edit).collect();
    if let Err(violation) = setters_keep_one_store_per_tag(&edits) {
        panic!("{violation}");
    }

    // Law 2: no directory anywhere in a setter-built tree holds a tag in both stores.
    if let Err(violation) = one_store_per_tag(&build_tree(data)) {
        panic!("{violation}");
    }
});
