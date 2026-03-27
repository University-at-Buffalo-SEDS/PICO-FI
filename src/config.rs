//! Runtime bridge configuration parsing and rendering.

use heapless::{String, Vec};

/// Static IPv4 configuration values used when DHCP is disabled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4Config {
    /// Interface IPv4 address.
    pub address: [u8; 4],
    /// CIDR prefix length for the interface address.
    pub prefix_len: u8,
    /// Default gateway IPv4 address.
    pub gateway: [u8; 4],
    /// Primary DNS server IPv4 address.
    pub dns: [u8; 4],
}

/// Supported interface address assignment modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressMode {
    /// Requests an address from DHCP.
    Dhcp,
    /// Uses the explicitly provided static IPv4 settings.
    Static(Ipv4Config),
}

/// TCP bridge role that the Pico should run after startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeMode {
    /// Connects outbound to a remote TCP endpoint.
    TcpClient { host: [u8; 4], port: u16 },
    /// Listens for an inbound TCP connection.
    TcpServer { port: u16 },
}

/// Upstream physical or logical interface attached to the local device.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamMode {
    /// Uses the control UART as the upstream payload interface.
    Uart,
    /// Uses the framed I2C slave transport as the upstream payload interface.
    I2c,
    /// Uses the framed SPI slave transport as the upstream payload interface.
    Spi,
    /// Uses the SPI transport in transaction-to-transaction echo mode for diagnostics.
    SpiEcho,
    /// Uses the simple TCP test mode instead of the normal bridge protocol.
    Test,
}

/// Full runtime bridge configuration assembled from compiled defaults and shell overrides.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BridgeConfig {
    /// Ethernet MAC address presented by the W5500.
    pub mac_address: [u8; 6],
    /// IP address assignment strategy.
    pub address_mode: AddressMode,
    /// Whether the bridge should listen or connect outbound.
    pub bridge_mode: BridgeMode,
    /// Which local upstream transport should feed the TCP bridge.
    pub upstream_mode: UpstreamMode,
}

impl Default for BridgeConfig {
    /// Returns the build-generated default configuration.
    fn default() -> Self {
        COMPILED_CONFIG
    }
}

include!(concat!(env!("OUT_DIR"), "/generated_config.rs"));

/// Shell commands accepted before the bridge starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Prints the shell help text.
    Help,
    /// Renders the active configuration.
    Show,
    /// Exits the shell and starts the bridge.
    Start,
    /// Switches the interface to DHCP mode.
    SetDhcp,
    /// Switches the interface to static IPv4 mode.
    SetStatic(Ipv4Config),
    /// Selects TCP client bridge mode.
    SetClient { host: [u8; 4], port: u16 },
    /// Selects TCP server bridge mode.
    SetServer { port: u16 },
    /// Selects the upstream transport.
    SetUpstream(UpstreamMode),
    /// Restores the compiled defaults and clears any persisted override.
    Reset,
}

/// Parses a configuration shell line into a typed command.
pub fn parse_command(line: &str) -> Result<Command, &'static str> {
    let mut tokens: Vec<&str, 8> = line.split_ascii_whitespace().collect();
    if tokens.is_empty() {
        return Err("empty command");
    }

    for token in tokens.iter_mut() {
        *token = token.trim();
    }

    match tokens.as_slice() {
        ["help"] => Ok(Command::Help),
        ["show"] => Ok(Command::Show),
        ["start"] => Ok(Command::Start),
        ["set", "dhcp"] => Ok(Command::SetDhcp),
        ["set", "static", cidr, gateway, dns] => {
            Ok(Command::SetStatic(parse_static(cidr, gateway, dns)?))
        }
        ["set", "client", host, port] => Ok(Command::SetClient {
            host: parse_ipv4(host)?,
            port: parse_port(port)?,
        }),
        ["set", "server", port] => Ok(Command::SetServer {
            port: parse_port(port)?,
        }),
        ["set", "upstream", "uart"] => Ok(Command::SetUpstream(UpstreamMode::Uart)),
        ["set", "upstream", "i2c"] => Ok(Command::SetUpstream(UpstreamMode::I2c)),
        ["set", "upstream", "spi"] => Ok(Command::SetUpstream(UpstreamMode::Spi)),
        ["set", "upstream", "spi_echo"] | ["set", "upstream", "spiecho"] => {
            Ok(Command::SetUpstream(UpstreamMode::SpiEcho))
        }
        ["set", "upstream", "test"] => Ok(Command::SetUpstream(UpstreamMode::Test)),
        ["reset"] => Ok(Command::Reset),
        _ => Err("unknown command"),
    }
}

