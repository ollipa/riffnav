//! File-tree icons. `Nerd` uses Nerd Font filetype glyphs (the default, per the
//! project's font requirement); `Unicode` and `Ascii` are progressively safer
//! fallbacks cycled with the `i` key.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconStyle {
    Nerd,
    Unicode,
    Ascii,
}

impl IconStyle {
    pub fn next(self) -> Self {
        match self {
            IconStyle::Nerd => IconStyle::Unicode,
            IconStyle::Unicode => IconStyle::Ascii,
            IconStyle::Ascii => IconStyle::Nerd,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IconStyle::Nerd => "nerd",
            IconStyle::Unicode => "unicode",
            IconStyle::Ascii => "ascii",
        }
    }
}

/// Open/closed folder marker for the given style.
pub fn dir_icon(expanded: bool, style: IconStyle) -> &'static str {
    match style {
        IconStyle::Nerd => {
            if expanded {
                "\u{f07c}" //
            } else {
                "\u{f07b}" //
            }
        }
        IconStyle::Unicode => {
            if expanded {
                "▾"
            } else {
                "▸"
            }
        }
        IconStyle::Ascii => {
            if expanded {
                "v"
            } else {
                ">"
            }
        }
    }
}

/// Filetype glyph for a path. Empty string for non-Nerd styles (the status
/// sigil already identifies files there).
pub fn file_icon(path: &str, style: IconStyle) -> &'static str {
    if style != IconStyle::Nerd {
        return "";
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "Cargo.toml" | "Cargo.lock" => return "\u{e7a8}", // rust
        "Makefile" | "makefile" => return "\u{e673}",
        "Dockerfile" => return "\u{f308}",
        ".gitignore" | ".gitattributes" | ".gitmodules" => return "\u{e702}",
        "LICENSE" => return "\u{f0219}",
        _ => {}
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "rs" => "\u{e7a8}",                                   //
        "go" => "\u{e627}",                                   //
        "py" => "\u{e606}",                                   //
        "js" | "mjs" | "cjs" => "\u{e74e}",                   //
        "ts" => "\u{e628}",                                   //
        "jsx" | "tsx" => "\u{e7ba}",                          //
        "json" => "\u{e60b}",                                 //
        "toml" => "\u{e6b2}",                                 //
        "yaml" | "yml" => "\u{e615}",                         //
        "md" | "markdown" => "\u{e73e}",                      //
        "sh" | "bash" | "zsh" | "fish" => "\u{e795}",         //
        "c" => "\u{e61e}",                                    //
        "h" | "hpp" => "\u{f0fd}",                            //
        "cpp" | "cc" | "cxx" => "\u{e61d}",                   //
        "html" | "htm" => "\u{e736}",                         //
        "css" => "\u{e749}",                                  //
        "scss" | "sass" => "\u{e603}",                        //
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => "\u{f1c5}", //
        "lock" => "\u{f023}",                                 //
        "txt" | "text" => "\u{f0f6}",                         //
        _ => "\u{f15b}",                                      //  default file
    }
}
