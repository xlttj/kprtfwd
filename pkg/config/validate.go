package config

import (
	"fmt"
	"regexp"
	"strings"
)

// dns1123LabelRegexp matches valid Kubernetes resource names (RFC 1123
// label): lowercase alphanumerics and '-', starting and ending with an
// alphanumeric.
var dns1123LabelRegexp = regexp.MustCompile(`^[a-z0-9]([-a-z0-9]*[a-z0-9])?$`)

const dns1123LabelMaxLen = 63

// ValidateKubernetesName checks that a namespace or service name is a valid
// RFC 1123 label. Anything else cannot exist in a cluster, so a non-matching
// value can only be corrupt data or an attempt to smuggle flags onto the
// kubectl command line.
func ValidateKubernetesName(kind, name string) error {
	if name == "" {
		return fmt.Errorf("%s must not be empty", kind)
	}
	if len(name) > dns1123LabelMaxLen {
		return fmt.Errorf("%s %q exceeds %d characters", kind, name, dns1123LabelMaxLen)
	}
	if !dns1123LabelRegexp.MatchString(name) {
		return fmt.Errorf("%s %q is not a valid Kubernetes name", kind, name)
	}
	return nil
}

// ValidateContextName checks that a kubectl context name is safe to place on
// a command line. Context names are user-defined and may legitimately contain
// characters like ':', '/', '@' (EKS ARNs, GKE contexts), so only values that
// kubectl would parse as a flag or that contain whitespace/control bytes are
// rejected. An empty name is allowed and means "use the current context".
func ValidateContextName(name string) error {
	if name == "" {
		return nil
	}
	if strings.HasPrefix(name, "-") {
		return fmt.Errorf("context %q must not start with '-'", name)
	}
	for _, r := range name {
		if r <= 0x20 || r == 0x7f {
			return fmt.Errorf("context %q contains whitespace or control characters", name)
		}
	}
	return nil
}

// ValidatePort checks that a port number is in the valid TCP range.
func ValidatePort(kind string, port int) error {
	if port < 1 || port > 65535 {
		return fmt.Errorf("%s %d is out of range (1-65535)", kind, port)
	}
	return nil
}
