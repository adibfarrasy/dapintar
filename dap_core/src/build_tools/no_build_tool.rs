use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::build_tools::{BuildResult, BuildToolHandler};

/// Fallback when no recognised build tool is found.
/// Reports a successful no-op build and returns any standard source directories
/// that happen to exist under the project root.
pub struct NoBuildTool;

impl BuildToolHandler for NoBuildTool {
    fn is_project(&self, _root: &Path) -> bool {
        true
    }

    fn is_build_file(&self, _path: &Path) -> bool {
        false
    }

    fn build(&self, _root: &Path) -> Result<BuildResult> {
        Ok(BuildResult {
            success: true,
            errors: vec![],
        })
    }

    fn get_source_roots(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let candidates = [
            "src/main/java",
            "src/main/groovy",
            "src/main/kotlin",
        ];
        let roots = candidates
            .iter()
            .map(|rel| root.join(rel))
            .filter(|p| p.exists())
            .collect();
        Ok(roots)
    }

    fn get_classpath(&self, root: &Path) -> Result<Vec<PathBuf>> {
        // Return any standard build output directories that exist.
        let candidates = [
            "build/classes/java/main",
            "build/classes/groovy/main",
            "build/classes/kotlin/main",
            "out/production/classes",
        ];
        let entries = candidates
            .iter()
            .map(|rel| root.join(rel))
            .filter(|p| p.exists())
            .collect();
        Ok(entries)
    }
}
