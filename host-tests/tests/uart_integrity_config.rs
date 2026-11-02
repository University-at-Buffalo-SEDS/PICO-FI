#[allow(dead_code)]
mod production {
    include!("../../build.rs");
    pub fn render(text:&str)->String { render_generated_config(serde_json::from_str(text).unwrap()) }
}
use std::path::Path;
#[test]
fn documented_gateway_and_plain_default_presets_choose_strict_without_flags() {
    let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for name in ["pico-fi.json","pico-fi-gateway-ethernet.json","pico-fi-gateway-ethernet-server.json"] {
        let text=std::fs::read_to_string(root.join(name)).unwrap();
        let output=production::render(&text);
        assert!(output.contains("COMPILED_UART_DATA_INTEGRITY: UartDataIntegrity = UartDataIntegrity::SedsnetCrc32"));
    }
}
#[test]
fn all_other_presets_missing_field_keep_opaque_and_transport_exact() {
    let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for entry in std::fs::read_dir(&root).unwrap() {
        let p=entry.unwrap().path();let name=p.file_name().unwrap().to_str().unwrap();
        if !name.starts_with("pico-fi")||!name.ends_with(".json")||["pico-fi.json","pico-fi-gateway-ethernet.json","pico-fi-gateway-ethernet-server.json"].contains(&name){continue;}
        let bytes=std::fs::read(&p).unwrap();
        assert!(production::render(std::str::from_utf8(&bytes).unwrap()).contains("UartDataIntegrity::Opaque"));
    }
}
#[test]
fn invalid_profile_and_non_uart_strict_fail_closed_at_build() {
    let root=Path::new(env!("CARGO_MANIFEST_DIR")).join("../pico-fi.json");
    let mut v:serde_json::Value=serde_json::from_slice(&std::fs::read(root).unwrap()).unwrap();
    v["upstream"]["data_integrity"]="typo".into();assert!(std::panic::catch_unwind(||production::render(&v.to_string())).is_err());
    v["upstream"]["data_integrity"]="sedsnet_crc32".into();v["upstream"]["transport"]="usb".into();assert!(std::panic::catch_unwind(||production::render(&v.to_string())).is_err());
    v["upstream"]["data_integrity"]="opaque".into();assert!(production::render(&v.to_string()).contains("UpstreamMode::Usb"));
}
