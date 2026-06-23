//go:build windows

package k8s

import (
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
