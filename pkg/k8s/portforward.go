package k8s

import (
	"bytes"
	"errors"
	"fmt"
	"net"
	"os/exec"
	"strings"
	"sync"

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
}

// PortForwarder manages multiple port-forward processes.
// Forwards are keyed by config ID (stable across config list reordering),
// never by list index: indices shift when configs are added/removed/edited.
type PortForwarder struct {
	RunningForwards  map[string]*runningInfo // Map of config ID to running info
	activeLocalPorts map[int]string          // Map of active local port -> config ID
	Mutex            sync.Mutex              // Mutex to protect the maps
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

	// Check if the process has exited. Non-blocking check.
	// cmd.ProcessState is nil if still running after Start()
	if cmd.ProcessState != nil {
		stderrStr := stderr.String()
		logging.LogError("Port-forward process exited quickly (PID: %d). Stderr: %s", cmd.Process.Pid, stderrStr)
		// Consider a specific sentinel error here too?
		return nil, fmt.Errorf("kubectl exited quickly. Stderr: %s", stderrStr)
	}

	// Do not treat immediate stderr as fatal; kubectl may emit warnings
	stderrStr := strings.TrimSpace(stderr.String())
	if stderrStr != "" {
		logging.LogDebug("kubectl port-forward initial stderr (non-fatal): %s", stderrStr)
	}

	// If we reach here, Start() succeeded and process appears running
	logging.LogDebug("Started port-forward process PID: %d, appears stable.", cmd.Process.Pid)
	return cmd, nil
}

// StopPortForward stops a port-forward process
func StopPortForward(cmd *exec.Cmd) error {
	if cmd == nil || cmd.Process == nil {
		return nil
	}
	logging.LogDebug("Stopping port-forward process PID: %d", cmd.Process.Pid)

	// Try graceful interrupt first, then kill
	_ = cmd.Process.Kill()
	// Reap the process to avoid zombies
	_ = cmd.Wait()
	return nil
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
		pf.RunningForwards[id] = &runningInfo{cmd: cmd, localPort: localPort}
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
	defer pf.Mutex.Unlock()

	runningInfo, exists := pf.RunningForwards[id]
	if !exists {
		// Not running (or not tracked), do nothing
		logging.LogDebug("Stop: Port-forward for '%s' not found or already stopped.", id)
		return nil
	}

	// Get local port from stored info
	localPort := runningInfo.localPort

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

	// Now stop the actual process
	err := StopPortForward(runningInfo.cmd) // Call helper
	if err != nil {
		// Log the error but proceed to remove from map
		logging.LogError("Stop: Error stopping port-forward process for '%s' (PID: %d, Port: %d): %v", id, runningInfo.cmd.Process.Pid, localPort, err)
	}

	// Remove from running map
	delete(pf.RunningForwards, id)
	logging.LogDebug("Stop: Stopped and deregistered port-forward for '%s' (Port: %d)", id, localPort)
	return err // Return the error from StopPortForward helper
}

// stopInternal stops a forward assuming the lock is already held.
func (pf *PortForwarder) stopInternal(id string) error {
	runningInfo, exists := pf.RunningForwards[id]
	if !exists {
		return nil // Already stopped
	}
	localPort := runningInfo.localPort
	if currentHolder, reserved := pf.activeLocalPorts[localPort]; reserved && currentHolder == id {
		delete(pf.activeLocalPorts, localPort)
	}
	err := StopPortForward(runningInfo.cmd) // Call helper
	delete(pf.RunningForwards, id)
	logging.LogDebug("stopInternal: Stopped '%s' (Port: %d)", id, localPort)
	return err
}

// startInternal starts a forward assuming the lock is already held for map checks/updates.
func (pf *PortForwarder) startInternal(cfg config.PortForwardConfig) error {
	id := cfg.ID
	localPort := cfg.PortLocal
	if _, exists := pf.RunningForwards[id]; exists {
		return nil // Already running
	}
	if conflictingID, reserved := pf.activeLocalPorts[localPort]; reserved {
		return fmt.Errorf("%w: port %d requested by '%s', but reserved by '%s'", ErrLocalPortReserved, localPort, id, conflictingID)
	}

	// Reserve port
	pf.activeLocalPorts[localPort] = id
	logging.LogDebug("startInternal: Reserved port %d for '%s'", localPort, id)

	// Unlock *briefly* only for the potentially blocking call
	pf.Mutex.Unlock()
	// Correctly initialize the params struct
	params := PortForwardParams{
		Context:    cfg.Context,
		Namespace:  cfg.Namespace,
		Service:    cfg.Service,
		PortRemote: cfg.PortRemote,
		PortLocal:  localPort,
	}
	cmd, err := StartPortForward(params)
	pf.Mutex.Lock() // Re-acquire lock immediately after call

	if err != nil {
		// Failed, release reservation
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == id {
			delete(pf.activeLocalPorts, localPort)
		}
		return err // Return error from helper
	}
	if cmd != nil {
		// Succeeded, add to running map
		pf.RunningForwards[id] = &runningInfo{cmd: cmd, localPort: localPort}
		return nil // Success
	} else {
		// Failed (nil cmd/err case), release reservation
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == id {
			delete(pf.activeLocalPorts, localPort)
		}
		return fmt.Errorf("StartPortForward returned nil cmd/err for '%s'", id)
	}
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

// RestartRunningForwards restarts all currently running port forwards
// This is useful when network connectivity is lost (e.g., VPN disconnect)
func (pf *PortForwarder) RestartRunningForwards(configs []config.PortForwardConfig) *RestartResult {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()

	result := &RestartResult{
		RestartedCount: 0,
		Errors:         make(map[string]error),
	}

	configsByID := make(map[string]config.PortForwardConfig, len(configs))
	for _, cfg := range configs {
		configsByID[cfg.ID] = cfg
	}

	// Get a snapshot of currently running IDs
	runningIDs := []string{}
	for id := range pf.RunningForwards {
		runningIDs = append(runningIDs, id)
	}

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

		// Stop the current port forward
		stopErr := pf.stopInternal(id)
		if stopErr != nil {
			logging.LogError("RestartRunningForwards: Failed to stop port forward '%s': %v", id, stopErr)
			result.Errors[id] = fmt.Errorf("failed to stop: %w", stopErr)
			continue
		}

		// Start it again with the same config
		startErr := pf.startInternal(cfg)
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
