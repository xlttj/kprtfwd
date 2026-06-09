package k8s

import (
	"bytes"
	"errors"
	"fmt"
	"net"
	"os/exec"
	"strings"
	"sync"
	"time"

	"github.com/xlttj/kprtfwd/pkg/config"
	"github.com/xlttj/kprtfwd/pkg/logging"
)

// Sentinel error for port conflict
var ErrPortInUse = errors.New("local port already in use")

// Sentinel error for port reserved internally by the application
var ErrLocalPortReserved = errors.New("local port is already reserved by another active forward")

// PortForwardParams contains the essential parameters for starting a port-forward.
type PortForwardParams struct {
	Context    string
	Namespace  string
	Service    string
	PortRemote int // The target port on the service
	PortLocal  int // The local port to forward to
}

// runningInfo holds the command process and the local port being used.
type runningInfo struct {
	cmd       *exec.Cmd
	localPort int
	stopping  bool          // set (under PortForwarder.Mutex) before an intentional kill
	done      chan struct{} // closed by the watcher once the process is reaped
}

// PortForwarder manages multiple port-forward processes.
// Forwards are keyed by config ID (stable across config list reordering),
// never by list index: indices shift when configs are added/removed/edited.
type PortForwarder struct {
	RunningForwards  map[string]*runningInfo // Map of config ID to running info
	activeLocalPorts map[int]string          // Map of active local port -> config ID
	// Mutex protects the two maps above. It must never be held across
	// blocking calls (spawning kubectl, waiting on a process); only the
	// non-blocking Kill signal may be sent while holding it.
	Mutex sync.Mutex
}

// NewPortForwarder creates a new port forwarder
func NewPortForwarder() *PortForwarder {
	return &PortForwarder{
		RunningForwards:  make(map[string]*runningInfo),
		activeLocalPorts: make(map[int]string),
	}
}

// isPortAvailable checks if a TCP port is available to listen on localhost.
func isPortAvailable(port int) bool {
	address := fmt.Sprintf("127.0.0.1:%d", port)
	listener, err := net.Listen("tcp", address)
	if err != nil {
		// Port is likely already in use or another error occurred
		logging.LogDebug("Port check: Cannot listen on %s: %v", address, err)
		// Check if the error is a bind error, which strongly suggests the port is in use
		// This is a bit heuristic, but common for port conflicts.
		// Consider checking specific error types if available and stable across OS.
		if opErr, ok := err.(*net.OpError); ok && strings.Contains(opErr.Err.Error(), "bind") {
			return false // Port is likely in use
		}
		// For other errors, maybe log differently? For now, treat as unavailable.
		return false
	}
	// Successfully listened, close the listener immediately
	_ = listener.Close()
	logging.LogDebug("Port check: Port %d appears to be available.", port)
	return true
}

// StartPortForward starts a port-forward for a specific set of parameters.
func StartPortForward(params PortForwardParams) (*exec.Cmd, error) {
	// *** Pre-check if local target port is available ***
	if !isPortAvailable(params.PortLocal) {
		// Return the specific sentinel error
		logging.LogError("Pre-check failed: %v", ErrPortInUse)
		return nil, ErrPortInUse
	}
	// *** End Pre-check ***

	logging.LogDebug("Attempting port-forward: kubectl port-forward --namespace %s svc/%s %d:%d context=%s", params.Namespace, params.Service, params.PortRemote, params.PortLocal, params.Context)

	args := []string{"port-forward",
		"--namespace", params.Namespace,
		fmt.Sprintf("svc/%s", params.Service),
		fmt.Sprintf("%d:%d", params.PortLocal, params.PortRemote),
	}
	if params.Context != "" {
		args = append([]string{"--context", params.Context}, args...)
	}
	cmd := exec.Command("kubectl", args...)

	var stderr bytes.Buffer

	// Set stderr to capture output for checking
	cmd.Stderr = &stderr
	// Don't capture stdout
	cmd.Stdout = nil

	err := cmd.Start()
	if err != nil {
		stderrStr := stderr.String()
		logging.LogError("Failed to cmd.Start() port-forward: %v. Stderr: %s", err, stderrStr)
		// Wrap the original error
		if stderrStr != "" {
			return nil, fmt.Errorf("kubectl start failed (stderr: %s): %w", stderrStr, err)
		}
		return nil, fmt.Errorf("kubectl start failed: %w", err)
	}

	// The stderr buffer must not be read here: exec's copy goroutine writes
	// to it until the process exits, so it is only safe to read after Wait
	// returns (the watcher logs it on unexpected exit).
	logging.LogDebug("Started port-forward process PID: %d", cmd.Process.Pid)
	return cmd, nil
}

