//! Kernel command-line parser (design §12, §9 Config, §3.2).
//!
//! ```text
//! console=ttyS0 ip=10.0.0.2::10.0.0.1:255.255.255.0:vm:eth0:off:1.1.1.1 \
//!   patina.epoch=1760000000 patina.env=HOME=/root,MSG=hello%20world \
//!   virtio_mmio.device=4K@0xd0000000:5 quiet -- --port 8080
//! ```
//!
//! - Parameters are whitespace-separated; `"` toggles quoting (so a quoted
//!   value may contain spaces) and is removed, like Linux `next_arg`.
//! - Everything after the first standalone `--` is the program's argv
//!   (after `/app`), one element per token, EXCEPT tokens containing a raw
//!   `=`: the CLI always percent-encodes `=` in real args (`%3D`), so a
//!   post-`--` token with a raw `=` is a kernel parameter and is parsed
//!   exactly like one before `--` (not added to argv). This is how QEMU
//!   microvm's appended ` virtio_mmio.device=...` and
//!   `run-qemu.sh -- patina.selftest=panic` reach the kernel (doc/decisions.md
//!   D11, doc/cli-contract.md). A later bare `--` is an ordinary arg.
//! - Percent-decoding (`%XX`) applies to argv elements, `patina.env` keys
//!   and values, and `patina.*` values. `+` is literal. An invalid escape
//!   is kept verbatim. The CLI must encode at least space, `"`, `%`, and,
//!   inside `patina.env`, `,` (and `=` in keys).
//! - `ip=` follows Linux `Documentation/admin-guide/nfs/nfsroot.rst`:
//!   `ip=<client>:<server>:<gw>:<netmask>:<hostname>:<device>:<autoconf>:<dns0>:<dns1>:<ntp0>`
//!   or a bare autoconf keyword (`dhcp`, `off`, ...).
//! - `virtio_mmio.device=[<size>@]<base>:<irq>[:<id>]` (may repeat), numbers
//!   in C `strtoul` base-0 syntax, `size`/`base` accepting `K/M/G/...`
//!   suffixes like Linux `memparse`.
//! - Anything else (including unknown `patina.*` keys and bare flags such
//!   as `quiet`) is preserved in [`Cmdline::unknown`], raw.
//!
//! Parsing never fails: problems with known keys are collected in
//! [`Cmdline::errors`] and the offending parameter is otherwise ignored.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

/// A raw `key[=value]` parameter (quotes removed, not percent-decoded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub key: String,
    pub value: Option<String>,
}

/// A problem with a recognised parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamError {
    pub key: String,
    pub value: String,
    pub reason: &'static str,
}

/// `<autoconf>` field of `ip=`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Autoconf {
    /// Field absent or empty: static if a client address was given,
    /// otherwise the kernel's choice.
    #[default]
    Unspecified,
    /// `off` / `none`: static configuration only.
    Off,
    /// `on` / `any`.
    Any,
    Dhcp,
    Bootp,
    Rarp,
    Both,
}

impl Autoconf {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "" => Autoconf::Unspecified,
            "off" | "none" => Autoconf::Off,
            "on" | "any" => Autoconf::Any,
            "dhcp" => Autoconf::Dhcp,
            "bootp" => Autoconf::Bootp,
            "rarp" => Autoconf::Rarp,
            "both" => Autoconf::Both,
            _ => return None,
        })
    }

    /// Whether this asks for DHCP (`dhcp`, `on`/`any`, or unspecified with
    /// no client address — the caller decides the latter).
    pub fn wants_dhcp(self) -> bool {
        matches!(
            self,
            Autoconf::Dhcp | Autoconf::Any | Autoconf::Both | Autoconf::Bootp
        )
    }
}

/// Parsed `ip=` parameter.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct IpConfig {
    pub client: Option<Ipv4Addr>,
    pub server: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub netmask: Option<Ipv4Addr>,
    pub hostname: Option<String>,
    pub device: Option<String>,
    pub autoconf: Autoconf,
    pub dns: [Option<Ipv4Addr>; 2],
    pub ntp: Option<Ipv4Addr>,
}

