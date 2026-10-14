//go:build darwin && arm64 && cgo

package vmm

/*
#cgo CFLAGS: -x objective-c -fobjc-arc -Wall
#cgo LDFLAGS: -framework Virtualization -framework Foundation

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#import <Foundation/Foundation.h>
#import <Virtualization/Virtualization.h>

// Event bytes written to the event pipe (read by Go).
#define VZEV_GUEST_STOP 'S' // guestDidStopVirtualMachine (PSCI SYSTEM_OFF)
#define VZEV_ERROR      'E' // virtualMachine:didStopWithError:
#define VZEV_KILLED     'K' // stopWithCompletionHandler finished (host stop)
#define VZEV_NET        'N' // NAT attachment disconnected

static char *vz_errstr(NSError *e, const char *what) {
	NSString *s;
	if (e == nil) {
		s = [NSString stringWithFormat:@"%s failed", what];
	} else {
		s = [NSString stringWithFormat:@"%s: %@", what, e.localizedDescription];
		if (e.localizedFailureReason && ![e.localizedDescription containsString:e.localizedFailureReason])
			s = [s stringByAppendingFormat:@" (%@)", e.localizedFailureReason];
		NSError *u = e.userInfo[NSUnderlyingErrorKey];
		if (u) s = [s stringByAppendingFormat:@" [%@ %ld]", u.domain, (long)u.code];
	}
	return strdup(s.UTF8String);
}

static char *vz_excstr(NSException *x, const char *what) {
	return strdup([NSString stringWithFormat:@"%s: %@: %@", what, x.name, x.reason].UTF8String);
}

@interface PatinaVZ : NSObject <VZVirtualMachineDelegate>
@property (strong) VZVirtualMachine *vm;
@property (strong) dispatch_queue_t queue;
@property int eventFD;
@property (copy) NSString *lastError;
@end

@implementation PatinaVZ
- (void)emit:(char)c {
	if (self.eventFD >= 0) {
		ssize_t n = write(self.eventFD, &c, 1);
		(void)n;
	}
}
- (void)guestDidStopVirtualMachine:(VZVirtualMachine *)vm {
	[self emit:VZEV_GUEST_STOP];
}
- (void)virtualMachine:(VZVirtualMachine *)vm didStopWithError:(NSError *)error {
	char *s = vz_errstr(error, "virtual machine stopped with an error");
	self.lastError = [NSString stringWithUTF8String:s];
	free(s);
	[self emit:VZEV_ERROR];
}
- (void)virtualMachine:(VZVirtualMachine *)vm networkDevice:(VZNetworkDevice *)nd attachmentWasDisconnectedWithError:(NSError *)error {
	char *s = vz_errstr(error, "network attachment disconnected");
	self.lastError = [NSString stringWithUTF8String:s];
	free(s);
	[self emit:VZEV_NET];
}
@end

// vz_build_config builds and validates the machine configuration.
static VZVirtualMachineConfiguration *vz_build_config(const char *kernel, const char *initrd,
		const char *cmdline, int cpus, uint64_t mem, int nat, int serial_wfd, char **err) {
	VZVirtualMachineConfiguration *c = [[VZVirtualMachineConfiguration alloc] init];

	VZLinuxBootLoader *boot = [[VZLinuxBootLoader alloc]
		initWithKernelURL:[NSURL fileURLWithPath:[NSString stringWithUTF8String:kernel]]];
	boot.initialRamdiskURL = [NSURL fileURLWithPath:[NSString stringWithUTF8String:initrd]];
	boot.commandLine = [NSString stringWithUTF8String:cmdline];
	c.bootLoader = boot;
	c.platform = [[VZGenericPlatformConfiguration alloc] init];
	c.CPUCount = (NSUInteger)cpus;
	c.memorySize = mem;

	// Console: virtio-console port 0 (hvc0) -> serial_wfd. No guest input.
	if (serial_wfd >= 0) {
		NSFileHandle *w = [[NSFileHandle alloc] initWithFileDescriptor:serial_wfd closeOnDealloc:YES];
		VZFileHandleSerialPortAttachment *att =
			[[VZFileHandleSerialPortAttachment alloc] initWithFileHandleForReading:nil fileHandleForWriting:w];
		VZVirtioConsoleDeviceSerialPortConfiguration *con = [[VZVirtioConsoleDeviceSerialPortConfiguration alloc] init];
		con.attachment = att;
		c.serialPorts = @[ con ];
	}
	c.socketDevices = @[ [[VZVirtioSocketDeviceConfiguration alloc] init] ];
	c.entropyDevices = @[ [[VZVirtioEntropyDeviceConfiguration alloc] init] ];
	if (nat) {
		VZVirtioNetworkDeviceConfiguration *net = [[VZVirtioNetworkDeviceConfiguration alloc] init];
		net.attachment = [[VZNATNetworkDeviceAttachment alloc] init];
		c.networkDevices = @[ net ];
	}

	NSError *e = nil;
	if (![c validateWithError:&e]) {
		*err = vz_errstr(e, "invalid VZ configuration");
		return nil;
	}
	return c;
}

// vz_validate builds the configuration without a VM (unit tests).
static int vz_validate(const char *kernel, const char *initrd, const char *cmdline, int cpus,
		uint64_t mem, int nat, char **err) {
	@autoreleasepool {
		@try {
			return vz_build_config(kernel, initrd, cmdline, cpus, mem, nat, -1, err) ? 0 : -1;
		} @catch (NSException *x) {
			*err = vz_excstr(x, "building VZ configuration");
			return -1;
		}
	}
}

static void vz_limits(int *mincpu, int *maxcpu, uint64_t *minmem, uint64_t *maxmem) {
	*mincpu = (int)VZVirtualMachineConfiguration.minimumAllowedCPUCount;
	*maxcpu = (int)VZVirtualMachineConfiguration.maximumAllowedCPUCount;
	*minmem = VZVirtualMachineConfiguration.minimumAllowedMemorySize;
	*maxmem = VZVirtualMachineConfiguration.maximumAllowedMemorySize;
}

// vz_new creates (but does not start) a VM. Takes ownership of serial_wfd.
static void *vz_new(const char *kernel, const char *initrd, const char *cmdline, int cpus,
		uint64_t mem, int nat, int serial_wfd, int event_wfd, char **err) {
	@autoreleasepool {
		@try {
			VZVirtualMachineConfiguration *c =
				vz_build_config(kernel, initrd, cmdline, cpus, mem, nat, serial_wfd, err);
			if (!c) {
				close(serial_wfd);
				return NULL;
			}
			PatinaVZ *p = [PatinaVZ new];
			p.eventFD = event_wfd;
			p.queue = dispatch_queue_create("dev.patina.vz", DISPATCH_QUEUE_SERIAL);
			dispatch_sync(p.queue, ^{
				p.vm = [[VZVirtualMachine alloc] initWithConfiguration:c queue:p.queue];
				p.vm.delegate = p;
			});
			return (void *)CFBridgingRetain(p);
		} @catch (NSException *x) {
			*err = vz_excstr(x, "creating VZ virtual machine");
			return NULL;
		}
	}
}

// vz_start starts the VM and waits for the framework's answer.
static int vz_start(void *h, char **err) {
	PatinaVZ *p = (__bridge PatinaVZ *)h;
	dispatch_semaphore_t done = dispatch_semaphore_create(0);
	__block NSError *serr = nil;
	__block NSString *xs = nil;
	dispatch_async(p.queue, ^{
		@try {
			[p.vm startWithCompletionHandler:^(NSError *e) {
				serr = e;
				dispatch_semaphore_signal(done);
			}];
		} @catch (NSException *x) {
			xs = [NSString stringWithFormat:@"starting VM: %@: %@", x.name, x.reason];
			dispatch_semaphore_signal(done);
		}
	});
	dispatch_semaphore_wait(done, DISPATCH_TIME_FOREVER);
	if (xs) {
		*err = strdup(xs.UTF8String);
		return -1;
	}
	if (serr) {
		*err = vz_errstr(serr, "starting VM");
		return -1;
	}
	return 0;
}

// vz_stop force-stops the VM (like pulling the plug) and emits 'K' when done.
// Returns 1 if a stop was issued, 0 if the VM could not be stopped (already
// stopped / not started).
static int vz_stop(void *h) {
	PatinaVZ *p = (__bridge PatinaVZ *)h;
	__block int issued = 0;
	dispatch_sync(p.queue, ^{
		if (p.vm.canStop) {
			issued = 1;
			[p.vm stopWithCompletionHandler:^(NSError *e) {
				if (e) {
					char *s = vz_errstr(e, "stopping VM");
					p.lastError = [NSString stringWithUTF8String:s];
					free(s);
				}
				[p emit:VZEV_KILLED];
			}];
		}
	});
	return issued;
}

// Connection attempt shared between the queue and the waiting caller.
@interface PatinaConnect : NSObject
@property (strong) VZVirtioSocketConnection *conn;
@property (strong) NSError *error;
@property BOOL abandoned;
@end
@implementation PatinaConnect
@end

// vz_connect opens a vsock stream to the guest port. On success *fd is the
// connection's descriptor (still owned by *conn; release with vz_conn_close).
static int vz_connect(void *h, uint32_t port, int64_t timeout_ms, int *fd, void **conn, char **err) {
	PatinaVZ *p = (__bridge PatinaVZ *)h;
	PatinaConnect *pc = [PatinaConnect new];
	dispatch_semaphore_t done = dispatch_semaphore_create(0);
	dispatch_async(p.queue, ^{
		VZVirtioSocketDevice *dev = nil;
		if (p.vm.state == VZVirtualMachineStateRunning && p.vm.socketDevices.count > 0)
			dev = (VZVirtioSocketDevice *)p.vm.socketDevices[0];
		if (!dev) {
			pc.error = [NSError errorWithDomain:@"patina" code:1
				userInfo:@{NSLocalizedDescriptionKey: @"VM is not running"}];
			dispatch_semaphore_signal(done);
			return;
		}
		[dev connectToPort:port completionHandler:^(VZVirtioSocketConnection *c, NSError *e) {
			@synchronized (pc) {
				if (pc.abandoned) {
					[c close];
					return;
				}
				pc.conn = c;
				pc.error = e;
			}
			dispatch_semaphore_signal(done);
		}];
	});
	dispatch_time_t dl = timeout_ms > 0 ? dispatch_time(DISPATCH_TIME_NOW, timeout_ms * NSEC_PER_MSEC) : DISPATCH_TIME_FOREVER;
	long timedOut = dispatch_semaphore_wait(done, dl);
	@synchronized (pc) {
		if (timedOut) {
			pc.abandoned = YES;
			*err = strdup("vsock connect timed out");
			return -1;
		}
		if (pc.conn == nil) {
			*err = vz_errstr(pc.error, "vsock connect");
			return -1;
		}
		*fd = pc.conn.fileDescriptor;
		*conn = (void *)CFBridgingRetain(pc.conn);
	}
	return 0;
}

static void vz_conn_close(void *c) {
	VZVirtioSocketConnection *conn = (VZVirtioSocketConnection *)CFBridgingRelease(c);
	[conn close];
}

static char *vz_last_error(void *h) {
	PatinaVZ *p = (__bridge PatinaVZ *)h;
	NSString *s = p.lastError;
	return s ? strdup(s.UTF8String) : NULL;
}

// vz_free drops the VM (closing the serial attachment) and the handle.
static void vz_free(void *h) {
	PatinaVZ *p = (PatinaVZ *)CFBridgingRelease(h);
	dispatch_sync(p.queue, ^{
		p.vm.delegate = nil;
		p.vm = nil;
	});
	p.eventFD = -1;
}
*/
import "C"

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unsafe"
)

