package k8s

import (
	"errors"
	"testing"

	"github.com/xlttj/kprtfwd/pkg/config"
)

// markRunning simulates an active forward without spawning a real kubectl
// process (StopPortForward treats a nil cmd as already exited).
func markRunning(pf *PortForwarder, id string, localPort int) {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	pf.RunningForwards[id] = &runningInfo{cmd: nil, localPort: localPort}
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