/// Applies a parsed shell command and reports whether startup should begin immediately.
pub fn apply_command(config: &mut BridgeConfig, cmd: Command) -> bool {
    match cmd {
        Command::Help | Command::Show => false,
        Command::Start => true,
        Command::SetDhcp => {
            config.address_mode = AddressMode::Dhcp;
            false
        }
        Command::SetStatic(ipv4) => {
            config.address_mode = AddressMode::Static(ipv4);
            false
        }
        Command::SetClient { host, port } => {
            config.bridge_mode = BridgeMode::TcpClient { host, port };
            false
        }
        Command::SetServer { port } => {
            config.bridge_mode = BridgeMode::TcpServer { port };
            false
        }
        Command::SetUpstream(mode) => {
            config.upstream_mode = mode;
            false
        }
        Command::Reset => false,
    }
}

/// Renders the current bridge configuration into a compact single-line summary.
pub fn render_config(config: &BridgeConfig) -> String<160> {
    let mut out = String::<160>::new();
    let _ = out.push_str("mac=");
    push_mac(&mut out, config.mac_address);
    let _ = out.push(' ');

    match config.address_mode {
        AddressMode::Dhcp => {
            let _ = out.push_str("ip=dhcp ");
        }
        AddressMode::Static(ipv4) => {
            let _ = out.push_str("ip=static ");
            push_ipv4(&mut out, ipv4.address);
            let _ = out.push('/');
            push_u8(&mut out, ipv4.prefix_len);
            let _ = out.push_str(" gw=");
            push_ipv4(&mut out, ipv4.gateway);
            let _ = out.push_str(" dns=");
            push_ipv4(&mut out, ipv4.dns);
            let _ = out.push(' ');
        }
    }

    match config.bridge_mode {
        BridgeMode::TcpClient { host, port } => {
            let _ = out.push_str("mode=client dest=");
            push_ipv4(&mut out, host);
            let _ = out.push(':');
            push_u16(&mut out, port);
        }
        BridgeMode::TcpServer { port } => {
            let _ = out.push_str("mode=server listen=");
            push_u16(&mut out, port);
        }
    }

    match config.upstream_mode {
        UpstreamMode::Uart => {
            let _ = out.push_str(" upstream=uart");
        }
        UpstreamMode::I2c => {
            let _ = out.push_str(" upstream=i2c");
        }
        UpstreamMode::Spi => {
            let _ = out.push_str(" upstream=spi");
        }
        UpstreamMode::SpiEcho => {
            let _ = out.push_str(" upstream=spi_echo");
        }
        UpstreamMode::Test => {
            let _ = out.push_str(" upstream=test");
        }
    }

    out
}

/// Parses the `a.b.c.d/prefix gateway dns` static-IP shell form.
fn parse_static(cidr: &str, gateway: &str, dns: &str) -> Result<Ipv4Config, &'static str> {
    let (addr, prefix) = cidr
        .split_once('/')
        .ok_or("expected cidr form a.b.c.d/prefix")?;
    let prefix_len = prefix.parse::<u8>().map_err(|_| "invalid prefix")?;
    if prefix_len > 32 {
        return Err("invalid prefix");
    }

    Ok(Ipv4Config {
        address: parse_ipv4(addr)?,
        prefix_len,
        gateway: parse_ipv4(gateway)?,
        dns: parse_ipv4(dns)?,
    })
}

/// Parses a decimal TCP or UDP port.
fn parse_port(value: &str) -> Result<u16, &'static str> {
    value.parse::<u16>().map_err(|_| "invalid port")
}