func cstrErr(p *C.char) error {
	if p == nil {
		return errors.New("unknown error")
	}
	defer C.free(unsafe.Pointer(p))
	s := C.GoString(p)
	if strings.Contains(s, "com.apple.security.virtualization") {
		s += "\n\tthe patina binary must be signed with that entitlement: build it with cli/scripts/build.sh"
	}
	return errors.New(s)
}

// VZLimits reports the host's allowed CPU and memory ranges.
func VZLimits() (minCPU, maxCPU int, minMem, maxMem uint64) {
	var a, b C.int
	var c, d C.uint64_t
	C.vz_limits(&a, &b, &c, &d)
	return int(a), int(b), uint64(c), uint64(d)
}

// vzValidate builds and validates the VZ configuration for spec without
// creating a VM.
func vzValidate(spec *VZSpec) error {
	k, i, cl := C.CString(spec.Kernel), C.CString(spec.Initrd), C.CString(spec.Cmdline)
	defer C.free(unsafe.Pointer(k))
	defer C.free(unsafe.Pointer(i))
	defer C.free(unsafe.Pointer(cl))
	var cerr *C.char
	if C.vz_validate(k, i, cl, C.int(spec.CPUs), C.uint64_t(spec.MemBytes), boolInt(spec.NAT), &cerr) != 0 {
		return fmt.Errorf("vz: %w", cstrErr(cerr))
	}
	return nil
}

