package k8s

import (
	"bytes"
	"errors"
	"fmt"
	"net"
	"os/exec"
	"strings"
	"sync"

	"kprtfwd/pkg/config"
	"kprtfwd/pkg/logging"
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
	// RunningForwards map[int]*exec.Cmd // Old map
	RunningForwards  map[int]*runningInfo // Map of config index to running info
	activeLocalPorts map[int]int          // Map of active local port -> config index
	Mutex            sync.Mutex           // Mutex to protect the maps
}

// NewPortForwarder creates a new port forwarder
func NewPortForwarder() *PortForwarder {
	return &PortForwarder{
		// RunningForwards: make(map[int]*exec.Cmd),
		RunningForwards:  make(map[int]*runningInfo),
		activeLocalPorts: make(map[int]int),
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

	// Check if anything was written to stderr
	stderrStr := stderr.String()
	if stderrStr != "" {
		logging.LogError("Port-forward process (PID: %d) produced stderr output shortly after start: %s", cmd.Process.Pid, stderrStr)
		_ = StopPortForward(cmd)
		// Return the error directly, maybe wrap it? Or use a specific error type?
		// For now, keep previous fix but consider wrapping later.
		return nil, fmt.Errorf("%s", strings.TrimSpace(stderrStr)) // Keep using %s format
	}

	// If we reach here, Start() succeeded, process didn't exit quickly, and no stderr output yet.
	logging.LogDebug("Started port-forward process PID: %d, appears stable.", cmd.Process.Pid) // Updated log message
	return cmd, nil
}

// StopPortForward stops a port-forward process
func StopPortForward(cmd *exec.Cmd) error {
	if cmd == nil || cmd.Process == nil {
		return nil
	}
	logging.LogDebug("Stopping port-forward process PID: %d", cmd.Process.Pid)

	return cmd.Process.Kill()
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
		// Start failed, release the reservation
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == index {
			delete(pf.activeLocalPorts, localPort)
			logging.LogDebug("Released local port %d reservation for index %d due to start failure: %v", localPort, index, err)
		} else {
			// Log if reservation was already gone or held by someone else (shouldn't happen ideally)
			logging.LogError("Could not release reservation for port %d (index %d) after start failure. Current holder: %d, Exists: %t", localPort, index, currentHolder, ok)
		}
		return err // Return the original error from StartPortForward
	}

	if cmd != nil {
		// Start succeeded, store running info
		pf.RunningForwards[index] = &runningInfo{cmd: cmd, localPort: localPort}
		// Reservation in activeLocalPorts remains
		logging.LogDebug("Successfully started and registered port-forward for index %d (PID: %d, Port: %d)", index, cmd.Process.Pid, localPort)
		return nil // Success
	} else {
		// Should not happen if StartPortForward only returns nil cmd with non-nil error
		// Release reservation as a precaution
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == index {
			delete(pf.activeLocalPorts, localPort)
			logging.LogDebug("Released local port %d reservation for index %d due to nil cmd/err", localPort, index)
		}
		return fmt.Errorf("StartPortForward returned nil command without error for index %d", index)
	}
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

	// Now stop the actual process
	err := StopPortForward(runningInfo.cmd) // Call helper
	if err != nil {
		// Log the error but proceed to remove from map
		logging.LogError("Stop: Error stopping port-forward process for index %d (PID: %d, Port: %d): %v", index, runningInfo.cmd.Process.Pid, localPort, err)
	}

	// Remove from running map
	delete(pf.RunningForwards, index)
	logging.LogDebug("Stop: Stopped and deregistered port-forward for index %d (Port: %d)", index, localPort)
	return err // Return the error from StopPortForward helper
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
		return nil // Already stopped
	}
	localPort := runningInfo.localPort
	if currentHolder, reserved := pf.activeLocalPorts[localPort]; reserved && currentHolder == index {
		delete(pf.activeLocalPorts, localPort)
	}
	err := StopPortForward(runningInfo.cmd) // Call helper
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
		// Failed, release reservation
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == index {
			delete(pf.activeLocalPorts, localPort)
		}
		return err // Return error from helper
	}
	if cmd != nil {
		// Succeeded, add to running map
		pf.RunningForwards[index] = &runningInfo{cmd: cmd, localPort: localPort}
		return nil // Success
	} else {
		// Failed (nil cmd/err case), release reservation
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == index {
			delete(pf.activeLocalPorts, localPort)
		}
		return fmt.Errorf("StartPortForward returned nil cmd/err for index %d", index)
	}
}

// IsRunning checks if a port forward is currently running for the given index
func (pf *PortForwarder) IsRunning(index int) bool {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	_, exists := pf.RunningForwards[index]
	return exists
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
	logging.LogDebug("CleanupAll finished.")
}

// ReloadResult represents the outcome of a configuration reload operation
type ReloadResult struct {
	Stopped []int            // Indices of stopped port forwards
	Started []int            // Indices of started port forwards  
	Updated []int            // Indices of updated port forwards
	Errors  map[int]error    // Errors by config index
}

