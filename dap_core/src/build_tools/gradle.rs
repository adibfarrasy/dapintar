use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};

use crate::build_tools::{BuildResult, BuildToolHandler};

pub struct GradleHandler;

impl GradleHandler {
    fn gradle_cmd(root: &Path) -> String {
        if root.join("gradlew").exists() {
            root.join("gradlew").to_string_lossy().into_owned()
        } else {
            "gradle".to_string()
        }
    }
}

impl BuildToolHandler for GradleHandler {
    fn is_project(&self, root: &Path) -> bool {
        root.join("build.gradle").exists()
            || root.join("build.gradle.kts").exists()
            || root.join("settings.gradle").exists()
            || root.join("settings.gradle.kts").exists()
    }

    fn is_build_file(&self, path: &Path) -> bool {
        matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some(
                "build.gradle"
                    | "build.gradle.kts"
                    | "settings.gradle"
                    | "settings.gradle.kts"
            )
        )
    }

    fn build(&self, root: &Path) -> Result<BuildResult> {
        let gradle_cmd = Self::gradle_cmd(root);
        let output = Command::new(&gradle_cmd)
            .current_dir(root)
            .args(["classes", "--console=plain"])
            .output()
            .context("failed to execute gradle")?;

        if output.status.success() {
            return Ok(BuildResult {
                success: true,
                errors: vec![],
            });
        }

        // Compiler errors land in stdout; Gradle's own diagnostics in stderr.
        // Collect lines that look like compiler errors across both streams.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let errors: Vec<String> = stdout
            .lines()
            .chain(stderr.lines())
            .filter(|line| {
                let l = line.trim();
                l.contains(": error:")       // javac / kotlinc
                    || l.starts_with("e: ")  // kotlinc standalone
                    || l.contains("error: ") // groovyc
            })
            .map(|l| l.to_string())
            .collect();

        // Fall back to full stdout when no pattern matched so the user
        // always sees something actionable.
        let errors = if errors.is_empty() {
            stdout
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
                .collect()
        } else {
            errors
        };

        Ok(BuildResult {
            success: false,
            errors,
        })
    }

    fn get_classpath(&self, root: &Path) -> Result<Vec<PathBuf>> {
        // Register a root-project task that iterates all subprojects so this
        // works for both single-project and multi-project Gradle builds.
        let init_script = r#"
gradle.projectsEvaluated {
    rootProject.task('dapClasspath') {
        doLast {
            def seen = [] as Set
            rootProject.allprojects.each { proj ->
                if (['java', 'groovy', 'kotlin', 'org.jetbrains.kotlin.jvm']
                        .any { proj.plugins.hasPlugin(it) }) {
                    try {
                        proj.sourceSets.main.runtimeClasspath.each {
                            if (seen.add(it.absolutePath)) println it.absolutePath
                        }
                    } catch (ignored) {}
                }
            }
        }
    }
}
"#;
        let temp_init = std::env::temp_dir().join("dap-gradle-classpath.gradle");
        std::fs::write(&temp_init, init_script)?;

        let gradle_cmd = Self::gradle_cmd(root);
        let output = Command::new(&gradle_cmd)
            .current_dir(root)
            .args(["-I", temp_init.to_str().unwrap(), "dapClasspath", "-q"])
            .output()
            .context("failed to execute gradle for classpath")?;

        if !output.status.success() {
            anyhow::bail!(
                "gradle classpath query failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let entries = String::from_utf8(output.stdout)?
            .lines()
            .map(|l| PathBuf::from(l.trim()))
            .filter(|p| p.exists())
            .collect();

        Ok(entries)
    }

    fn get_source_roots(&self, root: &Path) -> Result<Vec<PathBuf>> {
        // Register a root-project task that iterates all subprojects so this
        // works for both single-project and multi-project Gradle builds.
        let init_script = r#"
gradle.projectsEvaluated {
    rootProject.task('dapSourceRoots') {
        doLast {
            rootProject.allprojects.each { proj ->
                if (['java', 'groovy', 'kotlin', 'org.jetbrains.kotlin.jvm']
                        .any { proj.plugins.hasPlugin(it) }) {
                    proj.sourceSets.main.allSource.srcDirs
                        .findAll { it.exists() }
                        .each { println it.absolutePath }
                }
            }
        }
    }
}
"#;
        let temp_init = std::env::temp_dir().join("dap-gradle-source-roots.gradle");
        std::fs::write(&temp_init, init_script)?;

        let gradle_cmd = Self::gradle_cmd(root);
        let output = Command::new(&gradle_cmd)
            .current_dir(root)
            .args(["-I", temp_init.to_str().unwrap(), "dapSourceRoots", "-q"])
            .output()
            .context("failed to execute gradle")?;

        if !output.status.success() {
            anyhow::bail!(
                "gradle failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let roots = String::from_utf8(output.stdout)?
            .lines()
            .map(|l| PathBuf::from(l.trim()))
            .filter(|p| p.exists())
            .collect();

        Ok(roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_project_build_gradle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("build.gradle"), "").unwrap();
        assert!(GradleHandler.is_project(dir.path()));
    }

    #[test]
    fn test_is_project_settings_gradle_kts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.gradle.kts"), "").unwrap();
        assert!(GradleHandler.is_project(dir.path()));
    }

    #[test]
    fn test_is_not_project_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!GradleHandler.is_project(dir.path()));
    }

    #[test]
    fn test_is_build_file() {
        assert!(GradleHandler.is_build_file(Path::new("build.gradle")));
        assert!(GradleHandler.is_build_file(Path::new("build.gradle.kts")));
        assert!(GradleHandler.is_build_file(Path::new("settings.gradle")));
        assert!(!GradleHandler.is_build_file(Path::new("Main.java")));
        assert!(!GradleHandler.is_build_file(Path::new("pom.xml")));
    }
}

