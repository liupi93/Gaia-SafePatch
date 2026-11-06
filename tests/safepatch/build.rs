use std::{env, fs, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=../../.git/refs/heads/base");
    let output = Command::new("git")
        .args(["show", "base:gaia-xdp/src/main.rs"])
        .output()
        .expect("read base branch");
    assert!(
        output.status.success(),
        "create the original base branch before comparison"
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let start = text
        .find("fn ptrace_read_and_write(")
        .expect("original writer");
    let end = text[start..].find("\n}\n").unwrap() + start + 3;
    let source = format!(
        "use anyhow::{{Result, anyhow}};\nmacro_rules! warn {{ ($($t:tt)*) => {{ eprintln!($($t)*); }} }}\n{}\npub fn exchange(pid:u32,address:u64,data:&[u8])->Result<Vec<u8>> {{ ptrace_read_and_write(pid,address,data) }}\n",
        &text[start..end]
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("base.rs"),
        source,
    )
    .unwrap();
}