func boolInt(b bool) C.int {
	if b {
		return 1
	}
	return 0
}

type vzDarwin struct {
	h      unsafe.Pointer
	cfg    *Config
	events *os.File // read end of the event pipe
	serial *os.File // read end of the console pipe
	copied chan struct{}
	lastRx atomic.Int64 // UnixNano of the last console read

	vmMu    sync.RWMutex // held shared while calling into the VM, exclusively by free
	mu      sync.Mutex
	freed   bool
	killed  bool
	conns   map[*vzConn]struct{}
	waitRes *Exit
	waitErr error
	waited  chan struct{}
}

func vzStart(ctx context.Context, cfg *Config, spec *VZSpec) (Machine, error) {
	if cfg.Stderr == nil {
		cfg.Stderr = io.Discard
	}
	if cfg.Log == nil {
		cfg.Log = io.Discard
	}
	if cfg.Serial == nil {
		cfg.Serial = io.Discard
	}
	minC, maxC, minM, maxM := VZLimits()
	if spec.CPUs < minC || spec.CPUs > maxC {
		return nil, fmt.Errorf("vz: --cpus %d out of range [%d, %d]", spec.CPUs, minC, maxC)
	}
	if spec.MemBytes < minM || spec.MemBytes > maxM {
		return nil, fmt.Errorf("vz: --mem %d MiB out of range [%d, %d] MiB", spec.MemBytes>>20, minM>>20, maxM>>20)
	}
	sr, sw, err := os.Pipe()
	if err != nil {
		return nil, err
	}
	er, ew, err := os.Pipe()
	if err != nil {
		sr.Close()
		sw.Close()
		return nil, err
	}
	// The shim owns a dup of the console write end; ours is closed so EOF
	// arrives when the VM is released. (Fd() left both write ends blocking,
	// which NSFileHandle and the delegate's write(2) rely on.)
	swfd, err := syscall.Dup(int(sw.Fd()))
	sw.Close()
	if err != nil {
		sr.Close()
		er.Close()
		ew.Close()
		return nil, err
	}

	k, i, cl := C.CString(spec.Kernel), C.CString(spec.Initrd), C.CString(spec.Cmdline)
	defer C.free(unsafe.Pointer(k))
	defer C.free(unsafe.Pointer(i))
	defer C.free(unsafe.Pointer(cl))
	fmt.Fprintf(cfg.Log, "+ vz: kernel=%s initrd=%s cpus=%d mem=%dMiB nat=%v vsock-port=%d\n+ vz: cmdline %s\n",
		spec.Kernel, spec.Initrd, spec.CPUs, spec.MemBytes>>20, spec.NAT, spec.VsockPort, spec.Cmdline)
	var cerr *C.char
	h := C.vz_new(k, i, cl, C.int(spec.CPUs), C.uint64_t(spec.MemBytes), boolInt(spec.NAT),
		C.int(swfd), C.int(ew.Fd()), &cerr)
	if h == nil {
		sr.Close()
		er.Close()
		ew.Close()
		return nil, fmt.Errorf("vz: %w", cstrErr(cerr))
	}
	m := &vzDarwin{h: h, cfg: cfg, events: er, serial: sr, copied: make(chan struct{}),
		conns: map[*vzConn]struct{}{}, waited: make(chan struct{})}
	// ew stays open (the shim writes to its descriptor) until waitLoop has
	// freed the VM; holding the *os.File keeps its finalizer away.
	go m.copyConsole()
	if C.vz_start(h, &cerr) != 0 {
		err := fmt.Errorf("vz: %w", cstrErr(cerr))
		m.free()
		ew.Close()
		er.Close()
		return nil, err
	}
	go m.waitLoop(ew)
	go func() {
		select {
		case <-ctx.Done():
			m.Kill()
		case <-m.waited:
		}
	}()
	return &vzMachine{port: spec.VsockPort, dialFn: m.dial, wait: m.wait, kill: m.Kill}, nil
}

