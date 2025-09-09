package discovery

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/xlttj/kprtfwd/pkg/logging"
)

// RunDiscovery orchestrates the complete discovery process
func RunDiscovery(opts Options) error {
	logging.LogDebug("Running discovery with options: %+v", opts)

	// Step 1: Discover services
	result, err := DiscoverServices(opts)
	if err != nil {
		return fmt.Errorf("service discovery failed: %w", err)
	}

	if result.TotalCount == 0 {
		fmt.Printf("🔍 No services found matching criteria.\n")
		fmt.Printf("   Context: %s\n", result.Context)
		fmt.Printf("   Namespace filter: %s\n", result.NamespaceFilter)
		return nil
	}

	if opts.Verbose {
		fmt.Printf("\n🎯 Discovered %d service(s) total.\n\n", result.TotalCount)
	} else {
		fmt.Printf("🔍 Found %d service(s) in context '%s'\n\n", result.TotalCount, result.Context)
	}

	// Step 2: Select services
	err = selectServices(result, opts)
	if err != nil {
		return fmt.Errorf("service selection failed: %w", err)
	}

	if result.SelectedCount == 0 {
		fmt.Printf("No services selected. Exiting.\n")
		return nil
	}

	// Step 3: Generate and output configuration
	err = outputConfiguration(result, opts)
	if err != nil {
		return fmt.Errorf("configuration output failed: %w", err)
	}

	return nil
}

// selectServices handles the interactive selection process
func selectServices(result *DiscoveryResult, opts Options) error {
	if opts.AcceptAll {
		// Accept all services
		for i := range result.Services {
			result.Services[i].Selected = true
			result.SelectedCount++
		}

		if opts.Verbose {
			fmt.Printf("✅ Auto-selected all %d services (--accept-all enabled)\n\n", result.SelectedCount)
		}
		return nil
	}

	// Interactive selection
	reader := bufio.NewReader(os.Stdin)

	fmt.Printf("Select services to include in your configuration:\n")
	fmt.Printf("(Press Enter for [Y]es, 'n' for No, 'a' for All remaining, 'q' to Quit)\n\n")

	for i := range result.Services {
		service := &result.Services[i]

		// Display service information
		fmt.Printf("🔧 Service: %s\n", formatServiceDisplay(service))
		fmt.Printf("   Namespace: %s\n", service.ServiceInfo.Namespace)
		fmt.Printf("   Type: %s\n", service.ServiceInfo.Type)
		fmt.Printf("   Generated ID: %s\n", service.GeneratedID)

		// Show ports
		if len(service.ServiceInfo.Ports) == 1 {
			port := service.ServiceInfo.Ports[0]
			fmt.Printf("   Port: %d", port.Port)
			if port.Name != "" {
				fmt.Printf(" (%s)", port.Name)
			}
			if port.Protocol != "TCP" {
				fmt.Printf(" [%s]", port.Protocol)
			}
			fmt.Printf("\n")
		} else {
			fmt.Printf("   Ports: ")
			for j, port := range service.ServiceInfo.Ports {
				if j > 0 {
					fmt.Printf(", ")
				}
				fmt.Printf("%d", port.Port)
				if port.Name != "" {
					fmt.Printf("(%s)", port.Name)
				}
			}
			fmt.Printf("\n")
		}

		// Show relevant labels if available
		if service.ServiceInfo.Labels != nil {
			interestingLabels := []string{"app", "app.kubernetes.io/name", "app.kubernetes.io/component", "version", "tier"}
			var displayLabels []string
			for _, labelKey := range interestingLabels {
				if value, exists := service.ServiceInfo.Labels[labelKey]; exists {
					displayLabels = append(displayLabels, fmt.Sprintf("%s=%s", labelKey, value))
				}
			}
			if len(displayLabels) > 0 {
				fmt.Printf("   Labels: %s\n", strings.Join(displayLabels, ", "))
			}
		}

		// Prompt for selection
		fmt.Printf("\n❓ Include this service? [Y/n/a/q]: ")

		response, err := reader.ReadString('\n')
		if err != nil {
			return fmt.Errorf("failed to read user input: %w", err)
		}

		response = strings.TrimSpace(strings.ToLower(response))

		switch response {
		case "", "y", "yes":
			service.Selected = true
			result.SelectedCount++
			fmt.Printf("✅ Added: %s\n\n", service.GeneratedID)

		case "n", "no":
			fmt.Printf("⏭️  Skipped: %s\n\n", service.ServiceInfo.Name)

		case "a", "all":
			// Select this one and all remaining
			service.Selected = true
			result.SelectedCount++
			fmt.Printf("✅ Added: %s\n", service.GeneratedID)

			// Select all remaining services
			for j := i + 1; j < len(result.Services); j++ {
				result.Services[j].Selected = true
				result.SelectedCount++
				fmt.Printf("✅ Added: %s\n", result.Services[j].GeneratedID)
			}
			fmt.Printf("\n🎯 Selected all remaining services (%d total selected)\n\n", result.SelectedCount)
			break

		case "q", "quit":
			fmt.Printf("👋 Selection cancelled.\n")
			return fmt.Errorf("user cancelled selection")

		default:
			fmt.Printf("❌ Invalid response '%s'. Please use y/n/a/q.\n", response)
			i-- // Retry this service
			continue
		}
	}

	fmt.Printf("📊 Selection complete: %d out of %d services selected.\n\n", result.SelectedCount, result.TotalCount)
	return nil
}

