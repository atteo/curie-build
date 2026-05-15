use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Descriptor {
    pub application: Application,
    #[serde(default)]
    pub java: Java,
    #[serde(default)]
    pub docker: Docker,
    /// `[dependencies]` table: keys are `"group:artifact"`, values are version strings.
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    /// `[[repositories]]` array for additional Maven repositories.
    #[serde(default)]
    pub repositories: Vec<RepositoryEntry>,
}

#[derive(Debug, Deserialize)]
pub struct Application {
    pub name: String,
    pub version: String,
    #[serde(rename = "mainClass")]
    pub main_class: String,
}

#[derive(Debug, Deserialize)]
pub struct Java {
    #[serde(rename = "sourceCompatibility", default = "default_source_compat")]
    pub source_compatibility: String,
}

fn default_source_compat() -> String {
    "21".to_string()
}

impl Default for Java {
    fn default() -> Self {
        Java {
            source_compatibility: default_source_compat(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Docker {
    #[serde(rename = "baseImage", default = "default_base_image")]
    pub base_image: String,
    #[serde(rename = "imageName")]
    pub image_name: Option<String>,
    #[serde(rename = "imageTag")]
    pub image_tag: Option<String>,
    /// Tracks whether the [docker] section was explicitly present in curie.toml.
    /// Set by Descriptor::load after deserialisation via a raw TOML check.
    #[serde(skip)]
    pub section_present: bool,
}

fn default_base_image() -> String {
    "eclipse-temurin:21-jre-alpine".to_string()
}

impl Default for Docker {
    fn default() -> Self {
        Docker {
            base_image: default_base_image(),
            image_name: None,
            image_tag: None,
            section_present: false,
        }
    }
}

/// An additional Maven-compatible repository declared in `[[repositories]]`.
#[derive(Debug, Deserialize, Clone)]
pub struct RepositoryEntry {
    pub name: String,
    pub url: String,
}

impl Descriptor {
    /// Resolved Docker image name: descriptor override or application name.
    pub fn image_name(&self) -> &str {
        self.docker
            .image_name
            .as_deref()
            .unwrap_or(&self.application.name)
    }

    /// Resolved Docker image tag: descriptor override or application version.
    pub fn image_tag(&self) -> &str {
        self.docker
            .image_tag
            .as_deref()
            .unwrap_or(&self.application.version)
    }

    /// Full image reference, e.g. "hello-world:0.1.0".
    pub fn image_ref(&self) -> String {
        format!("{}:{}", self.image_name(), self.image_tag())
    }
}

pub fn load(project_root: &Path) -> Result<Descriptor> {
    let path = project_root.join("curie.toml");

    if !path.exists() {
        bail!(
            "no curie.toml found in {}",
            project_root.display()
        );
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    // Parse raw table first so we can detect optional section presence.
    let raw: toml::Value = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let docker_section_present = raw
        .as_table()
        .map(|t| t.contains_key("docker"))
        .unwrap_or(false);

    let mut descriptor: Descriptor = raw
        .try_into()
        .with_context(|| format!("failed to parse {}", path.display()))?;

    descriptor.docker.section_present = docker_section_present;

    Ok(descriptor)
}

/// Returns true when Docker support is active:
/// either a [docker] section exists in curie.toml (non-default base image or
/// explicit name/tag counts as intentional) OR a Dockerfile is present at the
/// project root.
pub fn docker_enabled(project_root: &Path, desc: &Descriptor) -> bool {
    desc.docker.section_present || project_root.join("Dockerfile").exists()
}
