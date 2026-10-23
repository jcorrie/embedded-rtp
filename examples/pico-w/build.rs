//! Puts `memory.x` where the linker finds it, and links the RP2040 boot
//! loader section and defmt.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");

    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    // Configuration baked in at build time; see README.md.
    for var in [
        "WIFI_NETWORK",
        "WIFI_PASSWORD",
        "INTERCOM_SERVER",
        "DEVICE_NAME",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}
