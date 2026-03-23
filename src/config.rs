use heapless::{String, Vec};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4Config {
    pub address: [u8; 4],
    pub prefix_len: u8,
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressMode {
    Dhcp,
    Static(Ipv4Config),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeMode {
    TcpClient { host: [u8; 4], port: u16 },
    TcpServer { port: u16 },
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamMode {
    Uart,
    Spi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BridgeConfig {
    pub address_mode: AddressMode,
    pub bridge_mode: BridgeMode,
    pub upstream_mode: UpstreamMode,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        COMPILED_CONFIG
    }
}

include!(concat!(env!("OUT_DIR"), "/generated_config.rs"));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Help,
    Show,
    Start,
    SetDhcp,
    SetStatic(Ipv4Config),
    SetClient { host: [u8; 4], port: u16 },
    SetServer { port: u16 },
}

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
        _ => Err("unknown command"),
    }
}

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
    }
}

pub fn render_config(config: &BridgeConfig) -> String<160> {
    let mut out = String::<160>::new();

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
        UpstreamMode::Spi => {
            let _ = out.push_str(" upstream=spi");
        }
    }

    out
}

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

fn parse_port(value: &str) -> Result<u16, &'static str> {

    value.parse::<u16>().map_err(|_| "invalid port")
}

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

fn push_ipv4(out: &mut String<160>, ip: [u8; 4]) {
    for (idx, octet) in ip.iter().enumerate() {
        if idx != 0 {
            let _ = out.push('.');
        }
        push_u8(out, *octet);
    }
}

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

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn parses_static() {
        let cmd = parse_command("set static 192.168.10.50/24 192.168.10.1 1.1.1.1").unwrap();
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

    #[test]
    fn parses_client() {
        let cmd = parse_command("set client 10.0.0.5 7000").unwrap();
        assert_eq!(
            cmd,
            Command::SetClient {
                host: [10, 0, 0, 5],
                port: 7000,
            }
        );
    }
}
