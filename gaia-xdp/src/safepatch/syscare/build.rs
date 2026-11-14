use std::{env, path::Path, process::Command};
pub fn build(root: &Path) {
    let source = root.join("bridge.c");
    println!("cargo:rerun-if-changed={}", source.display());
    let out = std::path::PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for (cmd, args) in [
        (
            "cc",
            vec![
                "-std=gnu11".into(),
                "-O2".into(),
                "-fPIC".into(),
                "-Wall".into(),
                "-Wextra".into(),
                "-Werror".into(),
                "-c".into(),
                source.into_os_string(),
                "-o".into(),
                out.join("gaia_upatch.o").into_os_string(),
            ],
        ),
        (
            "ar",
            vec![
                "crs".into(),
                out.join("libgaia_upatch.a").into_os_string(),
                out.join("gaia_upatch.o").into_os_string(),
            ],
        ),
    ] {
        assert!(
            Command::new(cmd).args(args).status().expect(cmd).success(),
            "{cmd} failed"
        );
    }
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=gaia_upatch");
}