#[cfg(feature = "integration-test")]
#[cfg(test)]
mod integration_tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn test_build_success() {
        let root = fixture("simple_java");
        let result = GradleHandler.build(&root).expect("build() should not error");
        assert!(result.success, "expected successful build; errors: {:?}", result.errors);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_build_failure_returns_errors() {
        let root = fixture("compile_error_java");
        let result = GradleHandler.build(&root).expect("build() should not error");
        assert!(!result.success, "expected build to fail");
        assert!(
            !result.errors.is_empty(),
            "expected at least one error line"
        );
    }

    #[test]
    fn test_get_source_roots_returns_java_root() {
        let root = fixture("simple_java");
        let roots = GradleHandler
            .get_source_roots(&root)
            .expect("get_source_roots() should not error");
        assert!(
            !roots.is_empty(),
            "expected at least one source root"
        );
        assert!(
            roots.iter().any(|r| r.ends_with("src/main/java")),
            "expected src/main/java in roots; got {roots:?}"
        );
    }

    #[test]
    fn test_get_source_roots_multi_module_returns_all_subprojects() {
        let root = fixture("multi_module_java");
        let roots = GradleHandler
            .get_source_roots(&root)
            .expect("get_source_roots() should not error");
        assert!(
            roots.iter().any(|r| r.ends_with("app/src/main/java")),
            "expected app source root; got {roots:?}"
        );
        assert!(
            roots.iter().any(|r| r.ends_with("lib/src/main/java")),
            "expected lib source root; got {roots:?}"
        );
    }

    #[test]
    fn test_build_multi_module_compiles_all_subprojects() {
        let root = fixture("multi_module_java");
        let result = GradleHandler.build(&root).expect("build() should not error");
        assert!(result.success, "expected successful build; errors: {:?}", result.errors);

        // Both subproject class directories should exist after build.
        let app_classes = root.join("app/build/classes/java/main");
        let lib_classes = root.join("lib/build/classes/java/main");
        assert!(app_classes.exists(), "app classes dir missing: {app_classes:?}");
        assert!(lib_classes.exists(), "lib classes dir missing: {lib_classes:?}");
    }
}