impl IpConfig {
    /// Parse the value of `ip=`.
    pub fn parse(v: &str) -> Result<Self, &'static str> {
        if !v.contains(':') {
            let autoconf = Autoconf::parse(v).ok_or("unknown ip= autoconf method")?;
            return Ok(IpConfig {
                autoconf,
                ..Default::default()
            });
        }
        let fields: Vec<&str> = v.split(':').collect();
        if fields.len() > 10 {
            return Err("too many ip= fields");
        }
        let get = |i: usize| fields.get(i).copied().unwrap_or("");
        let addr = |i: usize| -> Result<Option<Ipv4Addr>, &'static str> {
            let f = get(i);
            if f.is_empty() {
                Ok(None)
            } else {
                f.parse().map(Some).map_err(|_| "bad IPv4 address in ip=")
            }
        };
        let text = |i: usize| {
            let f = get(i);
            if f.is_empty() {
                None
            } else {
                Some(f.to_owned())
            }
        };
        let cfg = IpConfig {
            client: addr(0)?,
            server: addr(1)?,
            gateway: addr(2)?,
            netmask: addr(3)?,
            hostname: text(4),
            device: text(5),
            autoconf: Autoconf::parse(get(6)).ok_or("unknown ip= autoconf method")?,
            dns: [addr(7)?, addr(8)?],
            ntp: addr(9)?,
        };
        if cfg.netmask.is_some() && cfg.prefix_len().is_none() {
            return Err("non-contiguous netmask in ip=");
        }
        Ok(cfg)
    }

    /// Prefix length of `netmask`, if present and contiguous.
    pub fn prefix_len(&self) -> Option<u8> {
        let m = u32::from(self.netmask?);
        let ones = m.leading_ones();
        if m.checked_shl(ones).unwrap_or(0) == 0 {
            Some(ones as u8)
        } else {
            None
        }
    }

    /// True when this describes a static configuration (client address
    /// given and autoconf not explicitly requested).
    pub fn is_static(&self) -> bool {
        self.client.is_some() && matches!(self.autoconf, Autoconf::Off | Autoconf::Unspecified)
    }
}

/// One `virtio_mmio.device=` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioMmioDevice {
    /// Region size (Linux default when omitted: 512 bytes; we report `None`).
    pub size: Option<u64>,
    pub base: u64,
    pub irq: u32,
    pub id: Option<u32>,
}

impl VirtioMmioDevice {
    pub fn parse(v: &str) -> Result<Self, &'static str> {
        let (size, rest) = match v.split_once('@') {
            Some((s, r)) => (Some(memparse(s).ok_or("bad virtio_mmio size")?), r),
            None => (None, v),
        };
        let mut parts = rest.split(':');
        let base = memparse(parts.next().unwrap_or("")).ok_or("bad virtio_mmio base address")?;
        let irq = parse_c_uint(parts.next().ok_or("missing virtio_mmio irq")?)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("bad virtio_mmio irq")?;
        let id = match parts.next() {
            None => None,
            Some(s) => Some(
                parse_c_uint(s)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or("bad virtio_mmio id")?,
            ),
        };
        if parts.next().is_some() {
            return Err("trailing data in virtio_mmio.device");
        }
        if size == Some(0) {
            return Err("zero virtio_mmio size");
        }
        Ok(VirtioMmioDevice {
            size,
            base,
            irq,
            id,
        })
    }
}

