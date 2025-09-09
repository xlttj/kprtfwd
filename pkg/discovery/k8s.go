package discovery

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"strings"
	"time"

	"github.com/xlttj/kprtfwd/pkg/logging"
)

// K8sService represents the JSON structure returned by kubectl get services
type K8sService struct {
	ApiVersion string `json:"apiVersion"`
	Kind       string `json:"kind"`
	Metadata   struct {
		Name        string            `json:"name"`
		Namespace   string            `json:"namespace"`
		Labels      map[string]string `json:"labels"`
		Annotations map[string]string `json:"annotations"`
	} `json:"metadata"`
	Spec struct {
		Type  string `json:"type"`
		Ports []struct {
			Name       string      `json:"name"`
			Port       int32       `json:"port"`
			Protocol   string      `json:"protocol"`
			TargetPort interface{} `json:"targetPort"` // Can be int or string
		} `json:"ports"`
	} `json:"spec"`
}

// K8sServiceList represents the JSON structure for a list of services
type K8sServiceList struct {
	ApiVersion string       `json:"apiVersion"`
	Kind       string       `json:"kind"`
	Items      []K8sService `json:"items"`
}

// DiscoverServices finds services in the specified Kubernetes context and namespaces
func DiscoverServices(opts Options) (*DiscoveryResult, error) {
	logging.LogDebug("Starting service discovery with options: %+v", opts)

	// Get the current context if none specified
	context := opts.Context
	if context == "" {
		currentContext, err := getCurrentContext()
		if err != nil {
			return nil, fmt.Errorf("failed to get current context: %w", err)
		}
		context = currentContext
	}

	// Discover namespaces that match the filter
	namespaces, err := discoverNamespaces(context, opts.NamespaceFilter)
	if err != nil {
		return nil, fmt.Errorf("failed to discover namespaces: %w", err)
	}

	if opts.Verbose {
		fmt.Printf("📋 Found %d matching namespace(s): %s\n", len(namespaces), strings.Join(namespaces, ", "))
	}

	// For efficiency with large clusters, get all services at once and filter by namespace
	// This is much faster than making individual calls for each namespace
	allServices, err := getAllServicesInContext(context)
	if err != nil {
		return nil, fmt.Errorf("failed to get services: %w", err)
	}

	// Filter services to only include those in matching namespaces
	namespacesSet := make(map[string]bool)
	for _, ns := range namespaces {
		namespacesSet[ns] = true
	}

	var filteredServices []ServiceInfo
	servicesByNamespace := make(map[string]int)
	for _, service := range allServices {
		if namespacesSet[service.Namespace] {
			filteredServices = append(filteredServices, service)
			servicesByNamespace[service.Namespace]++
		}
	}

	if opts.Verbose {
		for _, namespace := range namespaces {
			if count := servicesByNamespace[namespace]; count > 0 {
				fmt.Printf("   └─ %s: %d service(s)\n", namespace, count)
			}
		}
	}

	allServices = filteredServices

	if len(allServices) == 0 {
		return &DiscoveryResult{
			Services:        []DiscoveredService{},
			SelectedCount:   0,
			TotalCount:      0,
			Context:         context,
			NamespaceFilter: opts.NamespaceFilter,
		}, nil
	}

	// Convert to DiscoveredService format
	discoveredServices := make([]DiscoveredService, len(allServices))
	for i, service := range allServices {
		// Generate ID for this service (using first port for now)
		var generatedID string
		if len(service.Ports) > 0 {
			generatedID = generateServiceID(context, service, service.Ports[0])
		} else {
			generatedID = generateServiceID(context, service, ServicePort{Name: "default", Port: 80})
		}

		discoveredServices[i] = DiscoveredService{
			ServiceInfo: service,
			Selected:    false, // Will be set during selection process
			GeneratedID: generatedID,
		}
	}

	return &DiscoveryResult{
		Services:        discoveredServices,
		SelectedCount:   0,
		TotalCount:      len(discoveredServices),
		Context:         context,
		NamespaceFilter: opts.NamespaceFilter,
	}, nil
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

// discoverNamespaces finds namespaces matching the given filter pattern
func discoverNamespaces(kubeContext, filter string) ([]string, error) {
	// Create context with timeout
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	// Get all namespaces
	args := []string{"get", "namespaces", "-o", "jsonpath={.items[*].metadata.name}"}
	if kubeContext != "" {
		args = append([]string{"--context", kubeContext}, args...)
	}

	cmd := exec.CommandContext(ctx, "kubectl", args...)
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	err := cmd.Run()
	if err != nil {
		if ctx.Err() == context.DeadlineExceeded {
			return nil, fmt.Errorf("kubectl get namespaces timed out after 30 seconds")
		}
		return nil, fmt.Errorf("kubectl get namespaces failed: %w (stderr: %s)", err, stderr.String())
	}

	allNamespaces := strings.Fields(stdout.String())
	if len(allNamespaces) == 0 {
		return nil, fmt.Errorf("no namespaces found")
	}

	// Filter namespaces based on the pattern
	var matchingNamespaces []string
	for _, ns := range allNamespaces {
		if matchesWildcardPattern(ns, filter) {
			matchingNamespaces = append(matchingNamespaces, ns)
		}
	}

	if len(matchingNamespaces) == 0 {
		return nil, fmt.Errorf("no namespaces match pattern '%s'", filter)
	}

	return matchingNamespaces, nil
}

// getAllServicesInContext retrieves all services from all namespaces in a context
// This is much more efficient than calling getServicesInNamespace for each namespace individually
func getAllServicesInContext(kubeContext string) ([]ServiceInfo, error) {
	// Create context with timeout - use longer timeout since this gets all services
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	args := []string{"get", "services", "--all-namespaces", "-o", "json"}
	if kubeContext != "" {
		args = append([]string{"--context", kubeContext}, args...)
	}

	cmd := exec.CommandContext(ctx, "kubectl", args...)
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	err := cmd.Run()
	if err != nil {
		if ctx.Err() == context.DeadlineExceeded {
			return nil, fmt.Errorf("kubectl get services --all-namespaces timed out after 60 seconds")
		}
		return nil, fmt.Errorf("kubectl get services --all-namespaces failed: %w (stderr: %s)", err, stderr.String())
	}

	// Parse JSON response
	var serviceList K8sServiceList
	err = json.Unmarshal(stdout.Bytes(), &serviceList)
	if err != nil {
		return nil, fmt.Errorf("failed to parse kubectl output: %w", err)
	}

	// Convert to our ServiceInfo format
	var services []ServiceInfo
	for _, k8sService := range serviceList.Items {
		// Convert ports
		var ports []ServicePort
		for _, k8sPort := range k8sService.Spec.Ports {
			targetPort := ""
			if k8sPort.TargetPort != nil {
				switch tp := k8sPort.TargetPort.(type) {
				case float64:
					targetPort = fmt.Sprintf("%.0f", tp)
				case string:
					targetPort = tp
				default:
					targetPort = fmt.Sprintf("%v", tp)
				}
			}

			port := ServicePort{
				Name:       k8sPort.Name,
				Port:       k8sPort.Port,
				TargetPort: targetPort,
				Protocol:   k8sPort.Protocol,
			}
			ports = append(ports, port)
		}

		// Skip services without ports
		if len(ports) == 0 {
			continue
		}

		service := ServiceInfo{
			Name:        k8sService.Metadata.Name,
			Namespace:   k8sService.Metadata.Namespace,
			Ports:       ports,
			Labels:      k8sService.Metadata.Labels,
			Annotations: k8sService.Metadata.Annotations,
			Type:        k8sService.Spec.Type,
		}

		services = append(services, service)
	}

	return services, nil
}

// getServicesInNamespace retrieves all services from a specific namespace
func getServicesInNamespace(kubeContext, namespace string) ([]ServiceInfo, error) {
	// Create context with timeout
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	args := []string{"get", "services", "-n", namespace, "-o", "json"}
	if kubeContext != "" {
		args = append([]string{"--context", kubeContext}, args...)
	}

	cmd := exec.CommandContext(ctx, "kubectl", args...)
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	err := cmd.Run()
	if err != nil {
		if ctx.Err() == context.DeadlineExceeded {
			return nil, fmt.Errorf("kubectl get services in namespace %s timed out after 30 seconds", namespace)
		}
		return nil, fmt.Errorf("kubectl get services failed: %w (stderr: %s)", err, stderr.String())
	}

	// Parse JSON response
	var serviceList K8sServiceList
	err = json.Unmarshal(stdout.Bytes(), &serviceList)
	if err != nil {
		return nil, fmt.Errorf("failed to parse kubectl output: %w", err)
	}

	// Convert to our ServiceInfo format
	var services []ServiceInfo
	for _, k8sService := range serviceList.Items {
		// Convert ports
		var ports []ServicePort
		for _, k8sPort := range k8sService.Spec.Ports {
			targetPort := ""
			if k8sPort.TargetPort != nil {
				switch tp := k8sPort.TargetPort.(type) {
				case float64:
					targetPort = fmt.Sprintf("%.0f", tp)
				case string:
					targetPort = tp
				default:
					targetPort = fmt.Sprintf("%v", tp)
				}
			}

			port := ServicePort{
				Name:       k8sPort.Name,
				Port:       k8sPort.Port,
				TargetPort: targetPort,
				Protocol:   k8sPort.Protocol,
			}
			ports = append(ports, port)
		}

		// Skip services without ports
		if len(ports) == 0 {
			continue
		}

		service := ServiceInfo{
			Name:        k8sService.Metadata.Name,
			Namespace:   k8sService.Metadata.Namespace,
			Ports:       ports,
			Labels:      k8sService.Metadata.Labels,
			Annotations: k8sService.Metadata.Annotations,
			Type:        k8sService.Spec.Type,
		}

		services = append(services, service)
	}

	return services, nil
}

// matchesWildcardPattern checks if a string matches a wildcard pattern
// Supports * at the beginning, end, or both
func matchesWildcardPattern(text, pattern string) bool {
	if pattern == "*" {
		return true
	}

	if pattern == "" {
		return text == ""
	}

	// Handle patterns like "prefix-*"
	if strings.HasSuffix(pattern, "*") && !strings.HasPrefix(pattern, "*") {
		prefix := pattern[:len(pattern)-1]
		return strings.HasPrefix(text, prefix)
	}

	// Handle patterns like "*-suffix"
	if strings.HasPrefix(pattern, "*") && !strings.HasSuffix(pattern, "*") {
		suffix := pattern[1:]
		return strings.HasSuffix(text, suffix)
	}

	// Handle patterns like "*middle*"
	if strings.HasPrefix(pattern, "*") && strings.HasSuffix(pattern, "*") {
		middle := pattern[1 : len(pattern)-1]
		return strings.Contains(text, middle)
	}

	// No wildcards - exact match
	return text == pattern
}
