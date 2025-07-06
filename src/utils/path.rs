use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};
use fancy_regex::Regex;
use indexmap::IndexSet;
use path_absolutize::Absolutize;

type ParseGlobResult = (String, Option<Vec<String>>, bool, Option<usize>);

pub fn safe_join_path<T1: AsRef<Path>, T2: AsRef<Path>>(
    base_path: T1,
    sub_path: T2,
) -> Option<PathBuf> {
    let base_path = base_path.as_ref();
    let sub_path = sub_path.as_ref();
    if sub_path.is_absolute() {
        return None;
    }

    let mut joined_path = PathBuf::from(base_path);

    for component in sub_path.components() {
        if Component::ParentDir == component {
            return None;
        }
        joined_path.push(component);
    }

    if joined_path.starts_with(base_path) {
        Some(joined_path)
    } else {
        None
    }
}

pub async fn expand_glob_paths<T: AsRef<str>>(
    paths: &[T],
    bail_non_exist: bool,
) -> Result<IndexSet<String>> {
    let mut new_paths = IndexSet::new();
    for path in paths {
        let (path_str, suffixes, current_only, depth) = parse_glob(path.as_ref())?;
        list_files(
            &mut new_paths,
            Path::new(&path_str),
            suffixes.as_ref(),
            current_only,
            bail_non_exist,
            depth,
        )
        .await?;
    }
    Ok(new_paths)
}

pub fn clear_dir(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

pub fn list_file_names<T: AsRef<Path>>(dir: T, ext: &str) -> Vec<String> {
    match fs::read_dir(dir.as_ref()) {
        Ok(rd) => {
            let mut names = vec![];
            for entry in rd.flatten() {
                let name = entry.file_name();
                if let Some(name) = name.to_string_lossy().strip_suffix(ext) {
                    names.push(name.to_string());
                }
            }
            names.sort_unstable();
            names
        }
        Err(_) => vec![],
    }
}

pub fn get_patch_extension(path: &str) -> Option<String> {
    Path::new(&path)
        .extension()
        .map(|v| v.to_string_lossy().to_lowercase())
}

pub fn to_absolute_path(path: &str) -> Result<String> {
    Ok(Path::new(&path).absolutize()?.display().to_string())
}

pub fn resolve_home_dir(path: &str) -> String {
    let mut path = path.to_string();
    if path.starts_with("~/") || path.starts_with("~\\") {
        if let Some(home_dir) = dirs::home_dir() {
            path.replace_range(..1, &home_dir.display().to_string());
        }
    }
    path
}

fn parse_glob(path_str: &str) -> Result<ParseGlobResult> {
    let globbed_single_subdir_regex = Regex::new(r"\*/[^/]+\.[^/]+$").expect("invalid regex");
    let globbed_recursive_subdir_regex = Regex::new(r"\*\*/[^/]+\.[^/]+$").expect("invalid regex");
    let glob_result =
        if let Some(start) = path_str.find("/**/*.").or_else(|| path_str.find(r"\**\*.")) {
            Some((start, 6, false, None))
        } else if let Some(start) = path_str.find("**/*.").or_else(|| path_str.find(r"**\*.")) {
            if start == 0 {
                Some((start, 5, false, None))
            } else {
                None
            }
        } else if let Some(m) = globbed_recursive_subdir_regex.find(path_str)? {
            Some((m.start(), 3, false, None))
        } else if let Some(m) = globbed_single_subdir_regex.find(path_str)? {
            Some((m.start(), 2, false, Some(1usize)))
        } else if let Some(start) = path_str.find("/*.").or_else(|| path_str.find(r"\*.")) {
            Some((start, 3, true, None))
        } else if let Some(start) = path_str.find("*.") {
            if start == 0 {
                Some((start, 2, true, None))
            } else {
                None
            }
        } else {
            None
        };
    if let Some((start, offset, current_only, depth)) = glob_result {
        let mut base_path = path_str[..start].to_string();
        if base_path.is_empty() {
            base_path = if path_str
                .chars()
                .next()
                .map(|v| v == '/')
                .unwrap_or_default()
            {
                "/"
            } else {
                "."
            }
            .into();
        }

        let extensions = if let Some(curly_brace_end) = path_str[start..].find('}') {
            let end = start + curly_brace_end;
            let extensions_str = &path_str[start + offset..end + 1];
            if extensions_str.starts_with('{') && extensions_str.ends_with('}') {
                extensions_str[1..extensions_str.len() - 1]
                    .split(',')
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>()
            } else {
                bail!("Invalid path '{path_str}'");
            }
        } else {
            let extensions_str = &path_str[start + offset..];
            vec![extensions_str.to_string()]
        };
        let extensions = if extensions.is_empty() {
            None
        } else {
            Some(extensions)
        };
        Ok((base_path, extensions, current_only, depth))
    } else if path_str.ends_with("/**") || path_str.ends_with(r"\**") {
        Ok((
            path_str[0..path_str.len() - 3].to_string(),
            None,
            false,
            None,
        ))
    } else {
        Ok((path_str.to_string(), None, false, None))
    }
}

#[async_recursion::async_recursion]
async fn list_files(
    files: &mut IndexSet<String>,
    entry_path: &Path,
    suffixes: Option<&Vec<String>>,
    current_only: bool,
    bail_non_exist: bool,
    depth: Option<usize>,
) -> Result<()> {
    if !entry_path.exists() {
        if bail_non_exist {
            bail!("Not found '{}'", entry_path.display());
        } else {
            return Ok(());
        }
    }
    if entry_path.is_dir() {
        let mut reader = tokio::fs::read_dir(entry_path).await?;
        while let Some(entry) = reader.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                if !current_only {
                    if let Some(remaining_depth) = depth {
                        if remaining_depth > 0 {
                            list_files(
                                files,
                                &path,
                                suffixes,
                                current_only,
                                bail_non_exist,
                                Some(remaining_depth - 1),
                            )
                            .await?;
                        }
                    } else {
                        list_files(files, &path, suffixes, current_only, bail_non_exist, None)
                            .await?;
                    }
                }
            } else {
                add_file(files, suffixes, &path);
            }
        }
    } else {
        add_file(files, suffixes, entry_path);
    }
    Ok(())
}

