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

// startupProbeDelay is how long StartPortForward waits before checking that
// kubectl is still alive. It lets fast failures (VPN down, bad context, port
// conflict kubectl detects itself) surface synchronously as a start error
// instead of flickering Running until the next status tick.
const startupProbeDelay = 200 * time.Millisecond

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
	startedAt time.Time     // when the process was registered; used to grace-skip health probes
	stopping  bool          // set (under PortForwarder.Mutex) before an intentional kill
	done      chan struct{} // closed by the watcher once the process is reaped
}

// PortForwarder manages multiple port-forward processes.
// Forwards are keyed by config ID (stable across config list reordering),
// never by list index: indices shift when configs are added/removed/edited.
type PortForwarder struct {
	RunningForwards  map[string]*runningInfo // Map of config ID to running info
	activeLocalPorts map[int]string          // Map of active local port -> config ID
	failedForwards   map[string]string       // ID -> human-readable reason it exited unexpectedly or failed to start
	// Mutex protects the maps above. It must never be held across blocking
	// calls (spawning kubectl, waiting on a process); only the non-blocking
	// Kill signal may be sent while holding it.
	Mutex sync.Mutex
}

// NewPortForwarder creates a new port forwarder
func NewPortForwarder() *PortForwarder {
	return &PortForwarder{
		RunningForwards:  make(map[string]*runningInfo),
		activeLocalPorts: make(map[int]string),
		failedForwards:   make(map[string]string),
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

// validateParams rejects parameters that kubectl could parse as flags (or
// that cannot exist in a cluster) before they reach the command line.
// Defense-in-depth: namespace and service names originate from cluster
// output during discovery and are persisted in the local config store.
func validateParams(params PortForwardParams) error {
	if err := config.ValidateContextName(params.Context); err != nil {
		return err
	}
	if err := config.ValidateKubernetesName("namespace", params.Namespace); err != nil {
		return err
	}
	if err := config.ValidateKubernetesName("service", params.Service); err != nil {
		return err
	}
	if err := config.ValidatePort("local port", params.PortLocal); err != nil {
		return err
	}
	return config.ValidatePort("remote port", params.PortRemote)
}

// StartPortForward starts a port-forward for a specific set of parameters.
func StartPortForward(params PortForwardParams) (*exec.Cmd, error) {
	if err := validateParams(params); err != nil {
		logging.LogError("Refusing to start port-forward: %v", err)
		return nil, err
	}

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
	// spawns (SSO exec-credential plugins, browser launchers) can be killed as
	// a unit. Otherwise a child holding the stderr pipe open keeps cmd.Wait()
	// blocked forever. See portforward_proc_unix.go / portforward_proc_windows.go.
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

	// Fast-failure detection (VPN down, invalid context, port conflict kubectl
	// detects itself) is done by the caller via the watcher's done channel, not
	// here: cmd.ProcessState is only set after Wait, and a signal-0 liveness
	// check is defeated by zombies (an exited-but-unreaped process still
	// answers). The stderr buffer must not be read on this success path either —
	// exec's copy goroutine keeps writing to it until the process exits, so it
	// is only safe to read after Wait returns.
	logging.LogDebug("Started port-forward process PID: %d", cmd.Process.Pid)
	return cmd, nil
}

// drainStderr returns the captured stderr of a finished command. It must only
// be called after cmd.Wait() has returned, when exec's copy goroutine is done.
func drainStderr(cmd *exec.Cmd) string {
	if cmd == nil {
		return ""
	}
	if buf, ok := cmd.Stderr.(*bytes.Buffer); ok {
		return strings.TrimSpace(buf.String())
	}
	return ""
}

// killProcess terminates a port-forward process group without reaping it.
// The watcher goroutine owns cmd.Wait, so this must never Wait. Killing the
// whole group (not just kubectl) closes every write-end of the stderr pipe,
// including SSO credential subprocesses, so the watcher's Wait returns promptly.
func killProcess(cmd *exec.Cmd) error {
	if cmd == nil || cmd.Process == nil {
		return nil
	}
	logging.LogDebug("Killing port-forward process group PID: %d", cmd.Process.Pid)
	return killCmdGroup(cmd)
}

// watch reaps the kubectl process and cleans up tracking state when it exits
// without Stop having been called (VPN drop, expired credentials, pod
// deletion, ...). Exactly one watcher owns cmd.Wait per started process.
func (pf *PortForwarder) watch(id string, info *runningInfo) {
	err := info.cmd.Wait()
	// Clean up tracking state first, then signal done. Closing done last means
	// a waiter (Start's quick-exit check, RestartForwards) observes
	// fully-settled state — and a reaped process whose socket is released.
	pf.handleProcessExit(id, info, err)
	if info.done != nil {
		close(info.done)
	}
}

// handleProcessExit deregisters a forward whose process exited on its own and
// records it as errored so the UI can show an Error status.
func (pf *PortForwarder) handleProcessExit(id string, info *runningInfo, waitErr error) {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()

	if info.stopping {
		// Intentional stop; Stop/stopInternal already cleaned up the maps.
		return
	}
	if current, exists := pf.RunningForwards[id]; !exists || current != info {
		// The ID was deregistered (e.g. by MarkBroken) or is owned by a newer
		// process now; whoever superseded us is responsible for its state.
		return
	}
	delete(pf.RunningForwards, id)
	if holder, reserved := pf.activeLocalPorts[info.localPort]; reserved && holder == id {
		delete(pf.activeLocalPorts, info.localPort)
	}

	// Safe to read only now: exec's copy goroutine finished when Wait returned.
	stderrStr := drainStderr(info.cmd)
	reason := stderrStr
	if reason == "" {
		reason = fmt.Sprintf("kubectl exited unexpectedly (%v)", waitErr)
	}
	pf.failedForwards[id] = reason
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

	if err != nil || cmd == nil {
		// Start failed, release the reservation and record error state
		if currentHolder, ok := pf.activeLocalPorts[localPort]; ok && currentHolder == id {
			delete(pf.activeLocalPorts, localPort)
			logging.LogDebug("Released local port %d reservation for '%s' due to start failure: %v", localPort, id, err)
		} else {
			// Log if reservation was already gone or held by someone else (shouldn't happen ideally)
			logging.LogError("Could not release reservation for port %d ('%s') after start failure. Current holder: '%s', Exists: %t", localPort, id, currentHolder, ok)
		}
		if err != nil {
			pf.failedForwards[id] = err.Error()
			pf.Mutex.Unlock()
			logging.LogError("Failed to start port-forward '%s': %v", id, err)
			return err // Return the original error from StartPortForward
		}
		pf.failedForwards[id] = "kubectl did not start"
		pf.Mutex.Unlock()
		return fmt.Errorf("StartPortForward returned nil command without error for '%s'", id)
	}

	// Start succeeded — clear any previous error and register the forward.
	delete(pf.failedForwards, id)
	info := &runningInfo{cmd: cmd, localPort: localPort, startedAt: time.Now(), done: make(chan struct{})}
	pf.RunningForwards[id] = info
	go pf.watch(id, info)
	logging.LogDebug("Successfully started and registered port-forward for '%s' (PID: %d, Port: %d)", id, cmd.Process.Pid, localPort)
	pf.Mutex.Unlock()

	// Quick-exit detection: give kubectl a moment to fail fast (VPN down, bad
	// context, port conflict it detects itself). If the watcher reaps the
	// process within the grace window, it exited immediately — surface that as
	// a start error so the UI shows Error now instead of flickering Running
	// until the next status tick. The watcher has already marked it failed and
	// deregistered it (done is closed only after that cleanup completes).
	select {
	case <-info.done:
		if stderrStr := drainStderr(cmd); stderrStr != "" {
			return fmt.Errorf("kubectl exited: %s", stderrStr)
		}
		return fmt.Errorf("kubectl exited immediately (check VPN / kube context / port conflicts)")
	case <-time.After(startupProbeDelay):
		return nil // survived startup; treat as running
	}
}

// Stop attempts to stop the port-forward process for the given config ID.
func (pf *PortForwarder) Stop(id string) error {
	pf.Mutex.Lock()

	info, exists := pf.RunningForwards[id]
	if !exists {
		// Not running (or not tracked). Still clear any error state, since an
		// explicit stop is an intentional action.
		delete(pf.failedForwards, id)
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

	// Intentional stop clears error state.
	delete(pf.failedForwards, id)

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
		delete(pf.failedForwards, id) // intentional stop clears error state
		return nil                    // Already stopped
	}
	info.stopping = true
	localPort := info.localPort
	if currentHolder, reserved := pf.activeLocalPorts[localPort]; reserved && currentHolder == id {
		delete(pf.activeLocalPorts, localPort)
	}
	delete(pf.failedForwards, id) // intentional stop clears error state
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

// IsError reports whether the port-forward with the given ID is in an error
// state — it either failed to start or its process exited unexpectedly. The
// flag is cleared once the forward is intentionally stopped or restarts cleanly.
func (pf *PortForwarder) IsError(id string) bool {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	_, failed := pf.failedForwards[id]
	return failed
}

// ErrorReason returns the human-readable reason the forward with the given ID
// last failed (kubectl stderr, a start error, or a broken-tunnel notice), or
// the empty string if it is not in an error state.
func (pf *PortForwarder) ErrorReason(id string) string {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	return pf.failedForwards[id]
}

// StopAllRunning stops every currently running port-forward and returns how
// many were stopped. Error state is cleared for each (intentional action).
func (pf *PortForwarder) StopAllRunning() int {
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	ids := make([]string, 0, len(pf.RunningForwards))
	for id := range pf.RunningForwards {
		ids = append(ids, id)
	}
	for _, id := range ids {
		_ = pf.stopInternal(id)
	}
	return len(ids)
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
	pf.failedForwards = make(map[string]string)
	logging.LogDebug("CleanupAll finished.")
}

// isPortForwardHealthy dials localhost:localPort and determines whether kubectl's
// tunnel is live. A healthy tunnel: kubectl holds the connection open waiting to
// forward data → our read times out. A broken tunnel (VPN down, pod gone): kubectl
// closes the connection immediately → we get EOF. Connection refused means kubectl
// is no longer listening (also unhealthy).
//
// Limitation: silent packet-drop black-holes (VPN route gone, no RST) cannot be
// detected this way because kubectl still appears to hold the connection.
func isPortForwardHealthy(localPort int) bool {
	address := fmt.Sprintf("127.0.0.1:%d", localPort)
	conn, err := net.DialTimeout("tcp", address, 200*time.Millisecond)
	if err != nil {
		return false
	}
	defer conn.Close()
	_ = conn.SetReadDeadline(time.Now().Add(200 * time.Millisecond))
	buf := make([]byte, 1)
	_, err = conn.Read(buf)
	if err == nil {
		return true // received data — definitely healthy
	}
	var netErr net.Error
	if errors.As(err, &netErr) && netErr.Timeout() {
		return true // held open — tunnel is live
	}
	return false // EOF or other error — upstream unreachable
}

// ProbeAllTunnels checks every running forward's TCP tunnel health concurrently
// and returns the IDs of forwards whose tunnel appears broken. Forwards started
// within the grace period are skipped so a just-started tunnel isn't flagged
// before kubectl has finished establishing it. Blocking; call from a goroutine
// or tea.Cmd.
func (pf *PortForwarder) ProbeAllTunnels() []string {
	const probeGrace = 5 * time.Second // don't probe a forward that just started

	pf.Mutex.Lock()
	toProbe := make(map[string]int) // id → localPort
	for id, info := range pf.RunningForwards {
		if time.Since(info.startedAt) < probeGrace {
			continue
		}
		toProbe[id] = info.localPort
	}
	pf.Mutex.Unlock()

	if len(toProbe) == 0 {
		return nil
	}

	type result struct {
		id      string
		healthy bool
	}
	ch := make(chan result, len(toProbe))
	for id, port := range toProbe {
		go func(i string, p int) {
			ch <- result{i, isPortForwardHealthy(p)}
		}(id, port)
	}

	var broken []string
	for range toProbe {
		r := <-ch
		if !r.healthy {
			broken = append(broken, r.id)
		}
	}
	return broken
}

// MarkBroken kills and deregisters the forwards with the given IDs, marking each
// as errored. Used to record tunnels that the TCP health probe found broken.
// The killed process is reaped by its own watcher goroutine.
func (pf *PortForwarder) MarkBroken(ids []string) {
	if len(ids) == 0 {
		return
	}
	pf.Mutex.Lock()
	defer pf.Mutex.Unlock()
	for _, id := range ids {
		info, ok := pf.RunningForwards[id]
		if !ok {
			continue
		}
		if holder, ok2 := pf.activeLocalPorts[info.localPort]; ok2 && holder == id {
			delete(pf.activeLocalPorts, info.localPort)
		}
		delete(pf.RunningForwards, id)
		pf.failedForwards[id] = fmt.Sprintf("tunnel health check failed on local port %d (VPN down or upstream unreachable)", info.localPort)
		logging.LogError("MarkBroken: tunnel broken for '%s' (port %d); killing process", id, info.localPort)
		// Non-blocking kill under the lock (allowed by the mutex contract);
		// the forward's watcher owns Wait and will reap it, then see the entry
		// is gone and leave the error state we just set in place.
		_ = killProcess(info.cmd)
	}
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

// RestartForwards restarts every forward that is currently running OR in an
// error state. Running forwards are stopped and started again (useful after a
// VPN drop); errored forwards are simply (re)started so the user can recover a
// failed connection with Ctrl+R without first toggling it off. Intentionally
// stopped forwards are left alone.
func (pf *PortForwarder) RestartForwards(configs []config.PortForwardConfig) *RestartResult {
	result := &RestartResult{
		RestartedCount: 0,
		Errors:         make(map[string]error),
	}

	configsByID := make(map[string]config.PortForwardConfig, len(configs))
	for _, cfg := range configs {
		configsByID[cfg.ID] = cfg
	}

	// Snapshot the IDs to restart: running ∪ errored. Stop and Start manage
	// their own locking, so the mutex is not held across the blocking kubectl
	// calls below and other callers (UI status checks, watchers) stay responsive.
	pf.Mutex.Lock()
	targets := make(map[string]bool, len(pf.RunningForwards)+len(pf.failedForwards))
	for id := range pf.RunningForwards {
		targets[id] = true
	}
	for id := range pf.failedForwards {
		targets[id] = true
	}
	pf.Mutex.Unlock()

	logging.LogDebug("RestartForwards: Found %d port forwards to restart (running + errored)", len(targets))

	for id := range targets {
		cfg, found := configsByID[id]
		if !found {
			logging.LogError("RestartForwards: Config '%s' no longer exists", id)
			result.Errors[id] = fmt.Errorf("config '%s' no longer exists", id)
			continue
		}

		logging.LogDebug("RestartForwards: Restarting port forward '%s' (%s)", id, cfg.Service)

		// Grab the running info (nil for a purely-errored forward) so we can
		// wait for the old process to be reaped after Stop; starting again
		// while it still holds the local socket would trip the port pre-check.
		pf.Mutex.Lock()
		oldInfo := pf.RunningForwards[id]
		pf.Mutex.Unlock()

		// Stop clears any error state and, if running, kills the process. For a
		// purely-errored forward this is a cheap no-op that just clears the flag.
		stopErr := pf.Stop(id)
		if stopErr != nil {
			logging.LogError("RestartForwards: Failed to stop port forward '%s': %v", id, stopErr)
			result.Errors[id] = fmt.Errorf("failed to stop: %w", stopErr)
			continue
		}

		if oldInfo != nil && oldInfo.done != nil {
			select {
			case <-oldInfo.done:
			case <-time.After(processReapTimeout):
				logging.LogError("RestartForwards: Timed out waiting for '%s' to exit; attempting start anyway", id)
			}
		}

		// Start it again with the same config
		startErr := pf.Start(cfg)
		if startErr != nil {
			logging.LogError("RestartForwards: Failed to start port forward '%s': %v", id, startErr)
			result.Errors[id] = fmt.Errorf("failed to restart: %w", startErr)
			continue
		}

		result.RestartedCount++
		logging.LogDebug("RestartForwards: Successfully restarted port forward '%s' (%s)", id, cfg.Service)
	}

	logging.LogDebug("RestartForwards: Complete - Restarted: %d, Errors: %d", result.RestartedCount, len(result.Errors))
	return result
}
