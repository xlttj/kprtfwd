package config

import (
	"strings"
	"testing"
)

func TestValidateKubernetesName(t *testing.T) {
	valid := []string{"a", "web", "my-service", "svc-2", "a1b2", strings.Repeat("a", 63)}
	for _, name := range valid {
		if err := ValidateKubernetesName("service", name); err != nil {
			t.Errorf("expected %q to be valid, got: %v", name, err)
		}
	}

	invalid := []string{
		"",                         // empty
		"--help",                   // flag injection
		"-web",                     // leading dash
		"web-",                     // trailing dash
		"Web",                      // uppercase
		"my service",               // whitespace
		"svc/extra",                // path separator
		"svc\nextra",               // newline
		"svc;rm -rf",               // shell metacharacters
		strings.Repeat("a", 64),    // too long
		"--kubeconfig=/tmp/stolen", // option injection
	}
	for _, name := range invalid {
		if err := ValidateKubernetesName("service", name); err == nil {
			t.Errorf("expected %q to be rejected", name)
		}
	}
}

func TestValidateContextName(t *testing.T) {
	valid := []string{
		"", // empty means current context
		"minikube",
		"arn:aws:eks:eu-west-1:123456789012:cluster/prod",
		"gke_my-project_europe-west1_cluster",
		"user@cluster.example.com",
	}
	for _, name := range valid {
		if err := ValidateContextName(name); err != nil {
			t.Errorf("expected context %q to be valid, got: %v", name, err)
		}
	}

	invalid := []string{
		"-oyaml",         // flag injection
		"--kubeconfig",   // option injection
		"ctx with space", // whitespace
		"ctx\nnewline",   // control characters
		"ctx\ttab",
	}
	for _, name := range invalid {
		if err := ValidateContextName(name); err == nil {
			t.Errorf("expected context %q to be rejected", name)
		}
	}
}

func TestValidatePort(t *testing.T) {
	for _, port := range []int{1, 80, 8080, 65535} {
		if err := ValidatePort("port", port); err != nil {
			t.Errorf("expected port %d to be valid, got: %v", port, err)
		}
	}
	for _, port := range []int{0, -1, 65536, 1 << 20} {
		if err := ValidatePort("port", port); err == nil {
			t.Errorf("expected port %d to be rejected", port)
		}
	}
}
