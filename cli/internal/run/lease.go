package run

import (
	"regexp"
	"strconv"
	"strings"

	"github.com/kenkeiter/patina/cli/internal/runstate"
)

// leaseRE matches the kernel's DHCP lease message (D72), either as the
// msg of a control-channel {"ev":"log","level":"info"} event or, after the
// "[patina] " prefix, as a console line:
//
//	net: dhcp lease 192.168.64.3/24 via 192.168.64.1, dns 192.168.64.1[, host H][, domain D] (N ms)
//	net: dhcp lease changed: 192.168.64.9/24 via ..., dns ... (N ms)
//
// The gateway and dns fields may be "none"; dns lists servers separated by
// spaces.
var leaseRE = regexp.MustCompile(`^net: dhcp lease (?:changed: )?([0-9.]+)/([0-9]+) via (\S+), dns ([^,(]+?)(?:, .*)? \([0-9]+ ms\)\s*$`)

// parseLease extracts the guest address from a lease message.
func parseLease(msg string) *runstate.Net {
	m := leaseRE.FindStringSubmatch(strings.TrimRight(msg, "\r\n"))
	if m == nil {
		return nil
	}
	prefix, err := strconv.Atoi(m[2])
	if err != nil {
		return nil
	}
	n := &runstate.Net{Addr: m[1], Prefix: prefix}
	if m[3] != "none" {
		n.Gateway = m[3]
	}
	if m[4] != "none" {
		n.DNS = strings.Fields(m[4])
	}
	return n
}

// consoleLeasePrefix starts the console form of the lease line.
const consoleLeasePrefix = "[patina] "
