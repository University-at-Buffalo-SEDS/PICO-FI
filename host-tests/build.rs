#[allow(dead_code)]
mod firmware_build {
    include!("../build.rs");
    pub fn generate() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pico-fi-gateway-ethernet.json");
        println!("cargo:rerun-if-changed={}", source.display());
        println!("cargo:rerun-if-changed=../build.rs");
        let config: JsonConfig =
            serde_json::from_str(&std::fs::read_to_string(source).unwrap()).unwrap();
        let generated = format!(
            "{}\npub const COMPILED_UART_BAUD_RATE:u32=1000000;\n",
            render_generated_config(config)
        );
        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        std::fs::write(out.join("generated_config.rs"), generated).unwrap();
    }
}
fn main() {
    firmware_build::generate();
}
