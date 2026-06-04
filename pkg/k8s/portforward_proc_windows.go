//go:build windows

package k8s

import (
	"os"
	"os/exec"
)

// setProcGroupAttrs is a no-op on Windows; process group management is handled
// differently (Job Objects) and kubectl on Windows doesn't spawn SSO plugins
// that hold pipe file descriptors open in the same way.
func setProcGroupAttrs(cmd *exec.Cmd) {}

// killCmdGroup falls back to killing just the process on Windows.
func killCmdGroup(cmd *exec.Cmd) error {
	if cmd.Process == nil {
		return nil
	}
	return cmd.Process.Kill()
}

// isProcessAlive reports whether proc appears to be running on Windows.
// Windows doesn't support POSIX signal 0, so we use a conservative heuristic:
// the process is considered alive as long as we have a non-nil handle.
// Quick-exit detection on Windows relies on the startup probe's wait timeout.
func isProcessAlive(proc *os.Process) bool {
	return proc != nil
}