func (m *vzDarwin) waitLoop(ew *os.File) {
	defer close(m.waited)
	defer ew.Close()
	ex := &Exit{}
	buf := make([]byte, 1)
	for {
		n, err := m.events.Read(buf)
		if n == 0 || err != nil {
			m.waitErr = fmt.Errorf("vz: event pipe: %v", err)
			break
		}
		switch buf[0] {
		case 'S':
			fmt.Fprintf(m.cfg.Log, "patina: vz: guest stopped the VM\n")
			ex.Code = 0
		case 'E':
			fmt.Fprintf(m.cfg.Stderr, "patina: vz: %v\n", cstrErr(C.vz_last_error(m.h)))
			ex.Code = 1
		case 'K':
			ex.Code = -1
		case 'N':
			fmt.Fprintf(m.cfg.Stderr, "patina: vz: %v\n", cstrErr(C.vz_last_error(m.h)))
			continue
		default:
			continue
		}
		break
	}
	m.waitRes = ex
	stopped := time.Now()
	m.free()
	m.drainConsole(stopped)
	m.serial.Close()
	m.events.Close()
}

// Console drain bounds: the framework does not reliably close its end of
// the console pipe when the VM is released (EOF came in ~1 of 10 runs), so
// after the stop event the console is read until it has been quiet for
// consoleQuiet (the guest's last lines are normally in the pipe already:
// its console driver waits for the device to consume them before
// poweroff), and never longer than consoleDrainMax.
const (
	consoleQuiet    = 20 * time.Millisecond
	consoleDrainMax = 250 * time.Millisecond
)

