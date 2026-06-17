//! Decision #17 (Phase 1.5): `Record<Snapshot>` is camera-shutter immutable.
//! It can be fossilized via `fix(hash)` but not updated.

use objectrecords_core::{Body, Kind, Mutable, Record};

fn main() {
    let mutable = Record::<Mutable>::new(Kind::Log, Body::Inline(vec![1]));
    let mut snap = mutable.snapshot();

    // The line below MUST NOT compile.
    snap.update(Body::Inline(vec![2]));
}
