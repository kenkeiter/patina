//go:build !(darwin && arm64 && cgo)

package vmm

import (
	"context"
	"fmt"
	"runtime"
)

func vzStart(context.Context, *Config, *VZSpec) (Machine, error) {
	if runtime.GOOS == "darwin" && runtime.GOARCH == "arm64" {
		return nil, fmt.Errorf("vz: this patina binary was built without cgo; rebuild with cli/scripts/build.sh")
	}
	return nil, fmt.Errorf("vz: Apple Virtualization.framework is unsupported on this platform (%s/%s); it needs macOS on Apple Silicon",
		runtime.GOOS, runtime.GOARCH)
}