func (m *vzDarwin) drainConsole(stopped time.Time) {
	deadline := stopped.Add(consoleDrainMax)
	for {
		last := time.Unix(0, m.lastRx.Load())
		if last.Before(stopped) {
			last = stopped
		}
		quietAt := last.Add(consoleQuiet)
		if quietAt.After(deadline) {
			quietAt = deadline
		}
		d := time.Until(quietAt)
		if d <= 0 {
			break
		}
		select {
		case <-m.copied:
			return
		case <-time.After(d):
		}
	}
	m.serial.SetReadDeadline(time.Now())
	<-m.copied
}

// copyConsole copies guest console output to cfg.Serial, noting when data
// last arrived (drainConsole).
func (m *vzDarwin) copyConsole() {
	defer close(m.copied)
	buf := make([]byte, 32<<10)
	for {
		n, err := m.serial.Read(buf)
		if n > 0 {
			m.lastRx.Store(time.Now().UnixNano())
			if _, werr := m.cfg.Serial.Write(buf[:n]); werr != nil {
				return
			}
		}
		if err != nil {
			return
		}
	}
}

func (m *vzDarwin) wait() (*Exit, error) {
	<-m.waited
	return m.waitRes, m.waitErr
}

// free releases the VM and every open vsock connection.
func (m *vzDarwin) free() {
	m.vmMu.Lock()
	defer m.vmMu.Unlock()
	m.mu.Lock()
	if m.freed {
		m.mu.Unlock()
		return
	}
	m.freed = true
	conns := m.conns
	m.conns = nil
	m.mu.Unlock()
	for c := range conns {
		c.Close()
	}
	C.vz_free(m.h)
}

