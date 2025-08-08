package discovery

import (
	"bufio"
	"fmt"
	"os"
	"strings"

	"kprtfwd/pkg/logging"
	"gopkg.in/yaml.v3"
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
	// Generate the configuration
	configFile := result.GenerateConfig()
	
	// Check both fields for count (since we use PortForwardsOld for output)
	portForwardCount := len(configFile.PortForwards) + len(configFile.PortForwardsOld)
	
	if portForwardCount == 0 {
		fmt.Printf("No port forwards to generate.\n")
		return nil
	}

	// Convert to YAML
	yamlData, err := yaml.Marshal(configFile)
	if err != nil {
		return fmt.Errorf("failed to marshal configuration to YAML: %w", err)
	}

	// Add a header comment
	header := fmt.Sprintf("# Generated by kprtfwd discover\n# Context: %s\n# Namespace filter: %s\n# Generated %d port forward(s) from %d selected service(s)\n\n", 
		result.Context, result.NamespaceFilter, portForwardCount, result.SelectedCount)
	
	fullYaml := header + string(yamlData)

	// Output to file or stdout
	if opts.OutputFile != "" {
		err = writeToFile(opts.OutputFile, fullYaml)
		if err != nil {
			return fmt.Errorf("failed to write configuration file: %w", err)
		}
		
		fmt.Printf("💾 Configuration saved to: %s\n", opts.OutputFile)
		fmt.Printf("📋 Generated %d port forward configuration(s)\n", portForwardCount)
		
		if opts.Verbose {
			fmt.Printf("\nTo use this configuration:\n")
			fmt.Printf("  cp %s ~/.kprtfwd/config.yaml\n", opts.OutputFile)
			fmt.Printf("  # or merge with existing config\n")
			fmt.Printf("  kprtfwd\n")
		}
	} else {
		// Output to stdout
		fmt.Printf("# Generated configuration:\n")
		fmt.Printf("%s", fullYaml)
	}

	return nil
}

// writeToFile writes content to a file, creating directories if needed
func writeToFile(filename, content string) error {
	file, err := os.Create(filename)
	if err != nil {
		return fmt.Errorf("failed to create file %s: %w", filename, err)
	}
	defer file.Close()

	_, err = file.WriteString(content)
	if err != nil {
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
