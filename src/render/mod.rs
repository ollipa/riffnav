//! riffnav's own diff renderer: git's hunks in, styled ratatui text out.
//!
//! Each row is built knowing its line numbers, so the [`LineMap`] comments anchor
//! to comes straight from the rows rather than being read back out of output.

mod highlight;
mod layout;
mod words;

use ratatui::text::Text;

use crate::comment::LineMap;
use crate::diff::{self, FileDiff};
use crate::theme::DiffTheme;

/// Render one file as a unified diff, `width` columns wide.
pub fn unified(
    file: &FileDiff,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
) -> (Text<'static>, LineMap) {
    let rows = diff::rows(file);
    let syntax = highlight::highlight(file, &rows, theme);
    let changes = words::changes(&rows);
    layout::unified(
        file,
        &rows,
        &syntax,
        &changes.emphasis,
        width,
        theme,
        tab_width,
    )
}

/// Render one file side by side, `width` columns wide.
pub fn side_by_side(
    file: &FileDiff,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
) -> (Text<'static>, LineMap) {
    let rows = diff::rows(file);
    let syntax = highlight::highlight(file, &rows, theme);
    let changes = words::changes(&rows);
    layout::side_by_side(file, &rows, &syntax, &changes, width, theme, tab_width)
}
