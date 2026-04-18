pub mod gradle;
pub mod no_build_tool;

use std::{path::{Path, PathBuf}, sync::Arc};

use anyhow::Result;

use crate::build_tools::{gradle::GradleHandler, no_build_tool::NoBuildTool};

/// The result of invoking the build tool to compile the project.
pub struct BuildResult {
    pub success: bool,
    /// Compiler error lines when success is false, empty otherwise.
    pub errors: Vec<String>,
}

pub trait BuildToolHandler: Send + Sync {
    /// Returns true if `root` is a project managed by this build tool.
    fn is_project(&self, root: &Path) -> bool;

    /// Returns true if `path` is a build configuration file for this tool.
    fn is_build_file(&self, path: &Path) -> bool;

    /// Compiles the project. Returns errors if compilation fails.
    fn build(&self, root: &Path) -> Result<BuildResult>;

    /// Returns source root directories that currently exist on disk.
    /// Covers all subprojects in a multi-module build.
    fn get_source_roots(&self, root: &Path) -> Result<Vec<PathBuf>>;

    /// Returns the runtime classpath entries needed to launch the built project.
    /// Must be called after `build()` succeeds so compiled class directories exist.
    fn get_classpath(&self, root: &Path) -> Result<Vec<PathBuf>>;
}

/// Returns the build tool handler for the project at `root`.
/// Falls back to `NoBuildTool` when no recognised build file is found.
pub fn get_build_tool(root: &Path) -> Arc<dyn BuildToolHandler + Send + Sync> {
    let providers: Vec<Arc<dyn BuildToolHandler>> = vec![Arc::new(GradleHandler)];
    providers
        .into_iter()
        .find(|p| p.is_project(root))
        .unwrap_or_else(|| Arc::new(NoBuildTool))
}
