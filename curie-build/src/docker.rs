use crate::build::mtime;
use crate::descriptor::Descriptor;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Determines which Dockerfile strategy to use.
enum DockerfileSource {
    /// User provided a Dockerfile at the project root. Build context = project root.
    UserProvided,
    /// Curie generates a Dockerfile in target/docker/. Build context = target/docker/.
    Generated,
}

fn dockerfile_source(project_root: &Path) -> DockerfileSource {
    if project_root.join("Dockerfile").exists() {
        DockerfileSource::UserProvided
    } else {
        DockerfileSource::Generated
    }
}

/// Build a Docker image. Returns the full image reference used (name:tag).
pub fn docker_build(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
) -> Result<String> {
    let image_ref = desc.image_ref();

    match dockerfile_source(project_root) {
        DockerfileSource::UserProvided => {
            build_with_user_dockerfile(project_root, desc, jar, &image_ref)?;
        }
        DockerfileSource::Generated => {
            build_with_generated_dockerfile(project_root, desc, jar, dep_jars, &image_ref)?;
        }
    }

    Ok(image_ref)
}

/// Run a Docker container from the built image, forwarding extra_args to the
/// container entrypoint. The container is removed after it exits (--rm).
pub fn docker_run(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
    extra_args: &[String],
) -> Result<()> {
    let image_ref = docker_build(project_root, desc, jar, dep_jars)?;

    println!("Running container {}  (--rm)", image_ref);
    println!();

    let mut cmd = Command::new("docker");
    cmd.arg("run").arg("--rm").arg(&image_ref);

    for arg in extra_args {
        cmd.arg(arg);
    }

    let status = cmd
        .status()
        .context("failed to invoke docker run — is Docker installed?")?;

    if !status.success() {
        let code = status.code().unwrap_or(1);
        std::process::exit(code);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_with_user_dockerfile(
    project_root: &Path,
    _desc: &Descriptor,
    jar: &Path,
    image_ref: &str,
) -> Result<()> {
    println!("  Using Dockerfile from project root");

    // Make JAR path relative to project root for the build arg.
    let jar_rel = jar
        .strip_prefix(project_root)
        .unwrap_or(jar)
        .to_string_lossy()
        .to_string();

    let status = Command::new("docker")
        .arg("build")
        .arg("--build-arg")
        .arg(format!("JAR_FILE={}", jar_rel))
        .arg("-t")
        .arg(image_ref)
        .arg(project_root)
        .status()
        .context("failed to invoke docker build — is Docker installed?")?;

    if !status.success() {
        bail!("docker build failed");
    }

    println!("  Image built: {}", image_ref);
    Ok(())
}

fn build_with_generated_dockerfile(
    project_root: &Path,
    desc: &Descriptor,
    jar: &Path,
    dep_jars: &[PathBuf],
    image_ref: &str,
) -> Result<()> {
    let docker_dir = project_root.join("target").join("docker");
    std::fs::create_dir_all(&docker_dir).context("failed to create target/docker")?;

    // Copy the JAR into the build context directory (skip if already current).
    let jar_filename = jar
        .file_name()
        .context("JAR path has no filename")?
        .to_string_lossy()
        .to_string();

    let jar_dest = docker_dir.join(&jar_filename);
    if mtime(jar) > mtime(&jar_dest) {
        std::fs::copy(jar, &jar_dest)
            .with_context(|| format!("failed to copy JAR to {}", jar_dest.display()))?;
    }

    // Copy dependency JARs into target/docker/libs/ (skip up-to-date files).
    let mut dep_filenames: Vec<String> = Vec::new();
    if !dep_jars.is_empty() {
        let libs_dir = docker_dir.join("libs");
        std::fs::create_dir_all(&libs_dir).context("failed to create target/docker/libs")?;

        let mut copied = 0usize;
        for dep in dep_jars {
            let fname = dep
                .file_name()
                .context("dep JAR path has no filename")?
                .to_string_lossy()
                .to_string();
            let dest = libs_dir.join(&fname);
            if mtime(dep) > mtime(&dest) {
                std::fs::copy(dep, &dest)
                    .with_context(|| format!("failed to copy dep JAR {} to {}", dep.display(), dest.display()))?;
                copied += 1;
            }
            dep_filenames.push(fname);
        }

        if copied > 0 {
            println!("  Copied {} dep JAR(s) to target/docker/libs/", copied);
        } else {
            println!("  Dep JARs up to date");
        }
    }

    // Generate the Dockerfile.
    let dockerfile_content =
        generate_dockerfile(&desc.docker.base_image, &jar_filename, &dep_filenames);
    let dockerfile_path = docker_dir.join("Dockerfile");
    std::fs::write(&dockerfile_path, &dockerfile_content)
        .context("failed to write generated Dockerfile")?;

    println!(
        "  Generated {}",
        dockerfile_path
            .strip_prefix(project_root)
            .unwrap_or(&dockerfile_path)
            .display()
    );

    let status = Command::new("docker")
        .arg("build")
        .arg("-t")
        .arg(image_ref)
        .arg(&docker_dir)
        .status()
        .context("failed to invoke docker build — is Docker installed?")?;

    if !status.success() {
        bail!("docker build failed");
    }

    println!("  Image built: {}", image_ref);
    Ok(())
}

fn generate_dockerfile(
    base_image: &str,
    jar_filename: &str,
    dep_filenames: &[String],
) -> String {
    let mut lines = vec![
        format!("FROM {}", base_image),
        "WORKDIR /app".to_string(),
        format!("COPY {} app.jar", jar_filename),
    ];

    if dep_filenames.is_empty() {
        lines.push("ENTRYPOINT [\"java\", \"-jar\", \"app.jar\"]".to_string());
    } else {
        // Copy all dep JARs into /app/libs/ inside the image.
        lines.push("COPY libs/ libs/".to_string());

        // Build CLASSPATH: app.jar + libs/<dep>.jar entries, separated by ":"
        let mut cp_parts = vec!["app.jar".to_string()];
        for fname in dep_filenames {
            cp_parts.push(format!("libs/{}", fname));
        }
        let classpath = cp_parts.join(":");

        lines.push(format!("ENV CLASSPATH={}", classpath));
        // Use -cp $CLASSPATH so the JVM honours our explicit classpath.
        // The main class is embedded in app.jar's MANIFEST.MF but we must
        // specify it explicitly when -cp overrides -jar class loading.
        lines.push("ENTRYPOINT [\"java\", \"-jar\", \"app.jar\"]".to_string());
    }

    lines.join("\n") + "\n"
}
