//! Minimal Maven POM parser.
//!
//! Parses only what curie-deps needs:
//!   - `<groupId>`, `<artifactId>`, `<version>` (own + parent)
//!   - `<dependencies>` with scope filtering (compile/runtime only; skip test/provided/optional)
//!
//! Property interpolation (`${...}`) is handled for the common `${project.version}`
//! and `${project.parent.version}` patterns.  Full property resolution is left
//! for a future iteration.

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

/// A parsed POM with only the fields relevant to dependency resolution.
#[derive(Debug, Default)]
pub struct Pom {
    pub group_id: Option<String>,
    pub artifact_id: Option<String>,
    pub version: Option<String>,
    pub parent: Option<ParentRef>,
    pub properties: HashMap<String, String>,
    pub dependencies: Vec<PomDep>,
    /// Versions declared in `<dependencyManagement>`, keyed by `"group:artifact"`.
    pub managed_versions: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ParentRef {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct PomDep {
    pub group_id: String,
    pub artifact_id: String,
    pub version: Option<String>,
    pub scope: Option<String>,
    pub optional: bool,
}

impl PomDep {
    /// Returns `true` when this dependency should be included on the compile classpath.
    pub fn is_compile_scope(&self) -> bool {
        if self.optional {
            return false;
        }
        matches!(self.scope.as_deref(), None | Some("compile") | Some("runtime"))
    }
}

impl Pom {
    /// Effective group ID, falling back to parent group if own is absent.
    pub fn effective_group(&self) -> Option<&str> {
        self.group_id
            .as_deref()
            .or_else(|| self.parent.as_ref().map(|p| p.group_id.as_str()))
    }

    /// Effective version, falling back to parent version if own is absent.
    pub fn effective_version(&self) -> Option<&str> {
        self.version
            .as_deref()
            .or_else(|| self.parent.as_ref().map(|p| p.version.as_str()))
    }

    /// Resolve `${property}` references against the pom's own properties and
    /// a small set of built-in variables derived from parent/self.
    ///
    /// Iterates until stable (up to 10 passes) to handle chained references
    /// such as `${jackson.version.annotations}` → `${jackson.version}` → `2.17.2`.
    pub fn resolve_value(&self, value: &str) -> String {
        if !value.contains("${") {
            return value.to_string();
        }
        let mut result = value.to_string();

        for _ in 0..10 {
            if !result.contains("${") {
                break;
            }
            let prev = result.clone();

            // Built-in project variables.
            if let Some(v) = self.effective_version() {
                result = result.replace("${project.version}", v);
                result = result.replace("${project.parent.version}", v);
            }

            // User-defined <properties>.
            for (k, v) in &self.properties {
                result = result.replace(&format!("${{{}}}", k), v);
            }

            // No progress — stop to avoid infinite loop.
            if result == prev {
                break;
            }
        }

        result
    }
}

/// Parse POM XML from a string, returning a [`Pom`].
pub fn parse(xml: &str) -> Result<Pom> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut pom = Pom::default();

    // tag stack for tracking position
    let mut stack: Vec<String> = Vec::new();

    // scratch buffers for current dependency being parsed
    let mut cur_dep: Option<PomDep> = None;
    // scratch buffer for current dependencyManagement entry
    let mut cur_mgd: Option<PomDep> = None;
    // current text accumulator
    let mut text_buf = String::new();

    loop {
        match reader.read_event().context("XML read error")? {
            Event::Start(e) => {
                let tag = std::str::from_utf8(e.name().as_ref())
                    .unwrap_or("")
                    .to_string();

                if tag == "dependency" && in_direct_dependencies(&stack) {
                    cur_dep = Some(PomDep {
                        group_id: String::new(),
                        artifact_id: String::new(),
                        version: None,
                        scope: None,
                        optional: false,
                    });
                }

                if tag == "dependency" && in_dependency_management(&stack) {
                    cur_mgd = Some(PomDep {
                        group_id: String::new(),
                        artifact_id: String::new(),
                        version: None,
                        scope: None,
                        optional: false,
                    });
                }

                stack.push(tag);
                text_buf.clear();
            }
            Event::Text(e) => {
                text_buf = e.unescape().unwrap_or_default().to_string();
            }
            Event::End(e) => {
                let tag = std::str::from_utf8(e.name().as_ref())
                    .unwrap_or("")
                    .to_string();

                let path = stack.join("/");

                match path.as_str() {
                    // --- top-level fields ----------------------------------------
                    "project/groupId" => pom.group_id = Some(text_buf.clone()),
                    "project/artifactId" => pom.artifact_id = Some(text_buf.clone()),
                    "project/version" => pom.version = Some(text_buf.clone()),

                    // --- parent --------------------------------------------------
                    "project/parent/groupId" => {
                        pom.parent.get_or_insert_with(|| ParentRef {
                            group_id: String::new(),
                            artifact_id: String::new(),
                            version: String::new(),
                        }).group_id = text_buf.clone();
                    }
                    "project/parent/artifactId" => {
                        pom.parent.get_or_insert_with(|| ParentRef {
                            group_id: String::new(),
                            artifact_id: String::new(),
                            version: String::new(),
                        }).artifact_id = text_buf.clone();
                    }
                    "project/parent/version" => {
                        pom.parent.get_or_insert_with(|| ParentRef {
                            group_id: String::new(),
                            artifact_id: String::new(),
                            version: String::new(),
                        }).version = text_buf.clone();
                    }

                    // --- properties ----------------------------------------------
                    _ if path.starts_with("project/properties/") => {
                        if let Some(key) = path.strip_prefix("project/properties/") {
                            pom.properties.insert(key.to_string(), text_buf.clone());
                        }
                    }

                    // --- dependency fields ---------------------------------------
                    _ if path.ends_with("/dependency/groupId")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.group_id = text_buf.clone();
                        }
                    }
                    _ if path.ends_with("/dependency/artifactId")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.artifact_id = text_buf.clone();
                        }
                    }
                    _ if path.ends_with("/dependency/version")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.version = Some(text_buf.clone());
                        }
                    }
                    _ if path.ends_with("/dependency/scope")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.scope = Some(text_buf.clone());
                        }
                    }
                    _ if path.ends_with("/dependency/optional")
                        && in_direct_dependencies_path(&path) =>
                    {
                        if let Some(d) = cur_dep.as_mut() {
                            d.optional = text_buf.trim() == "true";
                        }
                    }
                    _ if tag == "dependency" && in_direct_dependencies_path(&path) => {
                        if let Some(d) = cur_dep.take() {
                            pom.dependencies.push(d);
                        }
                    }

                    // --- dependencyManagement fields -----------------------------
                    _ if path.ends_with("/dependency/groupId")
                        && path.contains("dependencyManagement") =>
                    {
                        if let Some(d) = cur_mgd.as_mut() {
                            d.group_id = text_buf.clone();
                        }
                    }
                    _ if path.ends_with("/dependency/artifactId")
                        && path.contains("dependencyManagement") =>
                    {
                        if let Some(d) = cur_mgd.as_mut() {
                            d.artifact_id = text_buf.clone();
                        }
                    }
                    _ if path.ends_with("/dependency/version")
                        && path.contains("dependencyManagement") =>
                    {
                        if let Some(d) = cur_mgd.as_mut() {
                            d.version = Some(text_buf.clone());
                        }
                    }
                    _ if tag == "dependency" && path.contains("dependencyManagement") => {
                        if let Some(d) = cur_mgd.take() {
                            if let Some(v) = d.version {
                                let key = format!("{}:{}", d.group_id, d.artifact_id);
                                pom.managed_versions.insert(key, v);
                            }
                        }
                    }

                    _ => {}
                }

                stack.pop();
                text_buf.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(pom)
}

