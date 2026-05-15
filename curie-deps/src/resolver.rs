//! Dependency resolver: cache lookup → download → transitive expansion.
//!
//! # Algorithm
//! 1. For each declared `Gav`, check `~/.m2/repository` for the JAR and POM.
//! 2. On cache miss, try each configured repository in order; download to a
//!    `.part` file, rename atomically on success.
//! 3. Parse the POM to discover compile-scoped transitive dependencies.
//! 4. Recurse (BFS) until the full closure is resolved.
//! 5. Return all resolved JAR paths in stable topological order
//!    (declared deps first, then their transitive deps breadth-first).

use crate::gav::Gav;
use crate::pom::{self, Pom};
use crate::repo::{default_repositories, Repository};
use anyhow::{bail, Context, Result};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// Options for the resolver.
pub struct ResolveOptions {
    /// Additional repositories to check after Maven Central.
    pub extra_repos: Vec<Repository>,
    /// When `true`, print progress to stdout.
    pub verbose: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        ResolveOptions {
            extra_repos: vec![],
            verbose: true,
        }
    }
}

/// Walk the parent POM chain (up to 10 levels) and merge properties +
/// managed_versions into `pom`. Parent values only fill gaps — own values win.
fn merge_parent_chain(
    pom: &mut Pom,
    repos: &[Repository],
    client: &reqwest::blocking::Client,
) {
    let mut depth = 0;
    let mut current_parent = pom.parent.clone();

    while let Some(parent_ref) = current_parent {
        depth += 1;
        if depth > 10 {
            break;
        }

        let parent_gav = Gav {
            group: parent_ref.group_id.clone(),
            artifact: parent_ref.artifact_id.clone(),
            version: parent_ref.version.clone(),
        };

        let pom_path = match ensure_artifact(&parent_gav, repos, client, ArtifactKind::Pom) {
            Ok(p) => p,
            Err(_) => break,
        };
        let xml = match std::fs::read_to_string(&pom_path) {
            Ok(s) => s,
            Err(_) => break,
        };
        let parent_pom = match pom::parse(&xml) {
            Ok(p) => p,
            Err(_) => break,
        };

        // Properties: parent fills gaps.
        for (k, v) in &parent_pom.properties {
            pom.properties.entry(k.clone()).or_insert_with(|| v.clone());
        }
        // Managed versions: parent fills gaps.
        for (k, v) in &parent_pom.managed_versions {
            pom.managed_versions.entry(k.clone()).or_insert_with(|| v.clone());
        }

        current_parent = parent_pom.parent.clone();
    }
}

