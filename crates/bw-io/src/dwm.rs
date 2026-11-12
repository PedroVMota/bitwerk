//! Legacy `.dwm` import (Delphi object-graph streams, see the format notes in
//! the project documentation).
//!
//! Not implemented yet. When it is, it must follow the same rules as the
//! native loader: bounded sizes and counts, bounded recursion (macro depth ≤
//! [`crate::MAX_MACRO_DEPTH`]), no `unsafe`, and full validation through
//! [`bw_core::Circuit`] before anything reaches the application.

use crate::{IoError, Project};

/// Parses a legacy `.dwm` file. Currently always fails.
///
/// # Errors
/// Always [`IoError::LegacyNotSupported`] for now.
pub fn load(_bytes: &[u8]) -> Result<Project, IoError> {
    Err(IoError::LegacyNotSupported)
}
