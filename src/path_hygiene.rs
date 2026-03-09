#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    fn scan_dir(root: &Path, hits: &mut Vec<String>) {
        let entries = fs::read_dir(root).unwrap_or_else(|err| {
            panic!("failed to read {}: {err}", root.display());
        });
        for entry in entries {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            if path.is_dir() {
                scan_dir(&path, hits);
                continue;
            }
            scan_file(&path, hits);
        }
    }

    fn scan_file(path: &Path, hits: &mut Vec<String>) {
        let Ok(contents) = fs::read_to_string(path) else {
            return;
        };
        let prefix = format!("/{}/", "Users");
        let banned = [
            format!("{prefix}{}", "nikiv"),
            format!("{prefix}{}", "nikitavoloboev"),
        ];
        for (line_no, line) in contents.lines().enumerate() {
            if banned.iter().any(|needle| line.contains(needle)) {
                hits.push(format!("{}:{}", path.display(), line_no + 1));
            }
        }
    }

    #[test]
    fn repo_avoids_committed_absolute_user_home_paths() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut hits = Vec::new();

        for rel in [
            "src",
            "docs",
            ".ai/skills",
            "flow.toml",
            "readme.md",
            "install.sh",
        ] {
            let path = root.join(rel);
            if path.is_dir() {
                scan_dir(&path, &mut hits);
            } else {
                scan_file(&path, &mut hits);
            }
        }

        assert!(
            hits.is_empty(),
            "use ~/ instead of absolute home paths in committed files:\n{}",
            hits.join("\n")
        );
    }
}
