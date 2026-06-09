package k8s

import (
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"testing"
	"time"

	"github.com/xlttj/kprtfwd/pkg/config"
)

// installFakeKubectl puts a fake long-running kubectl on PATH so Start spawns
// a real, harmless process.
func installFakeKubectl(t *testing.T) {
	t.Helper()
	if runtime.GOOS == "windows" {
		t.Skip("fake kubectl shell script requires a Unix-like OS")
	}
	sleepPath, err := exec.LookPath("sleep")
	if err != nil {
		t.Skip("sleep binary not available")
	}
	dir := t.TempDir()
	script := fmt.Sprintf("#!/bin/sh\nexec %s 30\n", sleepPath)
	if err := os.WriteFile(filepath.Join(dir, "kubectl"), []byte(script), 0755); err != nil {
		t.Fatalf("failed to write fake kubectl: %v", err)
	}
	t.Setenv("PATH", dir+string(os.PathListSeparator)+os.Getenv("PATH"))
}

// freeLocalPort returns a TCP port that is currently free on localhost.
func freeLocalPort(t *testing.T) int {
	t.Helper()
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("failed to find a free port: %v", err)
	}
	port := l.Addr().(*net.TCPAddr).Port
	l.Close()
	return port
}

// currentPid returns the PID of the process backing the given forward.
func currentPid(t *testing.T, pf *PortForwarder, id string) int {
	t.Helper()
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	info := pf.RunningForwards[id]
	if info == nil || info.cmd == nil || info.cmd.Process == nil {
		t.Fatalf("no running process for '%s'", id)
	}
	return info.cmd.Process.Pid
}

// markRunning simulates an active forward without spawning a real kubectl
// process (killProcess treats a nil cmd as already exited). The done channel
// is pre-closed since there is no watcher to close it.
func markRunning(pf *PortForwarder, id string, localPort int) {
	done := make(chan struct{})
	close(done)
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	pf.RunningForwards[id] = &runningInfo{cmd: nil, localPort: localPort, done: done}
	pf.activeLocalPorts[localPort] = id
}

// Regression test for tracking forwards by list index: configs are sorted by
// context/namespace/service, so adding or deleting a config shifted every
// index after it and running state pointed at the wrong config. Keying by ID
// must keep state attached to the same config regardless of list order.
func TestRunningStateSurvivesConfigReordering(t *testing.T) {
	pf := NewPortForwarder()
	markRunning(pf, "ctx.ns.web", 8080)

	// "ctx.ns.web" was at index 1; deleting the config sorted before it
	// would previously have shifted it to index 0 and orphaned the state.
	if !pf.IsRunning("ctx.ns.web") {
		t.Fatal("forward should be running under its ID")
	}
	if pf.IsRunning("ctx.ns.api") {
		t.Fatal("unrelated ID must not report running")
	}

	if err := pf.Stop("ctx.ns.web"); err != nil {
		t.Fatalf("Stop returned error: %v", err)
	}
	if pf.IsRunning("ctx.ns.web") {
		t.Fatal("forward should be stopped")
	}
	pf.Mutex.Lock()
	_, reserved := pf.activeLocalPorts[8080]
	pf.Mutex.Unlock()
	if reserved {
		t.Fatal("Stop must release the local port reservation")
	}
}

func TestStopUnknownIDIsNoop(t *testing.T) {
	pf := NewPortForwarder()
	if err := pf.Stop("does-not-exist"); err != nil {
		t.Fatalf("stopping an untracked ID should be a no-op, got: %v", err)
	}
}

func TestStartRejectsPortReservedByOtherForward(t *testing.T) {
	pf := NewPortForwarder()
	markRunning(pf, "ctx.ns.api", 8080)

	err := pf.Start(config.PortForwardConfig{
		ID: "ctx.ns.web", Context: "ctx", Namespace: "ns",
		Service: "web", PortRemote: 80, PortLocal: 8080,
	})
	if !errors.Is(err, ErrLocalPortReserved) {
		t.Fatalf("expected ErrLocalPortReserved, got: %v", err)
	}
}