// ReloadSync performs intelligent synchronization during config reload
// Returns changes made and any errors  
func (pf *PortForwarder) ReloadSync(oldConfigs, newConfigs []config.PortForwardConfig) (*ReloadResult, error) {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()

	result := &ReloadResult{
		Stopped: []int{},
		Started: []int{}, 
		Updated: []int{},
		Errors:  make(map[int]error),
	}

	logging.LogDebug("ReloadSync: Starting with %d old configs, %d new configs", len(oldConfigs), len(newConfigs))
	
	// Debug: Log all old and new configs
	for i, cfg := range oldConfigs {
		logging.LogDebug("OLD Config[%d]: ID=%s, Context=%s, NS=%s, Svc=%s, Ports=%d:%d", 
			i, cfg.ID, cfg.Context, cfg.Namespace, cfg.Service, cfg.PortLocal, cfg.PortRemote)
	}
	for i, cfg := range newConfigs {
		logging.LogDebug("NEW Config[%d]: ID=%s, Context=%s, NS=%s, Svc=%s, Ports=%d:%d", 
			i, cfg.ID, cfg.Context, cfg.Namespace, cfg.Service, cfg.PortLocal, cfg.PortRemote)
	}

	// Use a simpler approach: direct positional comparison but with smarter logic
	// This avoids issues with duplicate configs having the same identity keys

	// Phase 1: Handle configs that exist in both old and new by index
	maxLen := max(len(oldConfigs), len(newConfigs))
	for i := 0; i < maxLen; i++ {
		var oldCfg *config.PortForwardConfig
		var newCfg *config.PortForwardConfig
		
		if i < len(oldConfigs) {
			oldCfg = &oldConfigs[i]
		}
		if i < len(newConfigs) {
			newCfg = &newConfigs[i]
		}

		if oldCfg != nil && newCfg != nil {
			// Both old and new exist - check for changes
			paramsChanged := configParamsChanged(*oldCfg, *newCfg)
			logging.LogDebug("ReloadSync: configParamsChanged(%d) = %v", i, paramsChanged)
			if paramsChanged {
				// Core parameters changed - need to restart if currently running
				logging.LogDebug("ReloadSync: Core parameters changed for index %d", i)
				if _, isRunning := pf.RunningForwards[i]; isRunning {
					logging.LogDebug("ReloadSync: Parameters changed for index %d, stopping old version", i)
					err := pf.stopInternal(i)
					if err != nil {
						result.Errors[i] = fmt.Errorf("failed to stop changed config: %w", err)
					} else {
						result.Stopped = append(result.Stopped, i)
					}
				}
				// Don't auto-start the new version - user controls runtime state via UI
				logging.LogDebug("ReloadSync: Config parameters updated at index %d (user can restart via UI)", i)
				result.Updated = append(result.Updated, i)
			} else {
				// Core parameters unchanged - preserve current runtime state
				logging.LogDebug("ReloadSync: Config at index %d unchanged - preserving current runtime state", i)
				// No action taken - preserve whatever is currently running
			}
		} else if oldCfg != nil {
			// Old config exists but new doesn't - config was removed
			if _, isRunning := pf.RunningForwards[i]; isRunning {
				logging.LogDebug("ReloadSync: Config removed at index %d, stopping running forward", i)
				err := pf.stopInternal(i)
				if err != nil {
					result.Errors[i] = fmt.Errorf("failed to stop removed config: %w", err)
				} else {
					result.Stopped = append(result.Stopped, i)
				}
			} else {
				logging.LogDebug("ReloadSync: Config removed at index %d (was not running)", i)
			}
		} else if newCfg != nil {
			// New config exists but old doesn't - config was added
			logging.LogDebug("ReloadSync: New config added at index %d (user can start via UI)", i)
			// Don't auto-start new configs - user controls runtime state via UI
		}
	}

	logging.LogDebug("ReloadSync: Complete - Stopped: %d, Started: %d, Updated: %d, Errors: %d", 
		len(result.Stopped), len(result.Started), len(result.Updated), len(result.Errors))
	return result, nil
}

// configParamsChanged checks if the core parameters of a config changed (excluding status)
func configParamsChanged(old, new config.PortForwardConfig) bool {
	return old.Context != new.Context ||
		old.Namespace != new.Namespace ||
		old.Service != new.Service ||
		old.PortRemote != new.PortRemote ||
		old.PortLocal != new.PortLocal ||
		old.ID != new.ID
}

// configsExactlyEqual checks if two configs are completely identical
func configsExactlyEqual(old, new config.PortForwardConfig) bool {
	return old.Context == new.Context &&
		old.Namespace == new.Namespace &&
		old.Service == new.Service &&
		old.PortRemote == new.PortRemote &&
		old.PortLocal == new.PortLocal &&
		old.ID == new.ID
}

// configIdentityKey creates a unique identity key for a config based on its core parameters
// This key is used to match configs across reloads regardless of their position in the array
// Note: For configs to be considered "the same", they must have identical parameters
func configIdentityKey(cfg config.PortForwardConfig) string {
	return fmt.Sprintf("%s|%s|%s|%d|%d|%s", 
		cfg.Context, cfg.Namespace, cfg.Service, cfg.PortRemote, cfg.PortLocal, cfg.ID)
}
