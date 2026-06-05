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
}

// PortForwarder manages multiple port-forward processes
type PortForwarder struct {
	RunningForwards  map[int]*runningInfo // Map of config index to running info
	activeLocalPorts map[int]int          // Map of active local port -> config index
	failedForwards   map[int]struct{}     // Indices that exited unexpectedly or failed to start
	Mutex            sync.Mutex           // Mutex to protect the maps
}

// NewPortForwarder creates a new port forwarder
func NewPortForwarder() *PortForwarder {
	return &PortForwarder{
		RunningForwards:  make(map[int]*runningInfo),
		activeLocalPorts: make(map[int]int),
		failedForwards:   make(map[int]struct{}),
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

	// Put kubectl in its own process group so that any child processes it
	// spawns (SSO credential plugins, browser launchers) can be killed as a
	// unit. See portforward_proc_unix.go / portforward_proc_windows.go.
	setProcGroupAttrs(cmd)

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

	// Give kubectl a moment to fail fast. Common quick-exit cases:
	//   - VPN not connected (EHOSTUNREACH → kubectl exits in <50ms)
	//   - Invalid context / bad kubeconfig
	//   - Port already in use (detected by kubectl itself)
	// Note: cmd.ProcessState is only populated after cmd.Wait(), so checking
	// it here (as the old code did) is always nil — it was dead code.
	time.Sleep(200 * time.Millisecond)

	if !isProcessAlive(cmd.Process) {
		// Process died during startup. Call Wait() to reap the zombie and
		// ensure the stderr pipe goroutine finishes flushing the buffer.
		_ = cmd.Wait()
		stderrStr := strings.TrimSpace(stderr.String())
		logging.LogError("kubectl (PID %d) exited immediately after start. Stderr: %s", cmd.Process.Pid, stderrStr)
		if stderrStr != "" {
			return nil, fmt.Errorf("kubectl exited: %s", stderrStr)
		}
		return nil, fmt.Errorf("kubectl exited immediately (check VPN / kube context / port conflicts)")
	}

	// Do not treat initial stderr as fatal; kubectl may emit harmless warnings.
	stderrStr := strings.TrimSpace(stderr.String())
	if stderrStr != "" {
		logging.LogDebug("kubectl port-forward initial stderr (non-fatal, PID %d): %s", cmd.Process.Pid, stderrStr)
	}

	logging.LogDebug("Started port-forward process PID: %d, appears stable.", cmd.Process.Pid)
	return cmd, nil
}

// StopPortForward stops a port-forward process and all child processes it has spawned.
// Killing just cmd.Process is insufficient when kubectl has launched a child (e.g. an
// SSO exec-credential plugin that opens a browser), because that child holds the write
// end of the stderr pipe open, which causes cmd.Wait() to block forever.
// killCmdGroup kills the entire process group, closing every write-end holder and
// allowing Wait() to return immediately.
func StopPortForward(cmd *exec.Cmd) error {
	if cmd == nil || cmd.Process == nil {
		return nil
	}
	logging.LogDebug("Stopping port-forward process group (PID: %d)", cmd.Process.Pid)

	// Kill the whole process group (kubectl + any SSO subprocesses)
	_ = killCmdGroup(cmd)
	// Reap the process to avoid zombies
	_ = cmd.Wait()
	return nil
}

// Start attempts to start the port-forward for the config at the given index.
func (pf *PortForwarder) Start(index int, cfg config.PortForwardConfig) error {
	localPort := cfg.PortLocal // Get local port for checks

	pf.Mutex.Lock()
	if _, exists := pf.RunningForwards[index]; exists {
		logging.LogDebug("Port-forward for index %d already marked as running.", index)
		pf.Mutex.Unlock()
		return nil // Already running, not an error
	}

	// *** Check internal reservation first ***
	if conflictingIndex, reserved := pf.activeLocalPorts[localPort]; reserved {
		logging.LogError("Cannot start index %d: %v (port %d reserved by index %d)", index, ErrLocalPortReserved, localPort, conflictingIndex)
		pf.Mutex.Unlock()
		return ErrLocalPortReserved // Return specific error
	}

	// *** Reserve the port internally ***
	pf.activeLocalPorts[localPort] = index
	logging.LogDebug("Reserved local port %d for index %d", localPort, index)
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
		// Start failed, release the reservation and record error state
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == index {
			delete(pf.activeLocalPorts, localPort)
			logging.LogDebug("Released local port %d reservation for index %d due to start failure: %v", localPort, index, err)
		} else {
			logging.LogError("Could not release reservation for port %d (index %d) after start failure. Current holder: %d, Exists: %t", localPort, index, currentHolder, ok)
		}
		pf.failedForwards[index] = struct{}{}
		return err
	}

	if cmd != nil {
		// Start succeeded — clear any previous error and store running info
		delete(pf.failedForwards, index)
		pf.RunningForwards[index] = &runningInfo{cmd: cmd, localPort: localPort}
		logging.LogDebug("Successfully started and registered port-forward for index %d (PID: %d, Port: %d)", index, cmd.Process.Pid, localPort)
		return nil
	}
	// Unreachable if StartPortForward upholds its contract, but guard anyway
	if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == index {
		delete(pf.activeLocalPorts, localPort)
	}
	pf.failedForwards[index] = struct{}{}
	return fmt.Errorf("StartPortForward returned nil command without error for index %d", index)
}

