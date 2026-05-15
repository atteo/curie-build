use crate::{descriptor, docker};
use anyhow::{bail, Context, Result};
use curie_deps::resolver::{resolve, ResolveOptions};
use curie_deps::repo::Repository;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

/// Reproducible-build epoch: 2024-01-01 00:00:00 UTC.
/// Matches SOURCE_DATE_EPOCH convention used by Debian, Nix, etc.
fn epoch() -> zip::DateTime {
    zip::DateTime::from_date_and_time(2024, 1, 1, 0, 0, 0)
        .expect("epoch constant is valid")
}

pub struct BuildOptions {
    pub no_docker: bool,
}

/// Output paths produced by a successful build.
pub struct BuildOutput {
    pub jar: PathBuf,
    /// Resolved dependency JARs (empty when no [dependencies] declared).
    pub dep_jars: Vec<PathBuf>,
}

pub fn build(project_root: &Path, opts: BuildOptions) -> Result<()> {
    let desc = descriptor::load(project_root)?;

    println!(
        "Building {} v{}",
        desc.application.name, desc.application.version
    );

    let output = do_build(project_root, &desc)?;

    println!(
        "Built {}",
        output
            .jar
            .strip_prefix(project_root)
            .unwrap_or(&output.jar)
            .display()
    );

    if !opts.no_docker && descriptor::docker_enabled(project_root, &desc) {
        println!("Building Docker image");
        docker::docker_build(project_root, &desc, &output.jar, &output.dep_jars)?;
    }

    Ok(())
}

pub fn do_build(
    project_root: &Path,
    desc: &descriptor::Descriptor,
) -> Result<BuildOutput> {
    // --- directories ---------------------------------------------------------
    let src_root = project_root.join("src").join("main").join("java");
    if !src_root.exists() {
        bail!("source directory not found: {}", src_root.display());
    }

    let classes_dir = project_root.join("target").join("classes");
    let output_dir = project_root.join("target");

    std::fs::create_dir_all(&classes_dir)
        .context("failed to create target/classes")?;

    // --- resolve dependencies ------------------------------------------------
    let dep_jars = if desc.dependencies.is_empty() {
        vec![]
    } else {
        println!("  Resolving dependencies");
        let pairs: Vec<(&str, &str)> = desc
            .dependencies
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let extra_repos: Vec<Repository> = desc
            .repositories
            .iter()
            .map(|r| Repository {
                name: r.name.clone(),
                url: r.url.clone(),
            })
            .collect();

        resolve(
            &pairs,
            &ResolveOptions {
                extra_repos,
                verbose: true,
            },
        )
        .context("dependency resolution failed")?
    };

    // --- discover sources (exclude *Test.java, *Tests.java, *Spec.java) ------
    // Sort deterministically so javac always receives files in the same order
    // regardless of filesystem or OS.
    let mut sources: Vec<_> = WalkDir::new(&src_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy();
            name.ends_with(".java")
                && !name.ends_with("Test.java")
                && !name.ends_with("Tests.java")
                && !name.ends_with("Spec.java")
        })
        .map(|e| e.into_path())
        .collect();

    if sources.is_empty() {
        bail!("no Java source files found under {}", src_root.display());
    }

    // Lexicographic sort → deterministic javac input order.
    sources.sort();

    println!("  Compiling {} source file(s)", sources.len());

    // --- compile -------------------------------------------------------------
    let mut javac = Command::new("javac");
    javac
        .arg("--release")
        .arg(&desc.java.source_compatibility)
        .arg("-d")
        .arg(&classes_dir);

    if !dep_jars.is_empty() {
        let cp = classpath_string(&dep_jars);
        javac.arg("-cp").arg(&cp);
    }

    for src in &sources {
        javac.arg(src);
    }

    let status = javac
        .status()
        .context("failed to invoke javac — is a JDK installed?")?;

    if !status.success() {
        bail!("compilation failed");
    }

    // --- package (deterministic JAR) -----------------------------------------
    let jar_name = format!(
        "{}-{}.jar",
        desc.application.name, desc.application.version
    );
    let jar_path = output_dir.join(&jar_name);

    println!("  Packaging {}", jar_name);

    write_deterministic_jar(&jar_path, &classes_dir, &desc.application.main_class, &dep_jars)
        .context("failed to write JAR")?;

    Ok(BuildOutput { jar: jar_path, dep_jars })
}