func TestStartIsIdempotentForRunningID(t *testing.T) {
	pf := NewPortForwarder()
	markRunning(pf, "ctx.ns.web", 8080)

	err := pf.Start(config.PortForwardConfig{
		ID: "ctx.ns.web", Context: "ctx", Namespace: "ns",
		Service: "web", PortRemote: 80, PortLocal: 8080,
	})
	if err != nil {
		t.Fatalf("starting an already-running ID should succeed without action, got: %v", err)
	}
}

// Unexpected process death (VPN drop, expired credentials) must deregister
// the forward and release its port reservation so it can be restarted.
func TestProcessExitDeregistersForwardAndReleasesPort(t *testing.T) {
	pf := NewPortForwarder()
	markRunning(pf, "ctx.ns.web", 8080)
	pf.Mutex.Lock()
	info := pf.RunningForwards["ctx.ns.web"]
	pf.Mutex.Unlock()

	pf.handleProcessExit("ctx.ns.web", info, fmt.Errorf("signal: killed"))

	if pf.IsRunning("ctx.ns.web") {
		t.Fatal("forward should be deregistered after its process exited")
	}
	pf.Mutex.Lock()
	_, reserved := pf.activeLocalPorts[8080]
	pf.Mutex.Unlock()
	if reserved {
		t.Fatal("port reservation must be released after process exit")
	}
}

// A watcher whose process was stopped intentionally must not touch state:
// Stop already cleaned up, and the ID may have been reused since.
func TestProcessExitAfterIntentionalStopLeavesStateAlone(t *testing.T) {
	pf := NewPortForwarder()
	markRunning(pf, "ctx.ns.web", 8080)
	pf.Mutex.Lock()
	oldInfo := pf.RunningForwards["ctx.ns.web"]
	pf.Mutex.Unlock()

	if err := pf.Stop("ctx.ns.web"); err != nil {
		t.Fatalf("Stop returned error: %v", err)
	}

	// Simulate the forward being started again (e.g. after a port edit)
	// before the old watcher observes the process exit.
	markRunning(pf, "ctx.ns.web", 9090)

	pf.handleProcessExit("ctx.ns.web", oldInfo, fmt.Errorf("signal: killed"))

	if !pf.IsRunning("ctx.ns.web") {
		t.Fatal("stale watcher must not deregister the restarted forward")
	}
	pf.Mutex.Lock()
	holder, reserved := pf.activeLocalPorts[9090]
	pf.Mutex.Unlock()
	if !reserved || holder != "ctx.ns.web" {
		t.Fatal("stale watcher must not release the new port reservation")
	}
}

// A watcher for an old process must not clobber state owned by a newer
// process registered under the same ID, even if the old one was never
// intentionally stopped.
func TestProcessExitIgnoresSupersededInfo(t *testing.T) {
	pf := NewPortForwarder()
	markRunning(pf, "ctx.ns.web", 8080)
	oldInfo := &runningInfo{cmd: nil, localPort: 8080}

	pf.handleProcessExit("ctx.ns.web", oldInfo, fmt.Errorf("signal: killed"))

	if !pf.IsRunning("ctx.ns.web") {
		t.Fatal("watcher with superseded info must not deregister the current forward")
	}
}