// Stop attempts to stop the port-forward process for the given index.
func (pf *PortForwarder) Stop(index int) error {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()

	runningInfo, exists := pf.RunningForwards[index]
	if !exists {
		// Not running (or not tracked), do nothing
		logging.LogDebug("Stop: Port-forward for index %d not found or already stopped.", index)
		return nil
	}

	// Get local port from stored info
	localPort := runningInfo.localPort

	// Release internal reservation first
	if currentHolder, reserved := pf.activeLocalPorts[localPort]; reserved {
		if currentHolder == index {
			delete(pf.activeLocalPorts, localPort)
			logging.LogDebug("Stop: Released internal reservation for local port %d (index %d)", localPort, index)
		} else {
			// Log if reservation was held by someone else (indicates inconsistency)
			logging.LogError("Stop: Port %d reservation for index %d was held by index %d! Inconsistency?", localPort, index, currentHolder)
		}
	} else {
		// Log if no reservation existed (indicates inconsistency)
		logging.LogError("Stop: No internal reservation found for local port %d (index %d) during stop! Inconsistency?", localPort, index)
	}

	// Intentional stop — clear error state regardless of outcome
	delete(pf.failedForwards, index)

	err := StopPortForward(runningInfo.cmd)
	if err != nil {
		logging.LogError("Stop: Error stopping port-forward process for index %d (PID: %d, Port: %d): %v", index, runningInfo.cmd.Process.Pid, localPort, err)
	}

	delete(pf.RunningForwards, index)
	logging.LogDebug("Stop: Stopped and deregistered port-forward for index %d (Port: %d)", index, localPort)
	return err
}

// SyncPortForwards is now a no-op since status is not stored in config
// Port forwards are managed entirely through UI interactions (Start/Stop methods)
// This method is kept for interface compatibility but does nothing
func (pf *PortForwarder) SyncPortForwards(configs []config.PortForwardConfig) map[int]error {
	logging.LogDebug("SyncPortForwards called - no action taken (runtime state only)")
	return make(map[int]error) // Return empty map
}

// stopInternal stops a forward assuming the lock is already held.
func (pf *PortForwarder) stopInternal(index int) error {
	runningInfo, exists := pf.RunningForwards[index]
	if !exists {
		return nil
	}
	localPort := runningInfo.localPort
	if currentHolder, reserved := pf.activeLocalPorts[localPort]; reserved && currentHolder == index {
		delete(pf.activeLocalPorts, localPort)
	}
	delete(pf.failedForwards, index) // Intentional stop clears error state
	err := StopPortForward(runningInfo.cmd)
	delete(pf.RunningForwards, index)
	logging.LogDebug("stopInternal: Stopped index %d (Port: %d)", index, localPort)
	return err
}

// startInternal starts a forward assuming the lock is already held for map checks/updates.
func (pf *PortForwarder) startInternal(index int, cfg config.PortForwardConfig) error {
	localPort := cfg.PortLocal
	if _, exists := pf.RunningForwards[index]; exists {
		return nil // Already running
	}
	if conflictingIndex, reserved := pf.activeLocalPorts[localPort]; reserved {
		return fmt.Errorf("%w: port %d requested by index %d, but reserved by index %d", ErrLocalPortReserved, localPort, index, conflictingIndex)
	}

	// Reserve port
	pf.activeLocalPorts[localPort] = index
	logging.LogDebug("startInternal: Reserved port %d for index %d", localPort, index)

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
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == index {
			delete(pf.activeLocalPorts, localPort)
		}
		pf.failedForwards[index] = struct{}{}
		return err
	}
	if cmd != nil {
		delete(pf.failedForwards, index)
		pf.RunningForwards[index] = &runningInfo{cmd: cmd, localPort: localPort}
		return nil
	}
	if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == index {
		delete(pf.activeLocalPorts, localPort)
	}
	pf.failedForwards[index] = struct{}{}
	return fmt.Errorf("StartPortForward returned nil cmd/err for index %d", index)
}

