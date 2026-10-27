//! Example workflows embedded in the binary, so a fresh workspace can try the app
//! without copying files by hand.

use std::path::Path;

use crate::{doc, storage};

pub struct Example {
    pub name: &'static str,
    pub markdown: &'static str,
    pub json: &'static str,
}

macro_rules! example {
    ($name:literal) => {
        Example {
            name: $name,
            markdown: include_str!(concat!("../examples/", $name, ".md")),
            json: include_str!(concat!("../examples/", $name, ".json")),
        }
    };
}

/// Every bundled example, children before the parents that call them.
pub const ALL: [Example; 4] = [
    example!("reviewed-note"),
    example!("call-reviewed-note"),
    example!("note-parts"),
    example!("use-note-parts"),
];

/// Copy the examples that are not already present into `graphs/`. Existing files are
/// never replaced. Returns the names installed and the names skipped.
pub fn install(root: &Path) -> Result<(Vec<String>, Vec<String>), String> {
    let dir = storage::directory(root, Path::new(doc::DIR), true)?;
    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    for example in &ALL {
        let markdown = dir.join(format!("{}.md", example.name));
        let json = dir.join(format!("{}.json", example.name));
        if markdown.exists() || json.exists() {
            skipped.push(example.name.to_string());
            continue;
        }
        serde_json::from_str::<doc::GraphDoc>(example.json)
            .map_err(|e| format!("bundled example {} is invalid: {e}", example.name))?;
        storage::write_atomic(&json, example.json.as_bytes())?;
        storage::write_atomic(&markdown, example.markdown.as_bytes())?;
        installed.push(example.name.to_string());
    }
    Ok((installed, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn examples_install_once_and_load() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("uapz-examples-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let (installed, skipped) = install(&root).unwrap();
        assert_eq!(installed.len(), ALL.len());
        assert!(skipped.is_empty());
        for example in &ALL {
            let loaded = doc::load_under(&root, example.name).unwrap();
            assert_eq!(loaded.name, example.name);
            assert!(
                root.join(doc::DIR)
                    .join(format!("{}.md", example.name))
                    .exists()
            );
        }
        let (installed, skipped) = install(&root).unwrap();
        assert!(installed.is_empty());
        assert_eq!(skipped.len(), ALL.len());
        fs::remove_dir_all(root).unwrap();
    }
}
