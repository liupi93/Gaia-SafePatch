use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, anyhow};
use aya_build::Toolchain;

fn main() -> anyhow::Result<()> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR")?);
    let webui_dir = manifest_dir.join("../gaia-webui");

    configure_webui_rerun(&webui_dir)?;
    build_webui(&webui_dir)?;

    let cargo_metadata::Metadata { packages, .. } = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .context("MetadataCommand::exec")?;
    let ebpf_package = packages
        .into_iter()
        .find(|cargo_metadata::Package { name, .. }| name.as_str() == "gaia-xdp-ebpf")
        .ok_or_else(|| anyhow!("gaia-xdp-ebpf package not found"))?;
    let cargo_metadata::Package {
        name,
        manifest_path,
        ..
    } = ebpf_package;
    let ebpf_package = aya_build::Package {
        name: name.as_str(),
        root_dir: manifest_path
            .parent()
            .ok_or_else(|| anyhow!("no parent for {manifest_path}"))?
            .as_str(),
        ..Default::default()
    };
    aya_build::build_ebpf(
        [ebpf_package],
        Toolchain::Custom("nightly-2025-05-01"),
    )?;
    println!("cargo:rustc-env=OUT_DIR={}", env::var("OUT_DIR").unwrap());
    Ok(())
}

fn configure_webui_rerun(webui_dir: &Path) -> anyhow::Result<()> {
    for relative in [
        "package.json",
        "pnpm-lock.yaml",
        "tsconfig.json",
        "tsconfig.app.json",
        "tsconfig.node.json",
        "vite.config.ts",
        "index.html",
    ] {
        println!("cargo:rerun-if-changed={}", webui_dir.join(relative).display());
    }

    let src_dir = webui_dir.join("src");
    if src_dir.exists() {
        emit_rerun_for_dir(&src_dir)?;
    }

    let public_dir = webui_dir.join("public");
    if public_dir.exists() {
        emit_rerun_for_dir(&public_dir)?;
    }

    Ok(())
}

fn emit_rerun_for_dir(dir: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            emit_rerun_for_dir(&path)?;
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    Ok(())
}

fn build_webui(webui_dir: &Path) -> anyhow::Result<()> {
    let pnpm = which::which("pnpm").context("pnpm not found in PATH")?;
    let install_status = Command::new(&pnpm)
        .arg("install")
        .current_dir(webui_dir)
        .status()
        .with_context(|| format!("failed to run pnpm install in {}", webui_dir.display()))?;

    if !install_status.success() {
        return Err(anyhow!("pnpm install failed with status {install_status}"));
    }

    let build_status = Command::new(pnpm)
        .arg("build")
        .current_dir(webui_dir)
        .status()
        .with_context(|| format!("failed to run pnpm build in {}", webui_dir.display()))?;

    if !build_status.success() {
        return Err(anyhow!("pnpm build failed with status {build_status}"));
    }

    Ok(())
}
