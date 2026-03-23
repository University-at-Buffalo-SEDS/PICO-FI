use std::env;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Deserialize)]
struct JsonConfig {
    network: JsonNetworkConfig,
    bridge: JsonBridgeConfig,
    upstream: JsonUpstreamConfig,
}

#[derive(Deserialize)]
struct JsonNetworkConfig {
    mode: String,
    ip: Option<String>,
    prefix_len: Option<u8>,
    gateway: Option<String>,
    dns: Option<String>,
}

#[derive(Deserialize)]
struct JsonBridgeConfig {
    role: String,
    listen_port: Option<u16>,
    remote_ip: Option<String>,
    remote_port: Option<u16>,
}

#[derive(Deserialize)]
struct JsonUpstreamConfig {
    transport: String,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=pico-fi.json");
    println!("cargo:rerun-if-changed=scripts/build-uf2.sh");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    fs::copy("memory.x", out_dir.join("memory.x")).expect("failed to copy memory.x to OUT_DIR");
    let generated = render_generated_config(load_json_config());
    fs::write(out_dir.join("generated_config.rs"), generated)
        .expect("failed to write generated config");

    println!("cargo:rustc-link-search={}", out_dir.display());
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rustc-link-arg=-Tlink-rp.x");

    if env::var("TARGET").as_deref() == Ok("thumbv6m-none-eabi") {
        println!(
            "cargo:warning=UF2 generation is handled by scripts/build-uf2.sh after linking."
        );
    }
}

fn load_json_config() -> JsonConfig {
    let text = fs::read_to_string("pico-fi.json").expect("failed to read pico-fi.json");
    serde_json::from_str(&text).expect("failed to parse pico-fi.json")
}

fn render_generated_config(config: JsonConfig) -> String {
    let address_mode = render_address_mode(&config.network);
    let bridge_mode = render_bridge_mode(&config.bridge);
    let upstream = render_upstream_mode(&config.upstream);

    format!(
        "pub const COMPILED_CONFIG: BridgeConfig = BridgeConfig {{\n    address_mode: {address_mode},\n    bridge_mode: {bridge_mode},\n    upstream_mode: {upstream},\n}};\n"
    )
}

fn render_address_mode(network: &JsonNetworkConfig) -> String {
    match network.mode.as_str() {
        "dhcp" => "AddressMode::Dhcp".to_owned(),
        "static" => {
            let ip = parse_ipv4(network.ip.as_deref(), "network.ip");
            let gateway = parse_ipv4(network.gateway.as_deref(), "network.gateway");
            let dns = parse_ipv4(network.dns.as_deref(), "network.dns");
            let prefix_len = network
                .prefix_len
                .expect("network.prefix_len is required for static mode");

            format!(
                "AddressMode::Static(Ipv4Config {{ address: {ip}, prefix_len: {prefix_len}, gateway: {gateway}, dns: {dns} }})"
            )
        }
        other => panic!("unsupported network.mode: {other}"),
    }
}

fn render_bridge_mode(bridge: &JsonBridgeConfig) -> String {
    match bridge.role.as_str() {
        "server" => {
            let port = bridge
                .listen_port
                .expect("bridge.listen_port is required for server role");
            format!("BridgeMode::TcpServer {{ port: {port} }}")
        }
        "client" => {
            let host = parse_ipv4(bridge.remote_ip.as_deref(), "bridge.remote_ip");
            let port = bridge
                .remote_port
                .expect("bridge.remote_port is required for client role");
            format!("BridgeMode::TcpClient {{ host: {host}, port: {port} }}")
        }
        other => panic!("unsupported bridge.role: {other}"),
    }
}

fn render_upstream_mode(upstream: &JsonUpstreamConfig) -> String {
    match upstream.transport.as_str() {
        "uart" => "UpstreamMode::Uart".to_owned(),
        "spi" => "UpstreamMode::Spi".to_owned(),
        other => panic!("unsupported upstream.transport: {other}"),
    }
}

fn parse_ipv4(value: Option<&str>, field: &str) -> String {
    let value = value.unwrap_or_else(|| panic!("{field} is required"));
    let mut octets = [0u8; 4];
    let mut count = 0usize;

    for (idx, chunk) in value.split('.').enumerate() {
        assert!(idx < 4, "invalid ipv4 address in {field}");
        octets[idx] = chunk
            .parse::<u8>()
            .unwrap_or_else(|_| panic!("invalid ipv4 address in {field}"));
        count += 1;
    }

    assert!(count == 4, "invalid ipv4 address in {field}");
    format!("[{}, {}, {}, {}]", octets[0], octets[1], octets[2], octets[3])
}
