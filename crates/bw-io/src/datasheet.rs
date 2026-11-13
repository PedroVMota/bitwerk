//! Safe resolution of a macro's datasheet link (a local file).
//!
//! A macro may carry a link to a local PDF / HTML / text file. The app
//! opens it **only** after an explicit click, and only what
//! [`resolve_link`] accepts:
//!
//! * a plain file path (absolute, or relative to the project's folder);
//!   anything that looks like a URL or URI (`scheme:`) is refused, so no
//!   web page, `file:` or custom-scheme handler is ever launched;
//! * the path is canonicalized (symlinks and `..` resolved) and must name an
//!   existing **regular file** (application bundles are directories and are
//!   refused);
//! * the extension of the canonical path must be in [`ALLOWED_EXTENSIONS`]
//!   (`pdf`, `html`, `htm`, `txt`, `md`); executables, scripts and
//!   everything else are refused;
//! * files with an executable permission bit are refused as well.
//!
//! The app then runs `/usr/bin/open <canonical path>` directly (no shell).

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Extensions a datasheet link may have (case-insensitive).
pub const ALLOWED_EXTENSIONS: [&str; 5] = ["pdf", "html", "htm", "txt", "md"];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LinkError {
    #[error("datasheet links must be local file paths, not URLs")]
    NotAPath,
    #[error("the datasheet file does not exist or cannot be read")]
    Missing,
    #[error("the datasheet link is not a regular file")]
    NotAFile,
    #[error("datasheet files must be .pdf, .html, .htm, .txt or .md")]
    Extension,
    #[error("the datasheet file is executable")]
    Executable,
}

/// Does `link` look like a URL / URI (`scheme:` with a scheme of two or
/// more letters, so `C:` style drive letters do not count)?
fn looks_like_url(link: &str) -> bool {
    if link.contains("://") {
        return true;
    }
    match link.split_once(':') {
        Some((scheme, _)) => {
            scheme.len() >= 2
                && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        }
        None => false,
    }
}

/// Resolves a datasheet link to a canonical path that is safe to hand to
/// the system opener (see the module docs). `base` is the folder relative
/// links are resolved against (the project's folder).
///
/// # Errors
/// See [`LinkError`].
pub fn resolve_link(link: &str, base: Option<&Path>) -> Result<PathBuf, LinkError> {
    let link = link.trim();
    if link.is_empty() || link.contains('\0') || looks_like_url(link) {
        return Err(LinkError::NotAPath);
    }
    let p = Path::new(link);
    let p = if p.is_absolute() { p.to_path_buf() } else { base.ok_or(LinkError::Missing)?.join(p) };
    let canon = p.canonicalize().map_err(|_| LinkError::Missing)?;
    let meta = std::fs::metadata(&canon).map_err(|_| LinkError::Missing)?;
    if !meta.is_file() {
        return Err(LinkError::NotAFile);
    }
    let ext = canon.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    if !ext.is_some_and(|e| ALLOWED_EXTENSIONS.contains(&e.as_str())) {
        return Err(LinkError::Extension);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return Err(LinkError::Executable);
        }
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_unsafe_files_are_refused() {
        for l in [
            "https://example.com/x.pdf",
            "http:x.pdf",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:a@b.c",
            "x-custom:thing",
            "",
        ] {
            assert_eq!(resolve_link(l, None), Err(LinkError::NotAPath), "{l}");
        }
        let dir = std::env::temp_dir().join(format!("bw-ds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("sheet.PDF");
        std::fs::write(&ok, b"%PDF").unwrap();
        let sh = dir.join("evil.sh");
        std::fs::write(&sh, b"echo").unwrap();
        let app = dir.join("Thing.app");
        std::fs::create_dir_all(&app).unwrap();
        let fake = dir.join("fake.pdf");
        std::fs::write(&fake, b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(resolve_link(ok.to_str().unwrap(), None), Ok(ok.canonicalize().unwrap()));
        assert_eq!(resolve_link("sheet.PDF", Some(&dir)), Ok(ok.canonicalize().unwrap()));
        assert_eq!(resolve_link("../nope/missing.pdf", Some(&dir)), Err(LinkError::Missing));
        assert_eq!(resolve_link(sh.to_str().unwrap(), None), Err(LinkError::Extension));
        assert_eq!(resolve_link(app.to_str().unwrap(), None), Err(LinkError::NotAFile));
        #[cfg(unix)]
        assert_eq!(resolve_link(fake.to_str().unwrap(), None), Err(LinkError::Executable));
        // A symlink named .pdf pointing at a script resolves to the script.
        #[cfg(unix)]
        {
            let link = dir.join("innocent.pdf");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&sh, &link).unwrap();
            assert_eq!(resolve_link(link.to_str().unwrap(), None), Err(LinkError::Extension));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
