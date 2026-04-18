use std::path::{Path, PathBuf};

use anyhow::Result;

/// A class or file that contains a `main` entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainClass {
    /// Fully qualified class name (e.g. `com.example.Main`).
    pub fully_qualified_name: String,
    /// Absolute path to the source file.
    pub source_file: PathBuf,
}

/// Scans `source_roots` for source files that contain a `main` entry point.
/// Supports `.java`, `.groovy`, and `.kt` files.
pub fn find_main_classes(source_roots: &[PathBuf]) -> Result<Vec<MainClass>> {
    let mut results = Vec::new();
    for root in source_roots {
        collect_main_classes(root, root, &mut results)?;
    }
    Ok(results)
}

fn collect_main_classes(root: &Path, dir: &Path, out: &mut Vec<MainClass>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_main_classes(root, &path, out)?;
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let detected = match ext {
                "java" => detect_java(&path)?,
                "groovy" => detect_groovy(&path)?,
                "kt" => detect_kotlin(&path)?,
                _ => false,
            };
            if detected {
                if let Some(fqn) = derive_fqn(root, &path) {
                    out.push(MainClass { fully_qualified_name: fqn, source_file: path });
                }
            }
        }
    }
    Ok(())
}

/// Detects `public static void main(String[] args)` or varargs form.
fn detect_java(path: &Path) -> Result<bool> {
    let src = std::fs::read_to_string(path)?;
    Ok(has_java_main(&src))
}

pub(crate) fn has_java_main(src: &str) -> bool {
    // Look for the canonical main signature in any order of modifiers.
    // Accepts: public static void main(String[] or public static void main(String...
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.contains("void") && trimmed.contains("main") {
            let has_public = trimmed.contains("public");
            let has_static = trimmed.contains("static");
            let has_string_param = trimmed.contains("String[]") || trimmed.contains("String...");
            if has_public && has_static && has_string_param {
                return true;
            }
        }
    }
    false
}

/// Detects `static void main(String[])`, `def main(String[])`, or `static main(String[])`.
/// Static is not required in Groovy.
fn detect_groovy(path: &Path) -> Result<bool> {
    let src = std::fs::read_to_string(path)?;
    Ok(has_groovy_main(&src))
}

pub(crate) fn has_groovy_main(src: &str) -> bool {
    for line in src.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("main") {
            continue;
        }
        // Matches: `static void main(`, `def main(`, `void main(`, `static main(`
        let has_void_or_def = trimmed.contains("void") || trimmed.contains("def");
        let has_string_param = trimmed.contains("String[]") || trimmed.contains("String...");
        if has_void_or_def && has_string_param && trimmed.contains("main(") {
            return true;
        }
        // Groovy script: bare `static main(String[] args)` without def/void
        if trimmed.contains("static") && trimmed.contains("main(") && has_string_param {
            return true;
        }
    }
    false
}

/// Detects top-level `fun main()` or `fun main(args: Array<String>)`.
/// Top-level means the line is not deeply indented (at most one level inside an object/companion).
fn detect_kotlin(path: &Path) -> Result<bool> {
    let src = std::fs::read_to_string(path)?;
    Ok(has_kotlin_main(&src))
}

pub(crate) fn has_kotlin_main(src: &str) -> bool {
    for line in src.lines() {
        let trimmed = line.trim();
        // Match `fun main(` with no leading indentation beyond one object block.
        // Reject if indented more than one block (method inside a class).
        let indent = line.len() - line.trim_start().len();
        if indent > 4 {
            continue;
        }
        if trimmed.starts_with("fun main(") || trimmed == "fun main()" {
            return true;
        }
    }
    false
}

/// Derives the fully qualified class name from `file` relative to `source_root`.
///
/// For Java/Groovy the FQN mirrors the directory structure (package path + file stem).
/// For Kotlin the same convention is used since the file stem is the class name.
fn derive_fqn(source_root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(source_root).ok()?;
    // Drop the extension, join components with `.`
    let without_ext = rel.with_extension("");
    let parts: Vec<&str> = without_ext
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    // --- Java ---

    #[test]
    fn test_java_single_main() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "com/example/App.java",
            r#"package com.example;
public class App {
    public static void main(String[] args) {
        System.out.println("hello");
    }
}"#,
        );
        let roots = vec![tmp.path().to_path_buf()];
        let found = find_main_classes(&roots).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fully_qualified_name, "com.example.App");
    }

    #[test]
    fn test_java_varargs_main() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "Main.java",
            "public class Main { public static void main(String... args) {} }",
        );
        let found = find_main_classes(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fully_qualified_name, "Main");
    }

    #[test]
    fn test_java_no_main() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "Util.java", "public class Util { public void helper() {} }");
        let found = find_main_classes(&[tmp.path().to_path_buf()]).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn test_java_multiple_main_classes() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "AppA.java",
            "public class AppA { public static void main(String[] args) {} }",
        );
        write_file(
            tmp.path(),
            "AppB.java",
            "public class AppB { public static void main(String[] args) {} }",
        );
        let mut found = find_main_classes(&[tmp.path().to_path_buf()]).unwrap();
        found.sort_by(|a, b| a.fully_qualified_name.cmp(&b.fully_qualified_name));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].fully_qualified_name, "AppA");
        assert_eq!(found[1].fully_qualified_name, "AppB");
    }

    // --- Groovy ---

    #[test]
    fn test_groovy_script_main() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "Script.groovy",
            r#"static void main(String[] args) {
    println "hello"
}"#,
        );
        let found = find_main_classes(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fully_qualified_name, "Script");
    }

    #[test]
    fn test_groovy_class_main() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "Runner.groovy",
            r#"class Runner {
    static void main(String[] args) {
        println "hello"
    }
}"#,
        );
        let found = find_main_classes(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fully_qualified_name, "Runner");
    }

    // --- Kotlin ---

    #[test]
    fn test_kotlin_top_level_main() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "main.kt",
            r#"fun main() {
    println("hello")
}"#,
        );
        let found = find_main_classes(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fully_qualified_name, "main");
    }

    #[test]
    fn test_kotlin_main_with_args() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "App.kt",
            r#"fun main(args: Array<String>) {
    println("hello")
}"#,
        );
        let found = find_main_classes(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn test_kotlin_no_main() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "Util.kt", "fun helper() {}");
        let found = find_main_classes(&[tmp.path().to_path_buf()]).unwrap();
        assert!(found.is_empty());
    }

    // --- Mixed / empty ---

    #[test]
    fn test_empty_source_root() {
        let tmp = TempDir::new().unwrap();
        let found = find_main_classes(&[tmp.path().to_path_buf()]).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn test_fqn_nested_package() {
        assert_eq!(
            derive_fqn(Path::new("/src"), Path::new("/src/com/example/sub/Main.java")),
            Some("com.example.sub.Main".to_string())
        );
    }
}
