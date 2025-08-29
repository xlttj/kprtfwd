package discovery

import (
	"kprtfwd/pkg/config"
)

// Options holds the configuration for service discovery
type Options struct {
	NamespaceFilter string // Wildcard filter for namespaces (e.g., "my-app-*")
	Context         string // Kubernetes context to use
	OutputFile      string // Output file path (empty = stdout)
	AcceptAll       bool   // Accept all services without prompting
	Verbose         bool   // Enable verbose output
}

// ServiceInfo represents a discovered Kubernetes service
type ServiceInfo struct {
	Name        string            // Service name
	Namespace   string            // Service namespace
	Ports       []ServicePort     // Available ports
	Labels      map[string]string // Service labels
	Annotations map[string]string // Service annotations
	Type        string            // Service type (ClusterIP, NodePort, LoadBalancer, etc.)
}

// ServicePort represents a port on a Kubernetes service
type ServicePort struct {
	Name       string // Port name (optional)
	Port       int32  // Service port
	TargetPort string // Target port (can be int or string)
	Protocol   string // Protocol (TCP, UDP, etc.)
}

// DiscoveredService represents a service that was found and potentially selected
type DiscoveredService struct {
	ServiceInfo ServiceInfo
	Selected    bool
	GeneratedID string // The human-readable ID we'll generate
}

// DiscoveryResult holds the results of the discovery process
type DiscoveryResult struct {
	Services        []DiscoveredService
	SelectedCount   int
	TotalCount      int
	Context         string
	NamespaceFilter string
}

// GenerateConfig creates a list of PortForwardConfig from selected services
func (dr *DiscoveryResult) GenerateConfig() []config.PortForwardConfig {
	var portForwards []config.PortForwardConfig

	for _, discovered := range dr.Services {
		if !discovered.Selected {
			continue
		}

		service := discovered.ServiceInfo

		// For each port on the service, create a port forward config
		for _, port := range service.Ports {
			// Try to determine a good local port
			localPort := int(port.Port)

			// Generate a unique ID
			id := generateServiceID(dr.Context, service, port)

			portForward := config.PortForwardConfig{
				ID:         id,
				Context:    dr.Context,
				Namespace:  service.Namespace,
				Service:    service.Name,
				PortRemote: int(port.Port),
				PortLocal:  localPort,
			}

			portForwards = append(portForwards, portForward)
		}
	}

	return portForwards
}

// generateServiceID creates a human-readable ID following the pattern:
// <context>.<service-type>.<discriminator>
func generateServiceID(context string, service ServiceInfo, port ServicePort) string {
	// Clean context name
	contextPart := sanitizeIDPart(context)

	// Determine service type from labels, annotations, or service name
	serviceType := detectServiceType(service)

	// Create discriminator from service name and optionally port name
	discriminator := sanitizeIDPart(service.Name)
	if port.Name != "" && port.Name != "http" && port.Name != "tcp" {
		discriminator += "-" + sanitizeIDPart(port.Name)
	}

	return contextPart + "." + serviceType + "." + discriminator
}

// detectServiceType attempts to identify the type of service based on common patterns
func detectServiceType(service ServiceInfo) string {
	serviceName := service.Name
	labels := service.Labels

	// Check common service types in labels first
	if labels != nil {
		if app, exists := labels["app"]; exists {
			return sanitizeIDPart(app)
		}
		if component, exists := labels["app.kubernetes.io/component"]; exists {
			return sanitizeIDPart(component)
		}
		if tier, exists := labels["tier"]; exists {
			return sanitizeIDPart(tier)
		}
	}

	// Fallback to parsing service name for common patterns
	commonTypes := []string{
		"mysql", "postgres", "postgresql", "redis", "mongodb", "mongo",
		"elasticsearch", "rabbitmq", "kafka", "zookeeper",
		"api", "web", "frontend", "backend", "service", "app",
		"grafana", "prometheus", "jaeger", "zipkin",
	}

	nameLower := serviceName
	for _, serviceType := range commonTypes {
		if contains(nameLower, serviceType) {
			return serviceType
		}
	}

	// Last resort: use "service" as default
	return "service"
}

// Helper functions
func sanitizeIDPart(input string) string {
	// Replace common separators and invalid characters with hyphens
	result := ""
	for _, char := range input {
		if (char >= 'a' && char <= 'z') ||
			(char >= 'A' && char <= 'Z') ||
			(char >= '0' && char <= '9') {
			result += string(char)
		} else if char == '-' || char == '_' || char == '.' {
			if len(result) > 0 && result[len(result)-1] != '-' {
				result += "-"
			}
		}
	}

	// Remove trailing hyphens
	for len(result) > 0 && result[len(result)-1] == '-' {
		result = result[:len(result)-1]
	}

	if result == "" {
		result = "unknown"
	}

	return result
}

func contains(str, substr string) bool {
	return len(str) >= len(substr) && (str == substr ||
		(len(str) > len(substr) &&
			(str[:len(substr)] == substr ||
				str[len(str)-len(substr):] == substr ||
				findSubstring(str, substr))))
}

func findSubstring(str, substr string) bool {
	for i := 0; i <= len(str)-len(substr); i++ {
		if str[i:i+len(substr)] == substr {
			return true
		}
	}
	return false
}
