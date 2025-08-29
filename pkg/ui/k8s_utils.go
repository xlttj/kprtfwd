package ui

import (
	"bytes"
	"context"
	"fmt"
	"os/exec"
	"strings"
	"time"
)

// getAvailableClusters returns a list of available Kubernetes contexts
func getAvailableClusters() ([]string, error) {
	// Create context with timeout
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	cmd := exec.CommandContext(ctx, "kubectl", "config", "get-contexts", "-o", "name")
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	err := cmd.Run()
	if err != nil {
		if ctx.Err() == context.DeadlineExceeded {
			return nil, fmt.Errorf("kubectl get-contexts timed out after 10 seconds")
		}
		return nil, fmt.Errorf("kubectl get-contexts failed: %w (stderr: %s)", err, stderr.String())
	}

	contexts := strings.Fields(stdout.String())
	if len(contexts) == 0 {
		return nil, fmt.Errorf("no Kubernetes contexts found")
	}

	return contexts, nil
}

// getCurrentContext gets the current kubectl context
func getCurrentContext() (string, error) {
	// Create context with timeout
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	cmd := exec.CommandContext(ctx, "kubectl", "config", "current-context")
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	err := cmd.Run()
	if err != nil {
		if ctx.Err() == context.DeadlineExceeded {
			return "", fmt.Errorf("kubectl current-context timed out after 10 seconds")
		}
		return "", fmt.Errorf("kubectl current-context failed: %w (stderr: %s)", err, stderr.String())
	}

	context := strings.TrimSpace(stdout.String())
	if context == "" {
		return "", fmt.Errorf("no current context set")
	}

	return context, nil
}
