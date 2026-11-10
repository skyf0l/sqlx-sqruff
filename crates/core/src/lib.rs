//! sqlx-sqruff-core: locate and lint/format SQL inside `sqlx::query*!` macros
//! and `sqlx::query*(...)` calls using the embedded `sqruff` library.

pub mod diagnostic;
pub mod engine;
pub mod extract;
mod line_index;
pub mod literal;
pub mod sqruff_adapter;
