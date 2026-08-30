use {
    anyhow::{Context, Result},
    std::path::{Path, PathBuf},
};

/// Whether `s` is non-empty and made only of `specific_char`.
#[must_use]
pub fn is_same_character(s: &str, specific_char: char) -> bool {
    !s.is_empty() && s.chars().all(|c| c == specific_char)
}

/// Turn a directory path into an absolute path with a trailing slash.
///
/// Relative paths are resolved against the current working directory. The trailing slash is what
/// the database has always stored for `source` and `destination`, so we keep the format.
///
/// # Errors
///
/// Fails if the current directory can't be read or the path is not valid UTF-8.
pub fn normalize_dir(dir: &str) -> Result<String> {
    let path = if Path::new(dir).is_absolute() {
        PathBuf::from(dir)
    } else {
        std::env::current_dir()
            .context("failed to get the current working directory")?
            .join(dir)
    };
    let mut normalized = path
        .to_str()
        .with_context(|| format!("the path {} is not valid UTF-8", path.display()))?
        .to_string();
    if !normalized.ends_with('/') {
        normalized.push('/');
    }

    Ok(normalized)
}

/// Remove trailing slashes from a path, keeping `/` itself intact.
#[must_use]
pub fn strip_trailing_slash(path: &str) -> &str {
    if path.is_empty() {
        return path;
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed }
}

/// Name of this machine, used in the snapshots metadata.
///
/// # Errors
///
/// Fails if the hostname can't be read.
pub fn machine_name() -> Result<String> {
    Ok(hostname::get()
        .context("failed to get the hostname")?
        .to_string_lossy()
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::{is_same_character, normalize_dir, strip_trailing_slash};

    #[test]
    fn same_character() {
        assert!(is_same_character("/", '/'));
        assert!(is_same_character("///", '/'));
        assert!(!is_same_character("", '/'));
        assert!(!is_same_character("/a", '/'));
    }

    #[test]
    fn normalize_absolute_paths() {
        assert_eq!(normalize_dir("/home").unwrap(), "/home/");
        assert_eq!(normalize_dir("/home/").unwrap(), "/home/");
        assert_eq!(normalize_dir("/").unwrap(), "/");
    }

    #[test]
    fn normalize_relative_paths_against_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let expected = format!("{}/snaps/", cwd.display());
        assert_eq!(normalize_dir("snaps").unwrap(), expected);
        assert_eq!(normalize_dir("snaps/").unwrap(), expected);
    }

    #[test]
    fn strip_slash() {
        assert_eq!(strip_trailing_slash("/home/"), "/home");
        assert_eq!(strip_trailing_slash("/home"), "/home");
        assert_eq!(strip_trailing_slash("/"), "/");
        assert_eq!(strip_trailing_slash("///"), "/");
        assert_eq!(strip_trailing_slash(""), "");
    }
}
