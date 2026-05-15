//! Maven repository configuration.

/// A Maven-compatible artifact repository.
#[derive(Debug, Clone)]
pub struct Repository {
    /// Human-readable name, e.g. "Maven Central".
    pub name: String,
    /// Base URL without trailing slash, e.g. `https://repo1.maven.org/maven2`.
    pub url: String,
}

impl Repository {
    /// Build the full URL for an artifact's relative path.
    pub fn artifact_url(&self, relative_path: &str) -> String {
        format!("{}/{}", self.url.trim_end_matches('/'), relative_path)
    }
}

/// The default set of repositories: Maven Central only.
pub fn default_repositories() -> Vec<Repository> {
    vec![Repository {
        name: "Maven Central".to_string(),
        url: "https://repo1.maven.org/maven2".to_string(),
    }]
}