// outputConfiguration generates and outputs the final configuration
func outputConfiguration(result *DiscoveryResult, opts Options) error {
	// Build the list of port forwards
	portForwards := result.GenerateConfig()

	portForwardCount := len(portForwards)
	if portForwardCount == 0 {
		fmt.Printf("No port forwards to generate.\n")
		return nil
	}

	// Prepare JSON export structure
	type jsonPF struct {
		ID         string `json:"id"`
		Context    string `json:"context"`
		Namespace  string `json:"namespace"`
		Service    string `json:"service"`
		PortRemote int    `json:"port_remote"`
		PortLocal  int    `json:"port_local"`
	}
	export := struct {
		Context         string   `json:"context"`
		NamespaceFilter string   `json:"namespace_filter"`
		PortForwards    []jsonPF `json:"port_forwards"`
	}{
		Context:         result.Context,
		NamespaceFilter: result.NamespaceFilter,
	}
	for _, pf := range portForwards {
		export.PortForwards = append(export.PortForwards, jsonPF{
			ID:         pf.ID,
			Context:    pf.Context,
			Namespace:  pf.Namespace,
			Service:    pf.Service,
			PortRemote: pf.PortRemote,
			PortLocal:  pf.PortLocal,
		})
	}

	jsonData, err := json.MarshalIndent(export, "", "  ")
	if err != nil {
		return fmt.Errorf("failed to marshal configuration to JSON: %w", err)
	}

	// Output to file or stdout
	if opts.OutputFile != "" {
		err = writeToFile(opts.OutputFile, string(jsonData))
		if err != nil {
			return fmt.Errorf("failed to write configuration file: %w", err)
		}

		fmt.Printf("💾 Export saved to: %s\n", opts.OutputFile)
		fmt.Printf("📋 Generated %d port forward configuration(s)\n", portForwardCount)
	} else {
		// Output to stdout
		fmt.Printf("%s\n", string(jsonData))
	}

	return nil
}

// writeToFile writes content to a file, creating directories if needed
func writeToFile(filename, content string) error {
	dir := filepath.Dir(filename)
	if err := os.MkdirAll(dir, 0700); err != nil {
		return fmt.Errorf("failed to create output directory %s: %w", dir, err)
	}
	file, err := os.OpenFile(filename, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0600)
	if err != nil {
		return fmt.Errorf("failed to create file %s: %w", filename, err)
	}
	defer file.Close()

	if _, err = file.WriteString(content); err != nil {
		return fmt.Errorf("failed to write to file %s: %w", filename, err)
	}
	return nil
}

// formatServiceDisplay creates a nice display name for a service
func formatServiceDisplay(service *DiscoveredService) string {
	name := service.ServiceInfo.Name

	// Add some visual indicators based on service type or common patterns
	if strings.Contains(strings.ToLower(name), "mysql") || strings.Contains(strings.ToLower(name), "mariadb") {
		return "🗃️  " + name
	} else if strings.Contains(strings.ToLower(name), "postgres") {
		return "🐘 " + name
	} else if strings.Contains(strings.ToLower(name), "redis") {
		return "🟥 " + name
	} else if strings.Contains(strings.ToLower(name), "mongo") {
		return "🍃 " + name
	} else if strings.Contains(strings.ToLower(name), "elasticsearch") || strings.Contains(strings.ToLower(name), "elastic") {
		return "🔍 " + name
	} else if strings.Contains(strings.ToLower(name), "kafka") {
		return "📡 " + name
	} else if strings.Contains(strings.ToLower(name), "rabbitmq") || strings.Contains(strings.ToLower(name), "rabbit") {
		return "🐰 " + name
	} else if strings.Contains(strings.ToLower(name), "api") {
		return "🌐 " + name
	} else if strings.Contains(strings.ToLower(name), "web") || strings.Contains(strings.ToLower(name), "frontend") {
		return "💻 " + name
	} else if strings.Contains(strings.ToLower(name), "grafana") {
		return "📊 " + name
	} else if strings.Contains(strings.ToLower(name), "prometheus") {
		return "📈 " + name
	}

	return "⚙️  " + name
}