/// Parse an unsigned integer in C `strtoul(s, NULL, 0)` syntax: `0x`
/// prefix for hex, leading `0` for octal, else decimal. Whole string must
/// be consumed.
pub fn parse_c_uint(s: &str) -> Option<u64> {
    let (digits, radix) = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (h, 16)
    } else if s.len() > 1 && s.starts_with('0') {
        (&s[1..], 8)
    } else {
        (s, 10)
    };
    if digits.is_empty() || !digits.bytes().all(|b| (b as char).is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

/// Linux `memparse`: [`parse_c_uint`] followed by an optional
/// `K/M/G/T/P/E` suffix (case-insensitive, binary multiples).
pub fn memparse(s: &str) -> Option<u64> {
    let (num, shift) = match s.as_bytes().last()? {
        b'k' | b'K' => (&s[..s.len() - 1], 10),
        b'm' | b'M' => (&s[..s.len() - 1], 20),
        b'g' | b'G' => (&s[..s.len() - 1], 30),
        b't' | b'T' => (&s[..s.len() - 1], 40),
        b'p' | b'P' => (&s[..s.len() - 1], 50),
        b'e' | b'E' if !s.starts_with("0x") && !s.starts_with("0X") => (&s[..s.len() - 1], 60),
        _ => (s, 0),
    };
    let n = parse_c_uint(num)?;
    n.checked_mul(1u64 << shift)
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode `s` to bytes. Invalid `%` sequences are kept verbatim.
pub fn percent_decode_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Percent-decode `s` to a `String`; non-UTF-8 results are converted
/// lossily (`U+FFFD`).
pub fn percent_decode(s: &str) -> String {
    match String::from_utf8(percent_decode_bytes(s)) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// Split a command line into tokens the way Linux `next_arg` does:
/// whitespace separates, `"` toggles quoting and is dropped.
pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut in_quote = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                in_token = true;
            }
            c if c.is_ascii_whitespace() && !in_quote => {
                if in_token {
                    out.push(core::mem::take(&mut cur));
                    in_token = false;
                }
            }
            c => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

/// The parsed command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cmdline {
    /// Every `console=` value, in order (raw, e.g. `ttyS0,115200`).
    pub console: Vec<String>,
    /// Last valid `ip=`.
    pub ip: Option<IpConfig>,
    /// `patina.epoch=<sec>[.<frac>]` as nanoseconds since the Unix epoch.
    pub epoch_ns: Option<u64>,
    /// `patina.env` pairs in order (later duplicates are kept; the consumer
    /// decides, normally last wins).
    pub env: Vec<(String, String)>,
    /// Program arguments after `--` (not including argv\[0\]).
    pub argv: Vec<String>,
    /// Whether a `--` separator was present.
    pub has_separator: bool,
    /// `virtio_mmio.device=` entries in order.
    pub virtio_mmio: Vec<VirtioMmioDevice>,
    /// Unrecognised parameters, raw, in order.
    pub unknown: Vec<Param>,
    /// Problems with recognised parameters.
    pub errors: Vec<ParamError>,
}

/// Parse `patina.epoch` (`sec` or `sec.frac`, up to 9 fractional digits).
pub fn parse_epoch_ns(v: &str) -> Option<u64> {
    let (sec, frac) = match v.split_once('.') {
        Some((s, f)) => (s, f),
        None => (v, ""),
    };
    if sec.is_empty() || !sec.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if frac.len() > 9
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || (v.contains('.') && frac.is_empty())
    {
        return None;
    }
    let s: u64 = sec.parse().ok()?;
    let mut ns: u64 = 0;
    for (i, d) in frac.bytes().enumerate() {
        ns += (d - b'0') as u64 * 10u64.pow(8 - i as u32);
    }
    s.checked_mul(1_000_000_000)?.checked_add(ns)
}

impl Cmdline {
    /// Parse a full kernel command line. Never fails; see [`Cmdline::errors`].
    pub fn parse(s: &str) -> Cmdline {
        let mut c = Cmdline::default();
        for tok in tokenize(s) {
            if !c.has_separator && tok == "--" {
                c.has_separator = true;
                continue;
            }
            if c.has_separator && !tok.contains('=') {
                c.argv.push(percent_decode(&tok));
                continue;
            }
            let (key, value) = match tok.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (tok.as_str(), None),
            };
            c.param(key, value);
        }
        c
    }

    fn error(&mut self, key: &str, value: &str, reason: &'static str) {
        self.errors.push(ParamError {
            key: key.to_owned(),
            value: value.to_owned(),
            reason,
        });
    }

    fn param(&mut self, key: &str, value: Option<&str>) {
        let v = value.unwrap_or("");
        match (key, value) {
            ("console", Some(v)) => self.console.push(v.to_owned()),
            ("ip", Some(v)) => match IpConfig::parse(v) {
                Ok(ip) => self.ip = Some(ip),
                Err(e) => self.error(key, v, e),
            },
            ("patina.epoch", Some(v)) => match parse_epoch_ns(&percent_decode(v)) {
                Some(ns) => self.epoch_ns = Some(ns),
                None => self.error(key, v, "bad epoch"),
            },
            ("patina.env", Some(v)) => {
                for pair in v.split(',').filter(|p| !p.is_empty()) {
                    match pair.split_once('=') {
                        Some((k, val)) if !k.is_empty() => {
                            self.env.push((percent_decode(k), percent_decode(val)));
                        }
                        _ => self.error(key, pair, "env entry is not K=V"),
                    }
                }
            }
            ("virtio_mmio.device", Some(v)) => match VirtioMmioDevice::parse(v) {
                Ok(d) => self.virtio_mmio.push(d),
                Err(e) => self.error(key, v, e),
            },
            ("console" | "ip" | "patina.epoch" | "patina.env" | "virtio_mmio.device", None) => {
                self.error(key, v, "missing value")
            }
            _ => self.unknown.push(Param {
                key: key.to_owned(),
                value: value.map(|v| v.to_owned()),
            }),
        }
    }

    /// Look up an unrecognised parameter: `Some(None)` for a bare flag,
    /// `Some(Some(v))` for `key=v` (last occurrence wins), `None` if absent.
    pub fn get(&self, key: &str) -> Option<Option<&str>> {
        self.unknown
            .iter()
            .rev()
            .find(|p| p.key == key)
            .map(|p| p.value.as_deref())
    }

    /// Last `console=` value split into device name and options
    /// (`ttyS0,115200n8` → `("ttyS0", Some("115200n8"))`).
    pub fn primary_console(&self) -> Option<(&str, Option<&str>)> {
        let c = self.console.last()?;
        Some(match c.split_once(',') {
            Some((n, o)) => (n, Some(o)),
            None => (c.as_str(), None),
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use alloc::vec;

    use super::*;

    fn ip(a: [u8; 4]) -> Option<Ipv4Addr> {
        Some(Ipv4Addr::from(a))
    }

    #[test]
    fn full_example() {
        let c = Cmdline::parse(
            "console=ttyS0,115200 ip=10.0.0.2::10.0.0.1:255.255.255.0:vm:eth0:off:1.1.1.1:8.8.8.8 \
             patina.epoch=1760000000 patina.env=HOME=/root,MSG=hello%20world,EMPTY= \
             virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=0x1000@0xd0001000:6:2 \
             quiet loglevel=3 patina.strace -- --port 8080 a%20b \"quoted arg\" 100%",
        );
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.console, vec!["ttyS0,115200"]);
        assert_eq!(c.primary_console(), Some(("ttyS0", Some("115200"))));
        let i = c.ip.as_ref().unwrap();
        assert_eq!(i.client, ip([10, 0, 0, 2]));
        assert_eq!(i.server, None);
        assert_eq!(i.gateway, ip([10, 0, 0, 1]));
        assert_eq!(i.netmask, ip([255, 255, 255, 0]));
        assert_eq!(i.prefix_len(), Some(24));
        assert_eq!(i.hostname.as_deref(), Some("vm"));
        assert_eq!(i.device.as_deref(), Some("eth0"));
        assert_eq!(i.autoconf, Autoconf::Off);
        assert_eq!(i.dns, [ip([1, 1, 1, 1]), ip([8, 8, 8, 8])]);
        assert!(i.is_static());
        assert_eq!(c.epoch_ns, Some(1_760_000_000_000_000_000));
        assert_eq!(
            c.env,
            vec![
                ("HOME".into(), "/root".into()),
                ("MSG".into(), "hello world".into()),
                ("EMPTY".into(), "".into())
            ]
        );
        assert_eq!(
            c.virtio_mmio,
            vec![
                VirtioMmioDevice {
                    size: Some(4096),
                    base: 0xd000_0000,
                    irq: 5,
                    id: None
                },
                VirtioMmioDevice {
                    size: Some(0x1000),
                    base: 0xd000_1000,
                    irq: 6,
                    id: Some(2)
                },
            ]
        );
        assert_eq!(c.get("quiet"), Some(None));
        assert_eq!(c.get("loglevel"), Some(Some("3")));
        assert_eq!(c.get("patina.strace"), Some(None));
        assert_eq!(c.get("nope"), None);
        assert!(c.has_separator);
        assert_eq!(c.argv, vec!["--port", "8080", "a b", "quoted arg", "100%"]);
    }

    #[test]
    fn argv_only_after_separator() {
        let c = Cmdline::parse("console=hvc0 -- -- x%3Dy");
        assert_eq!(c.argv, vec!["--", "x=y"]);
        assert!(c.unknown.is_empty());
        let c = Cmdline::parse("console=hvc0");
        assert!(!c.has_separator);
        assert!(c.argv.is_empty());
        let c = Cmdline::parse("  ");
        assert_eq!(c, Cmdline::default());
        let c = Cmdline::parse("--");
        assert!(c.has_separator && c.argv.is_empty());
    }

    #[test]
    fn raw_equals_after_separator_is_param() {
        let c = Cmdline::parse("console=ttyS0 -- a virtio_mmio.device=512@0xfeb00e00:12 b");
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.argv, vec!["a", "b"]);
        assert_eq!(
            c.virtio_mmio,
            vec![VirtioMmioDevice {
                size: Some(512),
                base: 0xfeb0_0e00,
                irq: 12,
                id: None
            }]
        );

        let c = Cmdline::parse("-- x%3Dy");
        assert_eq!(c.argv, vec!["x=y"]);
        assert!(c.unknown.is_empty());

        let c = Cmdline::parse("console=ttyAMA0 -- patina.selftest=panic");
        assert_eq!(c.get("patina.selftest"), Some(Some("panic")));
        assert!(c.argv.is_empty());

        let c = Cmdline::parse("-- patina.strace=1");
        assert_eq!(c.get("patina.strace"), Some(Some("1")));
        assert!(c.argv.is_empty());
        let c = Cmdline::parse("patina.strace --");
        assert_eq!(c.get("patina.strace"), Some(None));
        assert!(c.argv.is_empty());

        // A second `--` is an arg; a raw-`=` token after it is still a param.
        let c = Cmdline::parse("-- -- k=v --");
        assert_eq!(c.argv, vec!["--", "--"]);
        assert_eq!(c.get("k"), Some(Some("v")));
    }

    #[test]
    fn ip_forms() {
        assert_eq!(IpConfig::parse("dhcp").unwrap().autoconf, Autoconf::Dhcp);
        assert_eq!(IpConfig::parse("off").unwrap().autoconf, Autoconf::Off);
        assert_eq!(IpConfig::parse("none").unwrap().autoconf, Autoconf::Off);
        assert_eq!(IpConfig::parse("on").unwrap().autoconf, Autoconf::Any);
        assert!(IpConfig::parse("dhcp").unwrap().autoconf.wants_dhcp());
        assert!(IpConfig::parse("bogus").is_err());
        // Minimal static: client only, trailing fields absent.
        let i = IpConfig::parse("192.168.1.5:").unwrap();
        assert_eq!(i.client, ip([192, 168, 1, 5]));
        assert_eq!(i.autoconf, Autoconf::Unspecified);
        assert!(i.is_static());
        // All empty but autoconf dhcp.
        let i = IpConfig::parse("::::::dhcp").unwrap();
        assert_eq!(
            i,
            IpConfig {
                autoconf: Autoconf::Dhcp,
                ..Default::default()
            }
        );
        // Full with ntp.
        let i = IpConfig::parse("1.2.3.4:5.6.7.8:1.2.3.1:255.255.0.0:h:eth1:none:9.9.9.9::4.4.4.4")
            .unwrap();
        assert_eq!(i.server, ip([5, 6, 7, 8]));
        assert_eq!(i.dns, [ip([9, 9, 9, 9]), None]);
        assert_eq!(i.ntp, ip([4, 4, 4, 4]));
        assert_eq!(i.prefix_len(), Some(16));
        // Errors.
        assert!(IpConfig::parse("1.2.3:").is_err());
        assert!(IpConfig::parse("1.2.3.4:::255.0.255.0").is_err());
        assert!(IpConfig::parse("1.2.3.4::::::bogus").is_err());
        assert!(IpConfig::parse("1:2:3:4:5:6:off:8:9:10:11").is_err());
        assert_eq!(
            IpConfig::parse("1.2.3.4:::0.0.0.0").unwrap().prefix_len(),
            Some(0)
        );
        assert_eq!(
            IpConfig::parse("1.2.3.4:::255.255.255.255")
                .unwrap()
                .prefix_len(),
            Some(32)
        );
    }

    #[test]
    fn bad_known_params_reported() {
        let c = Cmdline::parse(
            "ip=1.2.3 patina.epoch=soon patina.env=NOEQ,A=1,=x virtio_mmio.device=zz console",
        );
        let keys: Vec<(&str, &str)> = c
            .errors
            .iter()
            .map(|e| (e.key.as_str(), e.value.as_str()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("ip", "1.2.3"),
                ("patina.epoch", "soon"),
                ("patina.env", "NOEQ"),
                ("patina.env", "=x"),
                ("virtio_mmio.device", "zz"),
                ("console", "")
            ]
        );
        assert_eq!(c.env, vec![("A".into(), "1".into())]);
        assert!(c.ip.is_none() && c.epoch_ns.is_none() && c.virtio_mmio.is_empty());
    }

    #[test]
    fn virtio_mmio() {
        let d = VirtioMmioDevice::parse("512@0xfeb00000:12").unwrap();
        assert_eq!(
            d,
            VirtioMmioDevice {
                size: Some(512),
                base: 0xfeb0_0000,
                irq: 12,
                id: None
            }
        );
        let d = VirtioMmioDevice::parse("0x200@0xa000000:48:0").unwrap();
        assert_eq!(d.id, Some(0));
        let d = VirtioMmioDevice::parse("1M@3G:010").unwrap();
        assert_eq!((d.size, d.base, d.irq), (Some(1 << 20), 3 << 30, 8));
        assert_eq!(VirtioMmioDevice::parse("0xd0000000:5").unwrap().size, None);
        for bad in [
            "",
            "4K@",
            "4K@0xd0000000",
            "4K@0xd0000000:",
            "4K@0xd0000000:x",
            "0@1:2",
            "4K@1:2:3:4",
            "4K@1:99999999999",
        ] {
            assert!(VirtioMmioDevice::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_c_uint("0"), Some(0));
        assert_eq!(parse_c_uint("10"), Some(10));
        assert_eq!(parse_c_uint("010"), Some(8));
        assert_eq!(parse_c_uint("0x1F"), Some(31));
        assert_eq!(parse_c_uint("0x"), None);
        assert_eq!(parse_c_uint("09"), None);
        assert_eq!(parse_c_uint("-1"), None);
        assert_eq!(parse_c_uint("99999999999999999999"), None);
        assert_eq!(memparse("4k"), Some(4096));
        assert_eq!(memparse("2G"), Some(2 << 30));
        assert_eq!(memparse("0x1e"), Some(0x1e));
        assert_eq!(memparse("1E"), Some(1 << 60));
        assert_eq!(memparse("16E"), None);
        assert_eq!(memparse(""), None);
        assert_eq!(memparse("K"), None);
    }

    #[test]
    fn epoch() {
        assert_eq!(parse_epoch_ns("0"), Some(0));
        assert_eq!(parse_epoch_ns("1.5"), Some(1_500_000_000));
        assert_eq!(parse_epoch_ns("1.000000001"), Some(1_000_000_001));
        assert_eq!(parse_epoch_ns("1."), None);
        assert_eq!(parse_epoch_ns(".5"), None);
        assert_eq!(parse_epoch_ns("1.0000000001"), None);
        assert_eq!(parse_epoch_ns("-1"), None);
        assert_eq!(parse_epoch_ns("99999999999999999999"), None);
    }

    #[test]
    fn percent() {
        assert_eq!(percent_decode("a%20b%2C%3d"), "a b,=");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%2"), "%2");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
        assert_eq!(percent_decode("a+b"), "a+b");
        assert_eq!(percent_decode("%C3%A9"), "é");
        assert_eq!(percent_decode("%FF"), "\u{fffd}");
        assert_eq!(percent_decode_bytes("%FF%00"), vec![0xff, 0]);
    }

    #[test]
    fn tokenizer() {
        assert_eq!(tokenize("a  b\tc\n"), vec!["a", "b", "c"]);
        assert_eq!(tokenize("k=\"x y\" \"z w\""), vec!["k=x y", "z w"]);
        assert_eq!(tokenize("\"\""), vec![""]);
        assert_eq!(tokenize("\"unterminated x"), vec!["unterminated x"]);
    }

    #[test]
    fn never_panics() {
        // Cheap fuzz over a small alphabet.
        let alphabet = [
            'a', '=', ':', ',', '%', '2', '0', 'x', '@', '-', ' ', '"', '.', 'K', 'é',
        ];
        let keys = [
            "ip=",
            "virtio_mmio.device=",
            "patina.env=",
            "patina.epoch=",
            "console=",
            "",
            "-- ",
        ];
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut rnd = |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        for _ in 0..20_000 {
            let mut s = String::new();
            for _ in 0..rnd(4) {
                s.push_str(keys[rnd(keys.len())]);
                for _ in 0..rnd(16) {
                    s.push(alphabet[rnd(alphabet.len())]);
                }
                s.push(' ');
            }
            let _ = Cmdline::parse(&s);
        }
    }
}