/// Resolve a list of `(key, version)` pairs from `curie.toml` into a list of
/// local JAR paths (including transitive dependencies).
///
/// `deps` is a slice of `("group:artifact", "version")` pairs as declared
/// in the `[dependencies]` table of `curie.toml`.
pub fn resolve(
    deps: &[(&str, &str)],
    opts: &ResolveOptions,
) -> Result<Vec<PathBuf>> {
    let repos: Vec<Repository> = {
        let mut r = default_repositories();
        r.extend(opts.extra_repos.iter().cloned());
        r
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent("curie-build/0.1")
        .build()
        .context("failed to build HTTP client")?;

    // BFS queue of GAVs to resolve; visited set prevents duplicate work.
    let mut queue: VecDeque<Gav> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut ordered_jars: Vec<PathBuf> = Vec::new();

    // Seed with declared dependencies.
    for (key, version) in deps {
        let gav = Gav::from_key_version(key, version)?;
        if visited.insert(gav.notation()) {
            queue.push_back(gav);
        }
    }

    while let Some(gav) = queue.pop_front() {
        if opts.verbose {
            print!("  Resolving {} ... ", gav);
        }

        // --- JAR ----------------------------------------------------------------
        let jar_path = ensure_artifact(&gav, &repos, &client, ArtifactKind::Jar)?;

        if opts.verbose {
            println!("OK");
        }

        ordered_jars.push(jar_path);

        // --- POM (for transitive deps) ------------------------------------------
        match ensure_artifact(&gav, &repos, &client, ArtifactKind::Pom) {
            Ok(pom_path) => {
                let xml = std::fs::read_to_string(&pom_path)
                    .with_context(|| format!("failed to read POM {}", pom_path.display()))?;
                let mut pom = pom::parse(&xml)
                    .with_context(|| format!("failed to parse POM for {}", gav))?;

                // Walk the full parent chain, merging properties and
                // dependencyManagement entries so that ${property} refs and
                // version-less deps resolve correctly (e.g. jackson-bom).
                merge_parent_chain(&mut pom, &repos, &client);

                for dep in pom.dependencies.iter().filter(|d| d.is_compile_scope()) {
                    let group = pom.resolve_value(&dep.group_id);
                    let artifact = pom.resolve_value(&dep.artifact_id);

                    // Resolve property placeholders in version, then fall back
                    // to dependencyManagement (from this POM or merged parent).
                    let raw_version = match &dep.version {
                        Some(v) => {
                            let resolved = pom.resolve_value(v);
                            if resolved.contains("${") {
                                // Still unresolved — try managed_versions.
                                let key = format!("{}:{}", group, artifact);
                                match pom.managed_versions.get(&key) {
                                    Some(mv) => pom.resolve_value(mv),
                                    None => continue, // give up on this dep
                                }
                            } else {
                                resolved
                            }
                        }
                        None => {
                            // No version at all — look up in dependencyManagement.
                            let key = format!("{}:{}", group, artifact);
                            match pom.managed_versions.get(&key) {
                                Some(mv) => pom.resolve_value(mv),
                                None => continue, // skip version-less dep
                            }
                        }
                    };

                    // Skip if version still unresolved after all attempts.
                    if raw_version.contains("${") {
                        continue;
                    }

                    let child_gav = Gav {
                        group,
                        artifact,
                        version: raw_version,
                    };

                    if visited.insert(child_gav.notation()) {
                        queue.push_back(child_gav);
                    }
                }
            }
            Err(_) => {
                // POM unavailable — continue without transitive expansion.
            }
        }
    }

    Ok(ordered_jars)
}

// ---------------------------------------------------------------------------

enum ArtifactKind {
    Jar,
    Pom,
}

/// Return the local path for an artifact, downloading it if necessary.
fn ensure_artifact(
    gav: &Gav,
    repos: &[Repository],
    client: &reqwest::blocking::Client,
    kind: ArtifactKind,
) -> Result<PathBuf> {
    let local_path = match kind {
        ArtifactKind::Jar => gav.local_cache_path()?,
        ArtifactKind::Pom => gav.local_pom_cache_path()?,
    };
    let relative = match kind {
        ArtifactKind::Jar => gav.relative_path(),
        ArtifactKind::Pom => gav.relative_pom_path(),
    };

    if local_path.exists() {
        return Ok(local_path);
    }

    // Ensure parent directory exists.
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache dir {}", parent.display()))?;
    }

    // Try each repository in order.
    let mut last_err: Option<anyhow::Error> = None;
    for repo in repos {
        let url = repo.artifact_url(&relative);
        match download(client, &url, &local_path) {
            Ok(()) => return Ok(local_path),
            Err(e) => {
                last_err = Some(e);
            }
        }
    }

    bail!(
        "failed to download {} from all repositories: {}",
        gav,
        last_err.unwrap()
    );
}

/// Download `url` to `dest`, using an adjacent `.part` file to avoid partial
/// writes surviving a crash.
fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
) -> Result<()> {
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("HTTP request failed for {}", url))?;

    if !response.status().is_success() {
        bail!("HTTP {} for {}", response.status(), url);
    }

    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read response body for {}", url))?;

    // Write to .part file then rename (atomic on POSIX).
    let part = dest.with_extension("part");
    std::fs::write(&part, &bytes)
        .with_context(|| format!("failed to write {}", part.display()))?;
    std::fs::rename(&part, dest)
        .with_context(|| format!("failed to rename {} → {}", part.display(), dest.display()))?;

    Ok(())
}
