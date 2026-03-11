//! LSP protocol feature implementations.
//!
//! This module provides implementations for LSP features:
//! - Diagnostics conversion from parser/validation errors
//! - Hover information for CEL expressions
//! - Semantic tokens for syntax highlighting

mod completion;
mod diagnostics;
mod hover;
mod semantic_tokens;

pub use completion::completion_at_position;
#[cfg(not(target_arch = "wasm32"))]
pub use completion::completion_at_position_proto;
#[cfg(not(target_arch = "wasm32"))]
pub use diagnostics::proto_to_diagnostics;
pub use diagnostics::to_diagnostics;
pub use hover::hover_at_position;
#[cfg(not(target_arch = "wasm32"))]
pub use hover::hover_at_position_proto;
#[cfg(not(target_arch = "wasm32"))]
pub use semantic_tokens::tokens_for_proto;
pub use semantic_tokens::{legend, tokens_for_ast};