// End-to-end: the watcher goroutine reaps a real process and cleans up.
func TestWatcherCleansUpAfterRealProcessExit(t *testing.T) {
	sleepPath, err := exec.LookPath("sleep")
	if err != nil {
		t.Skip("sleep binary not available")
	}

	pf := NewPortForwarder()
	cmd := exec.Command(sleepPath, "0.05")
	if err := cmd.Start(); err != nil {
		t.Fatalf("failed to start test process: %v", err)
	}
	info := &runningInfo{cmd: cmd, localPort: 8080}
	pf.Mutex.Lock()
	pf.RunningForwards["ctx.ns.web"] = info
	pf.activeLocalPorts[8080] = "ctx.ns.web"
	pf.Mutex.Unlock()
	go pf.watch("ctx.ns.web", info)

	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if !pf.IsRunning("ctx.ns.web") {
			pf.Mutex.Lock()
			_, reserved := pf.activeLocalPorts[8080]
			pf.Mutex.Unlock()
			if reserved {
				t.Fatal("port reservation must be released when the process dies")
			}
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("watcher did not deregister the forward after its process exited")
}

// End-to-end restart: the running process must be killed, reaped, and
// replaced by a new one on the same port, without errors. This exercises the
// snapshot/Stop/wait-for-reap/Start sequence that replaced the old
// hold-the-lock-across-the-loop implementation.
func TestRestartRunningForwardsReplacesProcess(t *testing.T) {
	installFakeKubectl(t)

	pf := NewPortForwarder()
	defer pf.CleanupAll()

	cfg := config.PortForwardConfig{
		ID: "ctx.ns.web", Context: "ctx", Namespace: "ns",
		Service: "web", PortRemote: 80, PortLocal: freeLocalPort(t),
	}
	if err := pf.Start(cfg); err != nil {
		t.Fatalf("Start failed: %v", err)
	}
	oldPid := currentPid(t, pf, cfg.ID)

	result := pf.RestartRunningForwards([]config.PortForwardConfig{cfg})

	if len(result.Errors) != 0 {
		t.Fatalf("restart reported errors: %v", result.Errors)
	}
	if result.RestartedCount != 1 {
		t.Fatalf("expected 1 restart, got %d", result.RestartedCount)
	}
	if !pf.IsRunning(cfg.ID) {
		t.Fatal("forward should be running after restart")
	}
	if newPid := currentPid(t, pf, cfg.ID); newPid == oldPid {
		t.Fatal("restart must replace the process, but the PID is unchanged")
	}
}

// Configs carrying values kubectl would parse as flags must be rejected
// before any process is spawned, and the port reservation released so the
// port stays usable.
func TestStartRejectsFlagInjectionValues(t *testing.T) {
	cases := []struct {
		name string
		cfg  config.PortForwardConfig
	}{
		{"service flag", config.PortForwardConfig{
			ID: "a", Context: "ctx", Namespace: "ns",
			Service: "--kubeconfig=/tmp/evil", PortRemote: 80, PortLocal: 18080}},
		{"namespace flag", config.PortForwardConfig{
			ID: "b", Context: "ctx", Namespace: "-oyaml",
			Service: "web", PortRemote: 80, PortLocal: 18080}},
		{"context flag", config.PortForwardConfig{
			ID: "c", Context: "--token=steal", Namespace: "ns",
			Service: "web", PortRemote: 80, PortLocal: 18080}},
		{"remote port out of range", config.PortForwardConfig{
			ID: "d", Context: "ctx", Namespace: "ns",
			Service: "web", PortRemote: 0, PortLocal: 18080}},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			pf := NewPortForwarder()
			if err := pf.Start(tc.cfg); err == nil {
				t.Fatal("expected Start to reject the config")
			}
			if pf.IsRunning(tc.cfg.ID) {
				t.Fatal("rejected forward must not be marked running")
			}
			pf.Mutex.Lock()
			_, reserved := pf.activeLocalPorts[tc.cfg.PortLocal]
			pf.Mutex.Unlock()
			if reserved {
				t.Fatal("rejected forward must not keep its port reservation")
			}
		})
	}
}

func TestRestartReportsDeletedConfigByID(t *testing.T) {
	pf := NewPortForwarder()
	markRunning(pf, "ctx.ns.deleted", 8080)

	// The running forward's config no longer exists; restart must report it
	// under its ID instead of restarting whichever config holds its old index.
	result := pf.RestartRunningForwards([]config.PortForwardConfig{
		{ID: "ctx.ns.other", Context: "ctx", Namespace: "ns", Service: "other", PortRemote: 80, PortLocal: 9090},
	})

	if result.RestartedCount != 0 {
		t.Fatalf("expected no restarts, got %d", result.RestartedCount)
	}
	if _, ok := result.Errors["ctx.ns.deleted"]; !ok {
		t.Fatalf("expected error keyed by ID 'ctx.ns.deleted', got: %v", result.Errors)
	}
}
