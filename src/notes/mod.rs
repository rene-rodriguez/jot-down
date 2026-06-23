//! Pure, storage-agnostic note-content helpers.
//!
//! These operate on raw note bodies (plain `&str` in, owned `String`/`Vec` out)
//! so they can be unit-tested headlessly and reused by the storage layer, the
//! editor, and the preview without dragging in any I/O.

pub mod daily;
pub mod wikilinks;