/// Parses a dotted-quad IPv4 address.
fn parse_ipv4(value: &str) -> Result<[u8; 4], &'static str> {
    let mut octets = [0u8; 4];
    let mut count = 0usize;

    for (idx, chunk) in value.split('.').enumerate() {
        if idx >= 4 {
            return Err("invalid ipv4 address");
        }
        octets[idx] = chunk.parse::<u8>().map_err(|_| "invalid ipv4 address")?;
        count += 1;
    }

    if count != 4 {
        return Err("invalid ipv4 address");
    }

    Ok(octets)
}

/// Appends a dotted-quad IPv4 address to the config render buffer.
fn push_ipv4(out: &mut String<160>, ip: [u8; 4]) {
    for (idx, octet) in ip.iter().enumerate() {
        if idx != 0 {
            let _ = out.push('.');
        }
        push_u8(out, *octet);
    }
}

/// Appends an unsigned 8-bit integer in decimal form.
fn push_u8(out: &mut String<160>, value: u8) {
    let mut digits = [0u8; 3];
    let mut n = value;
    let mut used = 0usize;

    loop {
        digits[used] = b'0' + (n % 10);
        used += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }

    for idx in (0..used).rev() {
        let _ = out.push(digits[idx] as char);
    }
}

/// Appends an unsigned 16-bit integer in decimal form.
fn push_u16(out: &mut String<160>, value: u16) {
    let mut digits = [0u8; 5];
    let mut n = value;
    let mut used = 0usize;

    loop {
        digits[used] = b'0' + ((n % 10) as u8);
        used += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }

    for idx in (0..used).rev() {
        let _ = out.push(digits[idx] as char);
    }
}

/// Appends a colon-separated MAC address to the config render buffer.
fn push_mac(out: &mut String<160>, mac: [u8; 6]) {
    for (idx, byte) in mac.iter().enumerate() {
        if idx != 0 {
            let _ = out.push(':');
        }
        push_hex_byte(out, *byte);
    }
}

/// Appends a two-character lowercase hexadecimal byte.
fn push_hex_byte(out: &mut String<160>, value: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let _ = out.push(HEX[(value >> 4) as usize] as char);
    let _ = out.push(HEX[(value & 0x0f) as usize] as char);
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    /// Verifies the static-IP shell command parser.
    #[test]
    fn parses_static() {
        let cmd = parse_command("set static 192.168.10.50/24 192.168.10.1 1.1.1.1")
            .expect("static config command should parse");
        assert_eq!(
            cmd,
            Command::SetStatic(Ipv4Config {
                address: [192, 168, 10, 50],
                prefix_len: 24,
                gateway: [192, 168, 10, 1],
                dns: [1, 1, 1, 1],
            })
        );
    }

    /// Verifies the client-mode shell command parser.
    #[test]
    fn parses_client() {
        let cmd = parse_command("set client 10.0.0.5 7000")
            .expect("client config command should parse");
        assert_eq!(
            cmd,
            Command::SetClient {
                host: [10, 0, 0, 5],
                port: 7000,
            }
        );
    }

    /// Verifies the upstream-mode shell command parser.
    #[test]
    fn parses_upstream_test() {
        let cmd = parse_command("set upstream test")
            .expect("upstream mode command should parse");
        assert_eq!(cmd, Command::SetUpstream(UpstreamMode::Test));
    }

    /// Verifies the upstream-mode shell command parser accepts I2C.
    #[test]
    fn parses_upstream_i2c() {
        let cmd = parse_command("set upstream i2c")
            .expect("upstream mode command should parse");
        assert_eq!(cmd, Command::SetUpstream(UpstreamMode::I2c));
    }

    /// Verifies the upstream-mode shell command accepts SPI.
    #[test]
    fn parses_upstream_spi() {
        let cmd = parse_command("set upstream spi")
            .expect("upstream mode command should parse");
        assert_eq!(cmd, Command::SetUpstream(UpstreamMode::Spi));
    }
}