/// Build an OS-appropriate classpath string from a list of JAR paths.
pub fn classpath_string(jars: &[PathBuf]) -> String {
    join_classpath(jars)
}

/// Build the `Class-Path` manifest value: space-separated list of JAR
/// filenames relative to the application JAR (they will sit in `libs/`).
fn manifest_class_path(dep_jars: &[PathBuf]) -> String {
    dep_jars
        .iter()
        .filter_map(|p| p.file_name())
        .map(|f| format!("libs/{}", f.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Write a reproducible JAR:
///   • entries sorted lexicographically
///   • all timestamps set to EPOCH (2024-01-01 00:00:00 UTC)
///   • MANIFEST.MF written first (JAR spec requirement)
///   • Class-Path header added when dep_jars is non-empty
///   • no extra tool-specific metadata that embeds build time
fn write_deterministic_jar(
    jar_path: &Path,
    classes_dir: &Path,
    main_class: &str,
    dep_jars: &[PathBuf],
) -> Result<()> {
    let file = std::fs::File::create(jar_path)
        .with_context(|| format!("cannot create {}", jar_path.display()))?;

    let mut zip = ZipWriter::new(file);

    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(epoch())
        .unix_permissions(0o644);

    let dir_options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(epoch())
        .unix_permissions(0o755);

    // --- MANIFEST.MF (must be first entry per JAR spec) ---------------------
    zip.start_file("META-INF/", dir_options)
        .context("failed to write META-INF/ directory entry")?;

    let mut manifest = format!(
        "Manifest-Version: 1.0\r\nMain-Class: {}\r\n",
        main_class
    );
    if !dep_jars.is_empty() {
        // The JAR spec requires the Class-Path value to be folded at 72 bytes.
        // For simplicity we write the full value and rely on the JVM's leniency.
        manifest.push_str(&format!(
            "Class-Path: {}\r\n",
            manifest_class_path(dep_jars)
        ));
    }
    manifest.push_str("\r\n");

    zip.start_file("META-INF/MANIFEST.MF", options)
        .context("failed to start MANIFEST.MF entry")?;
    zip.write_all(manifest.as_bytes())
        .context("failed to write MANIFEST.MF")?;

    // --- collect all class-file entries, sort them --------------------------
    let mut entries: Vec<(String, PathBuf)> = WalkDir::new(classes_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() || e.file_type().is_dir())
        .filter_map(|e| {
            let rel = e
                .path()
                .strip_prefix(classes_dir)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/"); // normalise on Windows
            if rel.is_empty() {
                return None; // skip classes_dir root itself
            }
            let zip_path = if e.file_type().is_dir() {
                format!("{}/", rel)
            } else {
                rel
            };
            Some((zip_path, e.into_path()))
        })
        .collect();

    // Stable sort: directories before their contents, then lexicographic.
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (zip_path, fs_path) in &entries {
        if zip_path.ends_with('/') {
            zip.start_file(zip_path, dir_options)
                .with_context(|| format!("failed to write directory entry {}", zip_path))?;
        } else {
            zip.start_file(zip_path, options)
                .with_context(|| format!("failed to start entry {}", zip_path))?;
            let data = std::fs::read(fs_path)
                .with_context(|| format!("failed to read {}", fs_path.display()))?;
            zip.write_all(&data)
                .with_context(|| format!("failed to write entry {}", zip_path))?;
        }
    }

    zip.finish().context("failed to finalise JAR")?;
    Ok(())
}

/// Join JAR paths with the OS classpath separator (":" on Unix, ";" on Windows).
fn join_classpath(jars: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    jars.iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep)
}