fn add_file(files: &mut IndexSet<String>, suffixes: Option<&Vec<String>>, path: &Path) {
    if is_valid_extension(suffixes, path) {
        let path = path.display().to_string();
        if !files.contains(&path) {
            files.insert(path);
        }
    }
}

fn is_valid_extension(suffixes: Option<&Vec<String>>, path: &Path) -> bool {
    let filename_regex = Regex::new(r"^.+\.*").unwrap();
    if let Some(suffixes) = suffixes {
        if !suffixes.is_empty() {
            if let Ok(Some(_)) = filename_regex.find(&suffixes.join(",")) {
                let file_name = path
                    .file_name()
                    .and_then(|v| v.to_str())
                    .expect("invalid filename")
                    .to_string();
                return suffixes.contains(&file_name);
            } else if let Some(extension) =
                path.extension().map(|v| v.to_string_lossy().to_string())
            {
                return suffixes.contains(&extension);
            }
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_glob() {
        assert_eq!(
            parse_glob("dir").unwrap(),
            ("dir".into(), None, false, None)
        );
        assert_eq!(
            parse_glob("dir/**").unwrap(),
            ("dir".into(), None, false, None)
        );
        assert_eq!(
            parse_glob("dir/file.md").unwrap(),
            ("dir/file.md".into(), None, false, None)
        );
        assert_eq!(
            parse_glob("**/*.md").unwrap(),
            (".".into(), Some(vec!["md".into()]), false, None)
        );
        assert_eq!(
            parse_glob("/**/*.md").unwrap(),
            ("/".into(), Some(vec!["md".into()]), false, None)
        );
        assert_eq!(
            parse_glob("dir/**/*.md").unwrap(),
            ("dir".into(), Some(vec!["md".into()]), false, None)
        );
        assert_eq!(
            parse_glob("dir/**/test.md").unwrap(),
            ("dir/".into(), Some(vec!["test.md".into()]), false, None)
        );
        assert_eq!(
            parse_glob("dir/*/test.md").unwrap(),
            (
                "dir/".into(),
                Some(vec!["test.md".into()]),
                false,
                Some(1usize)
            )
        );
        assert_eq!(
            parse_glob("dir/**/*.{md,txt}").unwrap(),
            (
                "dir".into(),
                Some(vec!["md".into(), "txt".into()]),
                false,
                None
            )
        );
        assert_eq!(
            parse_glob("C:\\dir\\**\\*.{md,txt}").unwrap(),
            (
                "C:\\dir".into(),
                Some(vec!["md".into(), "txt".into()]),
                false,
                None
            )
        );
        assert_eq!(
            parse_glob("*.md").unwrap(),
            (".".into(), Some(vec!["md".into()]), true, None)
        );
        assert_eq!(
            parse_glob("/*.md").unwrap(),
            ("/".into(), Some(vec!["md".into()]), true, None)
        );
        assert_eq!(
            parse_glob("dir/*.md").unwrap(),
            ("dir".into(), Some(vec!["md".into()]), true, None)
        );
        assert_eq!(
            parse_glob("dir/*.{md,txt}").unwrap(),
            (
                "dir".into(),
                Some(vec!["md".into(), "txt".into()]),
                true,
                None
            )
        );
        assert_eq!(
            parse_glob("C:\\dir\\*.{md,txt}").unwrap(),
            (
                "C:\\dir".into(),
                Some(vec!["md".into(), "txt".into()]),
                true,
                None
            )
        );
    }
}
