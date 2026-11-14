//! Stamp the release build into `fractal --version`.
//!
//! The release workflow sets `FRACTAL_BUILD` (the GitHub run number that
//! also names the `cli-v<N>` tag) and `FRACTAL_GIT_SHA` (the short commit
//! SHA). Local builds set neither and report `(dev)`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FRACTAL_BUILD");
    println!("cargo:rerun-if-env-changed=FRACTAL_GIT_SHA");

    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let build = non_empty("FRACTAL_BUILD");
    let sha = non_empty("FRACTAL_GIT_SHA");

    let suffix = match (build, sha) {
        (Some(build), Some(sha)) => format!("build {build}, {sha}"),
        (Some(build), None) => format!("build {build}"),
        (None, Some(sha)) => format!("dev, {sha}"),
        (None, None) => "dev".to_string(),
    };
    println!("cargo:rustc-env=FRACTAL_VERSION={pkg} ({suffix})");
}

fn non_empty(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
