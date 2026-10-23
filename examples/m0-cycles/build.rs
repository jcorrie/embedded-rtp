//! Put `memory.x` where the linker finds it, and link with cortex-m-rt's
//! script. Here rather than in `.cargo/config.toml`, where a `RUSTFLAGS`
//! environment variable (as CI sets) would override it.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
}
