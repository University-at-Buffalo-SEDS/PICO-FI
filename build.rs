use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=scripts/build-uf2.sh");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    fs::copy("memory.x", out_dir.join("memory.x")).expect("failed to copy memory.x to OUT_DIR");

    println!("cargo:rustc-link-search={}", out_dir.display());
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rustc-link-arg=-Tlink-rp.x");

    if env::var("TARGET").as_deref() == Ok("thumbv6m-none-eabi") {
        println!(
            "cargo:warning=UF2 generation is handled by scripts/build-uf2.sh after linking."
        );
    }
}
