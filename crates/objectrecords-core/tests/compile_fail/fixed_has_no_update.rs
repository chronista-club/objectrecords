//! Decision #5: `Record<Fixed>::update` does not exist — calling it must be
//! a compile error, not a runtime panic. This is the core type-state
//! guarantee of the immutable layer.

use objectrecords_core::{Body, Fixed, Kind, Mutable, Record, Sha256Hash};

fn main() {
    let mutable = Record::<Mutable>::new(Kind::Log, Body::Inline(vec![1]));
    let mut fixed: Record<Fixed> = mutable.fix(Sha256Hash([0u8; 32]));

    // The line below MUST NOT compile.
    fixed.update(Body::Inline(vec![2]));
}
