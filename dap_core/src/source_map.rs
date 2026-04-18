/// Bidirectional mapping between JVM binary class names and source file paths.
///
/// Built from the compiled class files in the build output by reading each
/// file's `SourceFile` bytecode attribute.  Used by the breakpoint manager
/// (step 6) to resolve DAP source locations to JDWP class references.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Bidirectional map: class binary name ↔ source file path.
#[derive(Debug, Default)]
pub struct SourceMap {
    /// binary_class_name → source_file_path
    by_class: HashMap<String, PathBuf>,
    /// source_file_path → Vec<binary_class_name>
    by_source: HashMap<PathBuf, Vec<String>>,
}

impl SourceMap {
    /// Returns the source file for a binary class name (e.g. `"com/example/Main"`).
    pub fn source_for_class(&self, class_name: &str) -> Option<&Path> {
        self.by_class.get(class_name).map(PathBuf::as_path)
    }

    /// Returns all binary class names that share the given source file.
    pub fn classes_for_source(&self, source: &Path) -> &[String] {
        self.by_source
            .get(source)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn is_empty(&self) -> bool {
        self.by_class.is_empty()
    }

    fn insert(&mut self, class_name: String, source: PathBuf) {
        self.by_source
            .entry(source.clone())
            .or_default()
            .push(class_name.clone());
        self.by_class.insert(class_name, source);
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Walks all directories in `classpath_dirs` (which are class output directories,
/// not JARs), reads each `.class` file's `SourceFile` attribute, and derives the
/// absolute source file path relative to `source_roots`.
///
/// Must be called after the project has been compiled.
pub fn build_source_map(
    classpath_dirs: &[PathBuf],
    source_roots: &[PathBuf],
) -> Result<SourceMap> {
    let mut map = SourceMap::default();

    for dir in classpath_dirs {
        if dir.is_dir() {
            collect_from_dir(dir, source_roots, &mut map);
        }
    }

    Ok(map)
}

fn collect_from_dir(dir: &Path, source_roots: &[PathBuf], map: &mut SourceMap) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_from_dir(&path, source_roots, map);
        } else if path.extension().and_then(|e| e.to_str()) == Some("class") {
            if let Ok(data) = std::fs::read(&path) {
                if let Ok((class_name, Some(source_file))) = parse_class_info(&data) {
                    if let Some(source_path) = resolve_source(
                        &class_name,
                        &source_file,
                        source_roots,
                    ) {
                        map.insert(class_name, source_path);
                    }
                }
            }
        }
    }
}

/// Derives the absolute source file path from the class binary name and the
/// `SourceFile` attribute filename.
///
/// For Java: the package directory mirrors the class's package, so we join
/// `source_root / package_dir / source_file`.
///
/// For Groovy / Kotlin: the file might not be in a directory matching the
/// package.  We search all source roots for files whose name equals
/// `source_file` and verify the package declaration matches the class package.
fn resolve_source(
    class_name: &str,
    source_file: &str,
    source_roots: &[PathBuf],
) -> Option<PathBuf> {
    let ext = Path::new(source_file).extension()?.to_str()?;
    // Normalise inner/anonymous class name by stripping the $… suffix.
    // The SourceFile attribute already points to the outer class's file, but
    // the class_name may still carry the $ suffix when deriving the package dir.
    let outer_name = strip_inner_suffix(class_name);
    let package_dir = package_dir_of(outer_name);

    match ext {
        "java" => {
            // Package directory mirrors class name — fast, no file scanning.
            for root in source_roots {
                let candidate = if package_dir.is_empty() {
                    root.join(source_file)
                } else {
                    root.join(&package_dir).join(source_file)
                };
                if candidate.exists() {
                    return Some(candidate);
                }
            }
            None
        }
        "groovy" | "kt" | "kts" => {
            // File may not be in a directory matching the package.
            // Scan source roots for a file named `source_file` whose package
            // declaration matches the class's package.
            let class_package = package_dir.replace('/', ".");
            for root in source_roots {
                let mut candidates = Vec::new();
                collect_files_by_name(root, source_file, &mut candidates);
                for candidate in candidates {
                    if file_package_matches(&candidate, &class_package) {
                        return Some(candidate);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Strips the `$…` suffix from an inner or anonymous class name.
/// `"com/example/Outer$Inner"` → `"com/example/Outer"`
/// `"com/example/Outer$1"` → `"com/example/Outer"`
fn strip_inner_suffix(class_name: &str) -> &str {
    match class_name.find('$') {
        Some(i) => &class_name[..i],
        None => class_name,
    }
}

/// Returns the package directory segment of a binary class name.
/// `"com/example/Main"` → `"com/example"`, `"Main"` → `""`
fn package_dir_of(class_name: &str) -> &str {
    match class_name.rfind('/') {
        Some(i) => &class_name[..i],
        None => "",
    }
}

/// Recursively collects all files named `name` under `dir`.
fn collect_files_by_name(dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_by_name(&path, name, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            out.push(path);
        }
    }
}

/// Returns true if the file at `path` declares the given package (or is in
/// the default package when `expected` is empty).
fn file_package_matches(path: &Path, expected: &str) -> bool {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("package ") {
            let decl = rest.trim_end_matches(';').trim();
            return decl == expected;
        }
        // First non-comment, non-blank line is not a package declaration:
        // the file is in the default package.
        return expected.is_empty();
    }
    expected.is_empty()
}

// ---------------------------------------------------------------------------
// Minimal class file parser
//
// Reads only the constant pool, `this_class`, and the class-level `SourceFile`
// attribute.  Enough to build the source map without a full parse library.
// ---------------------------------------------------------------------------

/// Returns `(binary_class_name, source_file_name)` from a raw class file.
/// `source_file_name` is `None` when the `SourceFile` attribute is absent
/// (e.g. compiled without debug info or a synthetic class).
fn parse_class_info(data: &[u8]) -> Result<(String, Option<String>)> {
    let mut pos = 0;

    // Magic
    if read_u32(data, &mut pos)? != 0xCAFE_BABE {
        return Err(anyhow!("not a valid class file"));
    }
    pos += 4; // minor + major version

    // Constant pool
    let cp_count = read_u16(data, &mut pos)? as usize;
    let mut utf8: HashMap<usize, String> = HashMap::new();
    let mut class_name_idx: HashMap<usize, usize> = HashMap::new(); // cp_index → name_index

    let mut idx = 1;
    while idx < cp_count {
        let tag = read_u8(data, &mut pos)?;
        match tag {
            1 => {
                // CONSTANT_Utf8
                let len = read_u16(data, &mut pos)? as usize;
                if pos + len > data.len() {
                    return Err(anyhow!("truncated Utf8 entry"));
                }
                let s = String::from_utf8_lossy(&data[pos..pos + len]).into_owned();
                utf8.insert(idx, s);
                pos += len;
            }
            3 | 4 => pos += 4, // Integer, Float
            5 | 6 => {
                pos += 8; // Long, Double
                idx += 1; // takes 2 slots
            }
            7 => {
                // CONSTANT_Class
                let ni = read_u16(data, &mut pos)? as usize;
                class_name_idx.insert(idx, ni);
            }
            8 | 16 | 19 | 20 => pos += 2, // String, MethodType, Module, Package
            9 | 10 | 11 | 12 | 17 | 18 => pos += 4, // refs, InvokeDynamic
            15 => pos += 3,                           // MethodHandle
            t => return Err(anyhow!("unknown constant pool tag {t}")),
        }
        idx += 1;
    }

    pos += 2; // access_flags

    // this_class
    let this_idx = read_u16(data, &mut pos)? as usize;
    let name_ni = class_name_idx
        .get(&this_idx)
        .copied()
        .ok_or_else(|| anyhow!("this_class CP entry missing"))?;
    let class_name = utf8
        .get(&name_ni)
        .cloned()
        .ok_or_else(|| anyhow!("class name Utf8 entry missing"))?;

    pos += 2; // super_class

    // Interfaces
    let iface_count = read_u16(data, &mut pos)? as usize;
    pos += iface_count * 2;

    // Fields
    let field_count = read_u16(data, &mut pos)? as usize;
    for _ in 0..field_count {
        pos += 6; // access_flags, name_index, descriptor_index
        skip_attributes(data, &mut pos)?;
    }

    // Methods
    let method_count = read_u16(data, &mut pos)? as usize;
    for _ in 0..method_count {
        pos += 6;
        skip_attributes(data, &mut pos)?;
    }

    // Class-level attributes — look for SourceFile
    let source_file = find_source_file(data, &mut pos, &utf8)?;

    Ok((class_name, source_file))
}

fn skip_attributes(data: &[u8], pos: &mut usize) -> Result<()> {
    let count = read_u16(data, pos)? as usize;
    for _ in 0..count {
        *pos += 2; // name_index
        let len = read_u32(data, pos)? as usize;
        if *pos + len > data.len() {
            return Err(anyhow!("attribute length overflows class file"));
        }
        *pos += len;
    }
    Ok(())
}

fn find_source_file(
    data: &[u8],
    pos: &mut usize,
    utf8: &HashMap<usize, String>,
) -> Result<Option<String>> {
    let count = read_u16(data, pos)? as usize;
    for _ in 0..count {
        let name_idx = read_u16(data, pos)? as usize;
        let attr_len = read_u32(data, pos)? as usize;

        let is_source_file = utf8
            .get(&name_idx)
            .map(|n| n == "SourceFile")
            .unwrap_or(false);

        if is_source_file {
            // SourceFile_attribute: u16 sourcefile_index
            let sf_idx = read_u16(data, pos)? as usize;
            // consume any remaining bytes (should be 0, attr_len is always 2)
            *pos += attr_len.saturating_sub(2);
            return Ok(utf8.get(&sf_idx).cloned());
        }
        *pos += attr_len;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Wire-reading helpers (big-endian, slice + cursor)
// ---------------------------------------------------------------------------

fn read_u8(data: &[u8], pos: &mut usize) -> Result<u8> {
    if *pos >= data.len() {
        return Err(anyhow!("class file: buffer underrun at {pos}"));
    }
    let v = data[*pos];
    *pos += 1;
    Ok(v)
}

fn read_u16(data: &[u8], pos: &mut usize) -> Result<u16> {
    if *pos + 2 > data.len() {
        return Err(anyhow!("class file: buffer underrun reading u16"));
    }
    let v = u16::from_be_bytes(data[*pos..*pos + 2].try_into().unwrap());
    *pos += 2;
    Ok(v)
}

fn read_u32(data: &[u8], pos: &mut usize) -> Result<u32> {
    if *pos + 4 > data.len() {
        return Err(anyhow!("class file: buffer underrun reading u32"));
    }
    let v = u32::from_be_bytes(data[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    Ok(v)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(feature = "integration-test")]
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    use crate::build_tools::{BuildToolHandler, gradle::GradleHandler};

    fn build_and_get_map(fixture_name: &str) -> SourceMap {
        let root = fixture(fixture_name);
        GradleHandler.build(&root).unwrap();
        let classpath = GradleHandler.get_classpath(&root).unwrap();
        let source_roots = GradleHandler.get_source_roots(&root).unwrap();
        build_source_map(&classpath, &source_roots).unwrap()
    }

    #[test]
    fn test_java_class_maps_to_source_file() {
        let map = build_and_get_map("simple_java");
        let source = map.source_for_class("Main").expect("Main not in source map");
        assert!(
            source.ends_with("src/main/java/Main.java"),
            "unexpected source path: {source:?}"
        );
    }

    #[test]
    fn test_inner_class_maps_to_outer_source_file() {
        let map = build_and_get_map("inner_class_java");

        let outer_source = map
            .source_for_class("OuterClass")
            .expect("OuterClass not in source map");
        assert!(
            outer_source.ends_with("OuterClass.java"),
            "outer source unexpected: {outer_source:?}"
        );

        let inner_source = map
            .source_for_class("OuterClass$InnerClass")
            .expect("OuterClass$InnerClass not in source map");
        assert_eq!(
            outer_source, inner_source,
            "inner class should map to same source file as outer"
        );
    }

    #[test]
    fn test_anonymous_class_maps_to_outer_source_file() {
        let map = build_and_get_map("inner_class_java");

        let outer_source = map
            .source_for_class("OuterClass")
            .expect("OuterClass not in source map");

        // Anonymous class is named OuterClass$1
        let anon_source = map
            .source_for_class("OuterClass$1")
            .expect("OuterClass$1 not in source map");
        assert_eq!(
            outer_source, anon_source,
            "anonymous class should map to same source file as outer"
        );
    }

    #[test]
    fn test_classes_for_source_returns_all_classes() {
        let map = build_and_get_map("inner_class_java");

        let outer_source = map
            .source_for_class("OuterClass")
            .expect("OuterClass not in source map")
            .to_path_buf();

        let classes = map.classes_for_source(&outer_source);
        let mut names: Vec<_> = classes.iter().map(String::as_str).collect();
        names.sort();

        assert!(
            names.contains(&"OuterClass"),
            "OuterClass missing from source lookup: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.starts_with("OuterClass$")),
            "inner/anonymous class missing from source lookup: {names:?}"
        );
    }
}