// IsRunning checks if a port forward is currently running for the given index.
// It verifies the OS process is actually alive, not just that an entry exists in
// the map. If the process has exited since it was last seen (e.g. because the
// VPN dropped), the stale entry is removed and the zombie is reaped.
func (pf *PortForwarder) IsRunning(index int) bool {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()

	info, exists := pf.RunningForwards[index]
	if !exists {
		return false
	}

	if !isProcessAlive(info.cmd.Process) {
		// Process died unexpectedly (VPN drop, pod restart, etc.).
		// Mark as errored, clean up maps, and reap the zombie outside the lock.
		cmd := info.cmd
		if holder, ok := pf.activeLocalPorts[info.localPort]; ok && holder == index {
			delete(pf.activeLocalPorts, info.localPort)
		}
		delete(pf.RunningForwards, index)
		pf.failedForwards[index] = struct{}{}
		logging.LogDebug("IsRunning: process for index %d (port %d) exited unexpectedly; marked as error", index, info.localPort)
		go func() { _ = cmd.Wait() }()
		return false
	}

	return true
}

// IsError reports whether the port-forward at index has an error state — meaning
// it either failed to start or its process exited unexpectedly. Returns false once
// the forward is manually stopped (intentional stop is not an error) or successfully
// restarted.
func (pf *PortForwarder) IsError(index int) bool {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	_, failed := pf.failedForwards[index]
	return failed
}

// StopAllRunning stops every currently running port-forward and returns how many
// were stopped. Error state is cleared for each one (intentional action).
func (pf *PortForwarder) StopAllRunning() int {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()

	indices := make([]int, 0, len(pf.RunningForwards))
	for idx := range pf.RunningForwards {
		indices = append(indices, idx)
	}
	for _, idx := range indices {
		_ = pf.stopInternal(idx)
	}
	return len(indices)
}

// CleanupAll stops all port-forwards
func (pf *PortForwarder) CleanupAll() {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	indices := make([]int, 0, len(pf.RunningForwards))
	for idx := range pf.RunningForwards {
		indices = append(indices, idx)
	}
	for _, idx := range indices {
		logging.LogDebug("CleanupAll: Stopping index %d", idx)
		_ = pf.stopInternal(idx) // Call internal stop
	}
	pf.RunningForwards = make(map[int]*runningInfo)
	pf.activeLocalPorts = make(map[int]int)
	pf.failedForwards = make(map[int]struct{})
	logging.LogDebug("CleanupAll finished.")
}

// RestartResult represents the outcome of a restart operation
type RestartResult struct {
	RestartedCount int           // Number of port forwards restarted
	Errors         map[int]error // Errors by config index
}

// RestartRunningForwards restarts all currently running port forwards
// This is useful when network connectivity is lost (e.g., VPN disconnect)
func (pf *PortForwarder) RestartRunningForwards(configs []config.PortForwardConfig) *RestartResult {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()

	result := &RestartResult{
		RestartedCount: 0,
		Errors:         make(map[int]error),
	}

	// Get a snapshot of currently running indices
	runningIndices := []int{}
	for index := range pf.RunningForwards {
		runningIndices = append(runningIndices, index)
	}

	logging.LogDebug("RestartRunningForwards: Found %d running port forwards to restart", len(runningIndices))

	// Restart each running port forward
	for _, index := range runningIndices {
		// Get the config for this index
		if index >= len(configs) {
			logging.LogError("RestartRunningForwards: Index %d is out of bounds (configs length: %d)", index, len(configs))
			result.Errors[index] = fmt.Errorf("index %d out of bounds", index)
			continue
		}
		cfg := configs[index]

		logging.LogDebug("RestartRunningForwards: Restarting port forward %d (%s)", index, cfg.Service)

		// Stop the current port forward
		stopErr := pf.stopInternal(index)
		if stopErr != nil {
			logging.LogError("RestartRunningForwards: Failed to stop port forward %d: %v", index, stopErr)
			result.Errors[index] = fmt.Errorf("failed to stop: %w", stopErr)
			continue
		}

		// Start it again with the same config
		startErr := pf.startInternal(index, cfg)
		if startErr != nil {
			logging.LogError("RestartRunningForwards: Failed to start port forward %d: %v", index, startErr)
			result.Errors[index] = fmt.Errorf("failed to restart: %w", startErr)
			continue
		}

		result.RestartedCount++
		logging.LogDebug("RestartRunningForwards: Successfully restarted port forward %d (%s)", index, cfg.Service)
	}

	logging.LogDebug("RestartRunningForwards: Complete - Restarted: %d, Errors: %d", result.RestartedCount, len(result.Errors))
	return result
}