// killProcess terminates a port-forward process without reaping it.
// The watcher goroutine owns cmd.Wait, so this must never Wait.
func killProcess(cmd *exec.Cmd) error {
	if cmd == nil || cmd.Process == nil {
		return nil
	}
	logging.LogDebug("Killing port-forward process PID: %d", cmd.Process.Pid)
	return cmd.Process.Kill()
}

// watch reaps the kubectl process and cleans up tracking state when it exits
// without Stop having been called (VPN drop, expired credentials, pod
// deletion, ...). Exactly one watcher owns cmd.Wait per started process.
func (pf *PortForwarder) watch(id string, info *runningInfo) {
	err := info.cmd.Wait()
	if info.done != nil {
		close(info.done) // the process is reaped; its sockets are released
	}
	pf.handleProcessExit(id, info, err)
}

// handleProcessExit deregisters a forward whose process exited on its own.
func (pf *PortForwarder) handleProcessExit(id string, info *runningInfo, waitErr error) {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()

	if info.stopping {
		// Intentional stop; Stop/stopInternal already cleaned up the maps.
		return
	}
	if current, exists := pf.RunningForwards[id]; !exists || current != info {
		// The ID was deregistered or is owned by a newer process now.
		return
	}
	delete(pf.RunningForwards, id)
	if holder, reserved := pf.activeLocalPorts[info.localPort]; reserved && holder == id {
		delete(pf.activeLocalPorts, info.localPort)
	}

	// Safe to read only now: exec's copy goroutine finished when Wait returned.
	stderrStr := ""
	if info.cmd != nil {
		if buf, ok := info.cmd.Stderr.(*bytes.Buffer); ok {
			stderrStr = strings.TrimSpace(buf.String())
		}
	}
	logging.LogError("Port-forward '%s' (port %d) exited unexpectedly: %v (stderr: %s)", id, info.localPort, waitErr, stderrStr)
}

