use crate::Result;
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

pub struct Checkout {
    pub root: PathBuf,
    pub database: PathBuf,
}

impl Checkout {
    pub fn find(directory: &Path) -> Result<Self> {
        let directory = dunce::canonicalize(directory)?;
        for root in directory.ancestors() {
            let marker = root.join(".git");
            let git_dir = if marker.is_dir() {
                dunce::canonicalize(marker)?
            } else if marker.is_file() {
                let pointer = fs::read_to_string(&marker)?;
                let target = pointer
                    .trim_end()
                    .strip_prefix("gitdir: ")
                    .ok_or("Invalid .git pointer")?;
                dunce::canonicalize(root.join(target))?
            } else {
                continue;
            };
            if !git_dir.join("HEAD").is_file() {
                return Err(format!("Missing Git metadata in {}", git_dir.display()).into());
            }
            return Ok(Self {
                root: root.to_owned(),
                // Use the existing database so a rename preserves active locks and lock IDs.
                database: git_dir.join("codex-checkout-lock.sqlite3"),
            });
        }
        Err(format!("No Git checkout at {}", directory.display()).into())
    }

    pub fn files(&self, files: &[String], from: &Path) -> Result<BTreeMap<String, String>> {
        if files.is_empty() || files.len() > crate::MAX_ITEMS {
            return Err(format!("Specify 1 to {} file paths", crate::MAX_ITEMS).into());
        }
        let mut paths = BTreeMap::new();
        for file in files {
            let path = Path::new(file);
            if file.is_empty()
                || file.contains('\0')
                || path.has_root() && !path.is_absolute()
                || matches!(path.components().next(), Some(Component::Prefix(_)))
                    && !path.is_absolute()
            {
                return Err(format!("Invalid file path: {file}").into());
            }
            let path = from.join(path);
            let resolved = resolve_file(&path)?;
            let relative = resolved
                .strip_prefix(&self.root)
                .map_err(|_| format!("File is outside {}: {file}", self.root.display()))?;
            let display = relative
                .iter()
                .map(|segment| segment.to_str().ok_or("Non-UTF-8 filename"))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if display.is_empty()
                || display.iter().any(|part| part.eq_ignore_ascii_case(".git"))
                || resolved.starts_with(self.database.parent().unwrap())
                || resolved.is_dir()
            {
                return Err(format!("Expected a file inside the checkout: {file}").into());
            }
            let display = display.join("/");
            let key = if cfg!(windows) {
                display.to_uppercase()
            } else {
                display.clone()
            };
            paths.entry(key).or_insert(display);
        }
        Ok(paths)
    }
}

fn resolve_file(path: &Path) -> Result<PathBuf> {
    let normalized = normalize(path)?;
    match fs::symlink_metadata(&normalized) {
        Ok(metadata) if metadata.is_symlink() => {
            return Err(
                format!("Use the target file instead of a link: {}", path.display()).into(),
            );
        }
        Ok(_) => return Ok(dunce::canonicalize(normalized)?),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // Resolve the existing ancestor so new files share a key across aliased parent paths.
    let mut ancestor = normalized.as_path();
    loop {
        let Some(parent) = ancestor.parent() else {
            return Err(format!("No existing ancestor for {}", path.display()).into());
        };
        ancestor = parent;
        match dunce::canonicalize(ancestor) {
            Ok(existing) => return Ok(existing.join(normalized.strip_prefix(ancestor)?)),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn normalize(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Resolve aliases before interpreting '..', matching POSIX filesystem traversal.
                if let Ok(canonical) = dunce::canonicalize(&result) {
                    result = canonical;
                }
                if !result.pop() {
                    return Err(format!("Path traverses above its root: {}", path.display()).into());
                }
            }
            Component::Normal(part) => {
                if cfg!(windows) {
                    let name = part.to_str().ok_or("Non-UTF-8 filename")?;
                    let stem = name
                        .split('.')
                        .next()
                        .unwrap_or_default()
                        .trim_end()
                        .to_ascii_uppercase();
                    let device = matches!(
                        stem.as_str(),
                        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
                    ) || ["COM", "LPT"].into_iter().any(|prefix| {
                        stem.strip_prefix(prefix).is_some_and(|suffix| {
                            matches!(
                                suffix,
                                "0" | "1"
                                    | "2"
                                    | "3"
                                    | "4"
                                    | "5"
                                    | "6"
                                    | "7"
                                    | "8"
                                    | "9"
                                    | "¹"
                                    | "²"
                                    | "³"
                            )
                        })
                    });
                    if device
                        || name.ends_with(['.', ' '])
                        || name.contains([':', '<', '>', '"', '|', '?', '*'])
                        || name.chars().any(char::is_control)
                    {
                        return Err(format!("Unsupported Windows file name: {name}").into());
                    }
                }
                result.push(part);
            }
            _ => result.push(component.as_os_str()),
        }
    }
    Ok(result)
}
