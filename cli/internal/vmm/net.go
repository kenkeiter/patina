package vmm

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"net/netip"
	"strings"
)

// TapKernelParams returns the ip= kernel parameter for tap networking:
// ip=dhcp, or the static configuration from --ip (cfg.Net.IP). With no
// network it returns nothing (and rejects --ip). Callers handle their own
// non-tap modes (QEMU "user") before calling it.
func TapKernelParams(cfg *Config) ([]string, error) {
	switch cfg.Net.Mode {
	case "", "none":
		if cfg.Net.IP != "" {
			return nil, errors.New("--ip needs --net <tap device>")
		}
		return nil, nil
	}
	if cfg.Net.IP == "" {
		return []string{"ip=dhcp"}, nil
	}
	v, err := StaticIP(cfg.Net.IP)
	if err != nil {
		return nil, err
	}
	return []string{"ip=" + v}, nil
}

// StaticIP turns a --ip value into a Linux nfsroot-style ip= value
// (<client>:<server>:<gw>:<netmask>:<hostname>:<device>:<autoconf>:<dns0>).
// Accepted forms:
//
//	ADDR/PREFIX[,GATEWAY[,DNS]]   e.g. 172.16.0.2/24,172.16.0.1,1.1.1.1
//	                              -> 172.16.0.2::172.16.0.1:255.255.255.0:patina:eth0:off:1.1.1.1
//	a raw ip= value containing ':' (passed through verbatim)
//	dhcp | off | none             (passed through)
func StaticIP(s string) (string, error) {
	if s == "" || strings.ContainsAny(s, " \t\n\"") {
		return "", fmt.Errorf("invalid --ip %q", s)
	}
	switch s {
	case "dhcp", "off", "none":
		return s, nil
	}
	if client, _, ok := strings.Cut(s, ":"); ok {
		// Raw nfsroot syntax; the client field is empty or IPv4 (the
		// kernel's ip= is IPv4-only).
		if a, err := netip.ParseAddr(client); strings.Contains(s, "/") || (client != "" && (err != nil || !a.Is4())) {
			return "", fmt.Errorf("invalid --ip %q: raw ip= values are <client>:<server>:<gw>:<netmask>:... with IPv4 addresses", s)
		}
		return s, nil
	}
	parts := strings.Split(s, ",")
	if len(parts) > 3 {
		return "", fmt.Errorf("invalid --ip %q: want ADDR/PREFIX[,GATEWAY[,DNS]]", s)
	}
	pfx, err := netip.ParsePrefix(parts[0])
	if err != nil || !pfx.Addr().Is4() {
		return "", fmt.Errorf("invalid --ip %q: want an IPv4 ADDR/PREFIX such as 172.16.0.2/24", s)
	}
	bits := pfx.Bits()
	m := uint32(0)
	if bits > 0 {
		m = ^uint32(0) << (32 - bits)
	}
	mask := fmt.Sprintf("%d.%d.%d.%d", m>>24, m>>16&0xff, m>>8&0xff, m&0xff)
	var gw, dns string
	if len(parts) > 1 {
		a, err := netip.ParseAddr(parts[1])
		if err != nil || !a.Is4() {
			return "", fmt.Errorf("invalid --ip gateway %q", parts[1])
		}
		gw = a.String()
	}
	if len(parts) > 2 {
		a, err := netip.ParseAddr(parts[2])
		if err != nil || !a.Is4() {
			return "", fmt.Errorf("invalid --ip DNS server %q", parts[2])
		}
		dns = a.String()
	}
	v := fmt.Sprintf("%s::%s:%s:patina:eth0:off", pfx.Addr(), gw, mask)
	if dns != "" {
		v += ":" + dns
	}
	return v, nil
}

// GuestMAC derives a stable, locally administered unicast MAC address for
// the VM whose run dir is seed (so the same --name gets the same MAC and
// therefore the same DHCP lease).
func GuestMAC(seed string) string {
	h := sha256.Sum256([]byte("patina-mac\x00" + seed))
	h[0] = h[0]&^0x01 | 0x02
	return fmt.Sprintf("%02x:%02x:%02x:%02x:%02x:%02x", h[0], h[1], h[2], h[3], h[4], h[5])
}

// tapOnly rejects network modes a tap-only VMM cannot provide.
func tapOnly(vmm string, n NetConfig) error {
	if n.Mode == "user" {
		return fmt.Errorf("--net user is QEMU-only; %s needs a tap device (--net tap0)", vmm)
	}
	if len(n.Publish) > 0 {
		return fmt.Errorf("--publish only applies to QEMU --net user (%s: forward ports on the host to the tap address instead)", vmm)
	}
	return nil
}