// Start attempts to start the port-forward for the given config.
func (pf *PortForwarder) Start(cfg config.PortForwardConfig) error {
	id := cfg.ID
	localPort := cfg.PortLocal // Get local port for checks

	pf.Mutex.Lock()
	if _, exists := pf.RunningForwards[id]; exists {
		logging.LogDebug("Port-forward for '%s' already marked as running.", id)
		pf.Mutex.Unlock()
		return nil // Already running, not an error
	}

	// *** Check internal reservation first ***
	if conflictingID, reserved := pf.activeLocalPorts[localPort]; reserved {
		logging.LogError("Cannot start '%s': %v (port %d reserved by '%s')", id, ErrLocalPortReserved, localPort, conflictingID)
		pf.Mutex.Unlock()
		return ErrLocalPortReserved // Return specific error
	}

	// *** Reserve the port internally ***
	pf.activeLocalPorts[localPort] = id
	logging.LogDebug("Reserved local port %d for '%s'", localPort, id)
	pf.Mutex.Unlock() // Unlock *before* calling potentially blocking StartPortForward helper

	// Fallback: Check if port is actually available using net.Listen (done inside StartPortForward)
	// Create params struct from config
	params := PortForwardParams{
		Context:    cfg.Context,
		Namespace:  cfg.Namespace,
		Service:    cfg.Service,
		PortRemote: cfg.PortRemote,
		PortLocal:  localPort,
	}

	// Call the helper function (which performs the net.Listen check)
	cmd, err := StartPortForward(params)

	// --- Handle outcome ---
	pf.Mutex.Lock() // Re-acquire lock to update state
	defer pf.Mutex.Unlock()

	if err != nil {
		// Start failed, release the reservation
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == id {
			delete(pf.activeLocalPorts, localPort)
			logging.LogDebug("Released local port %d reservation for '%s' due to start failure: %v", localPort, id, err)
		} else {
			// Log if reservation was already gone or held by someone else (shouldn't happen ideally)
			logging.LogError("Could not release reservation for port %d ('%s') after start failure. Current holder: '%s', Exists: %t", localPort, id, currentHolder, ok)
		}
		return err // Return the original error from StartPortForward
	}

	if cmd != nil {
		// Start succeeded, store running info
		info := &runningInfo{cmd: cmd, localPort: localPort, done: make(chan struct{})}
		pf.RunningForwards[id] = info
		go pf.watch(id, info)
		// Reservation in activeLocalPorts remains
		logging.LogDebug("Successfully started and registered port-forward for '%s' (PID: %d, Port: %d)", id, cmd.Process.Pid, localPort)
		return nil // Success
	} else {
		// Should not happen if StartPortForward only returns nil cmd with non-nil error
		// Release reservation as a precaution
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == id {
			delete(pf.activeLocalPorts, localPort)
			logging.LogDebug("Released local port %d reservation for '%s' due to nil cmd/err", localPort, id)
		}
		return fmt.Errorf("StartPortForward returned nil command without error for '%s'", id)
	}
}

// Stop attempts to stop the port-forward process for the given config ID.
func (pf *PortForwarder) Stop(id string) error {
	pf.Mutex.Lock()

	info, exists := pf.RunningForwards[id]
	if !exists {
		// Not running (or not tracked), do nothing
		pf.Mutex.Unlock()
		logging.LogDebug("Stop: Port-forward for '%s' not found or already stopped.", id)
		return nil
	}

	// Mark as intentionally stopping so the watcher doesn't treat the
	// process exit as a failure.
	info.stopping = true

	// Get local port from stored info
	localPort := info.localPort

	// Release internal reservation first
	if currentHolder, reserved := pf.activeLocalPorts[localPort]; reserved {
		if currentHolder == id {
			delete(pf.activeLocalPorts, localPort)
			logging.LogDebug("Stop: Released internal reservation for local port %d ('%s')", localPort, id)
		} else {
			// Log if reservation was held by someone else (indicates inconsistency)
			logging.LogError("Stop: Port %d reservation for '%s' was held by '%s'! Inconsistency?", localPort, id, currentHolder)
		}
	} else {
		// Log if no reservation existed (indicates inconsistency)
		logging.LogError("Stop: No internal reservation found for local port %d ('%s') during stop! Inconsistency?", localPort, id)
	}

	// Remove from running map
	delete(pf.RunningForwards, id)
	pf.Mutex.Unlock()

	// Kill outside the lock; the watcher goroutine reaps the process.
	err := killProcess(info.cmd)
	if err != nil {
		logging.LogError("Stop: Error killing port-forward process for '%s' (Port: %d): %v", id, localPort, err)
	}
	logging.LogDebug("Stop: Stopped and deregistered port-forward for '%s' (Port: %d)", id, localPort)
	return err
}

// stopInternal stops a forward assuming the lock is already held.
func (pf *PortForwarder) stopInternal(id string) error {
	info, exists := pf.RunningForwards[id]
	if !exists {
		return nil // Already stopped
	}
	info.stopping = true
	localPort := info.localPort
	if currentHolder, reserved := pf.activeLocalPorts[localPort]; reserved && currentHolder == id {
		delete(pf.activeLocalPorts, localPort)
	}
	delete(pf.RunningForwards, id)
	// Kill is a non-blocking signal; the watcher goroutine reaps the process.
	err := killProcess(info.cmd)
	logging.LogDebug("stopInternal: Stopped '%s' (Port: %d)", id, localPort)
	return err
}

