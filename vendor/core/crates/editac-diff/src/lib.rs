//! On-demand immutable review inputs. CRDT updates are transport, never source.
//! No live replica, serializer, persistence or typing observer is owned here.
pub mod portable;
pub mod representation;
pub mod semantic;
pub mod text;