// Kill implements Machine.Kill: force-stop the VM.
func (m *vzDarwin) Kill() error {
	m.mu.Lock()
	if m.freed || m.killed {
		m.mu.Unlock()
		return nil
	}
	m.killed = true
	m.mu.Unlock()
	C.vz_stop(m.h)
	return nil
}

func (m *vzDarwin) dial(port uint32, timeout time.Duration) (net.Conn, error) {
	m.vmMu.RLock()
	defer m.vmMu.RUnlock()
	m.mu.Lock()
	freed := m.freed
	m.mu.Unlock()
	if freed {
		return nil, errors.New("VM has stopped")
	}
	var fd C.int
	var conn unsafe.Pointer
	var cerr *C.char
	if C.vz_connect(m.h, C.uint32_t(port), C.int64_t(timeout.Milliseconds()), &fd, &conn, &cerr) != 0 {
		return nil, cstrErr(cerr)
	}
	c, err := newVZConn(int(fd), func() { C.vz_conn_close(conn) })
	if err != nil {
		C.vz_conn_close(conn)
		return nil, err
	}
	c.untrack = func() {
		m.mu.Lock()
		delete(m.conns, c)
		m.mu.Unlock()
	}
	m.mu.Lock()
	m.conns[c] = struct{}{}
	m.mu.Unlock()
	return c, nil
}

// vzConn is a net.Conn over a dup of a VZVirtioSocketConnection descriptor;
// Close also releases the framework's connection object.
type vzConn struct {
	net.Conn
	once    sync.Once
	release func()
	untrack func()
}

func newVZConn(fd int, release func()) (*vzConn, error) {
	nfd, err := syscall.Dup(fd)
	if err != nil {
		return nil, err
	}
	// Non-blocking before NewFile so the runtime poller (and deadlines)
	// apply even on the fileConn fallback.
	syscall.SetNonblock(nfd, true)
	f := os.NewFile(uintptr(nfd), "vz-vsock")
	var nc net.Conn
	if c, err := net.FileConn(f); err == nil {
		nc = c
		f.Close()
	} else {
		nc = &fileConn{File: f}
	}
	return &vzConn{Conn: nc, release: release}, nil
}

func (c *vzConn) Close() error {
	err := c.Conn.Close()
	c.once.Do(func() {
		if c.untrack != nil {
			c.untrack()
		}
		c.release()
	})
	return err
}

// fileConn adapts an *os.File to net.Conn.
type fileConn struct{ *os.File }

type vzAddr struct{}

func (vzAddr) Network() string { return "vsock" }
func (vzAddr) String() string  { return "vz-vsock" }

func (f *fileConn) LocalAddr() net.Addr  { return vzAddr{} }
func (f *fileConn) RemoteAddr() net.Addr { return vzAddr{} }