// IsRunning checks if a port forward is currently running for the given config ID
func (pf *PortForwarder) IsRunning(id string) bool {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	_, exists := pf.RunningForwards[id]
	return exists
}

// CleanupAll stops all port-forwards
func (pf *PortForwarder) CleanupAll() {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	ids := make([]string, 0, len(pf.RunningForwards))
	for id := range pf.RunningForwards {
		ids = append(ids, id)
	}
	for _, id := range ids {
		logging.LogDebug("CleanupAll: Stopping '%s'", id)
		_ = pf.stopInternal(id) // Call internal stop
	}
	pf.RunningForwards = make(map[string]*runningInfo)
	pf.activeLocalPorts = make(map[int]string)
	logging.LogDebug("CleanupAll finished.")
}

// RestartResult represents the outcome of a restart operation
type RestartResult struct {
	RestartedCount int              // Number of port forwards restarted
	Errors         map[string]error // Errors by config ID
}

// processReapTimeout bounds how long a restart waits for the killed kubectl
// process to be reaped (and its listening socket released) before starting
// the replacement.
const processReapTimeout = 5 * time.Second

// RestartRunningForwards restarts all currently running port forwards
// This is useful when network connectivity is lost (e.g., VPN disconnect)
func (pf *PortForwarder) RestartRunningForwards(configs []config.PortForwardConfig) *RestartResult {
	result := &RestartResult{
		RestartedCount: 0,
		Errors:         make(map[string]error),
	}

	configsByID := make(map[string]config.PortForwardConfig, len(configs))
	for _, cfg := range configs {
		configsByID[cfg.ID] = cfg
	}

	// Snapshot the currently running IDs. Stop and Start manage their own
	// locking, so the mutex is not held across the blocking kubectl calls
	// below and other callers (UI status checks, watchers) stay responsive.
	pf.Mutex.Lock()
	runningIDs := make([]string, 0, len(pf.RunningForwards))
	for id := range pf.RunningForwards {
		runningIDs = append(runningIDs, id)
	}
	pf.Mutex.Unlock()

	logging.LogDebug("RestartRunningForwards: Found %d running port forwards to restart", len(runningIDs))

	// Restart each running port forward
	for _, id := range runningIDs {
		cfg, found := configsByID[id]
		if !found {
			logging.LogError("RestartRunningForwards: Config '%s' no longer exists", id)
			result.Errors[id] = fmt.Errorf("config '%s' no longer exists", id)
			continue
		}

		logging.LogDebug("RestartRunningForwards: Restarting port forward '%s' (%s)", id, cfg.Service)

		// Grab the running info first so we can wait for the old process to
		// be reaped after Stop; starting again while it still holds the
		// local socket would trip the port-availability pre-check.
		pf.Mutex.Lock()
		oldInfo := pf.RunningForwards[id]
		pf.Mutex.Unlock()

		// Stop the current port forward
		stopErr := pf.Stop(id)
		if stopErr != nil {
			logging.LogError("RestartRunningForwards: Failed to stop port forward '%s': %v", id, stopErr)
			result.Errors[id] = fmt.Errorf("failed to stop: %w", stopErr)
			continue
		}

		if oldInfo != nil && oldInfo.done != nil {
			select {
			case <-oldInfo.done:
			case <-time.After(processReapTimeout):
				logging.LogError("RestartRunningForwards: Timed out waiting for '%s' to exit; attempting start anyway", id)
			}
		}

		// Start it again with the same config
		startErr := pf.Start(cfg)
		if startErr != nil {
			logging.LogError("RestartRunningForwards: Failed to start port forward '%s': %v", id, startErr)
			result.Errors[id] = fmt.Errorf("failed to restart: %w", startErr)
			continue
		}

		result.RestartedCount++
		logging.LogDebug("RestartRunningForwards: Successfully restarted port forward '%s' (%s)", id, cfg.Service)
	}

	logging.LogDebug("RestartRunningForwards: Complete - Restarted: %d, Errors: %d", result.RestartedCount, len(result.Errors))
	return result
}