/// `stack` currently points *inside* `project/dependencies` (not `dependencyManagement`).
fn in_direct_dependencies(stack: &[String]) -> bool {
    let path = stack.join("/");
    path == "project/dependencies"
        || path.ends_with("/project/dependencies")
}

fn in_dependency_management(stack: &[String]) -> bool {
    let path = stack.join("/");
    path.contains("dependencyManagement") && path.ends_with("/dependencies")
}

fn in_direct_dependencies_path(path: &str) -> bool {
    // must contain /dependencies/ but NOT /dependencyManagement/
    path.contains("/dependencies/") && !path.contains("dependencyManagement")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_POM: &str = r#"<?xml version="1.0"?>
<project>
  <groupId>com.example</groupId>
  <artifactId>myapp</artifactId>
  <version>1.0.0</version>
  <dependencies>
    <dependency>
      <groupId>com.google.guava</groupId>
      <artifactId>guava</artifactId>
      <version>33.2.0-jre</version>
    </dependency>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
      <version>4.13.2</version>
      <scope>test</scope>
    </dependency>
  </dependencies>
</project>"#;

    #[test]
    fn parse_basic() {
        let pom = parse(SIMPLE_POM).unwrap();
        assert_eq!(pom.group_id.as_deref(), Some("com.example"));
        assert_eq!(pom.artifact_id.as_deref(), Some("myapp"));
        assert_eq!(pom.version.as_deref(), Some("1.0.0"));
        assert_eq!(pom.dependencies.len(), 2);
    }

    #[test]
    fn compile_scope_filter() {
        let pom = parse(SIMPLE_POM).unwrap();
        let compile: Vec<_> = pom.dependencies.iter().filter(|d| d.is_compile_scope()).collect();
        assert_eq!(compile.len(), 1);
        assert_eq!(compile[0].artifact_id, "guava");
    }
}
