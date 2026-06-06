//! sqlx-sqruff-core: locate and lint/format SQL inside `sqlx::query*!` macros
//! using the embedded `sqruff` library.

pub mod diagnostic;
pub mod engine;
pub mod extract;
pub mod literal;
pub mod sqruff_adapter;
