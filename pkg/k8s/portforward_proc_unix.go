//go:build !windows

package k8s

import (
	"os/exec"
	"syscall"
)

// setProcGroupAttrs puts cmd in its own process group when it starts.
// This ensures kubectl's child processes (e.g. SSO exec-credential plugins
// that open a browser for authentication) belong to the same group, so we
// can kill the whole group atomically on stop rather than just kubectl itself.
func setProcGroupAttrs(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
}

// killCmdGroup kills the entire process group that cmd belongs to.
// With Setpgid=true the group ID equals cmd.Process.Pid, so this kills
// kubectl and all children it has spawned (credential helpers, browser
// launchers, etc.), closing every write-end holder of the stderr pipe and
// allowing cmd.Wait() to return promptly.
func killCmdGroup(cmd *exec.Cmd) error {
	if cmd.Process == nil {
		return nil
	}
	return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
}
