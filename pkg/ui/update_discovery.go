package ui

import (
	"fmt"
	"strings"

	"github.com/xlttj/kprtfwd/pkg/config"
	"github.com/xlttj/kprtfwd/pkg/discovery"
	"github.com/xlttj/kprtfwd/pkg/logging"

	"github.com/charmbracelet/bubbles/table"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// updateServiceDiscovery handles updates in the service discovery view
func (m *Model) updateServiceDiscovery(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	keyStr := msg.String()

	// Handle edit mode for local port editing
	if m.discoveryPhase == PhaseServiceSelection && m.discoveryEditMode {
		return m.handleDiscoveryEditMode(msg)
	}

	// Handle filter mode for service selection phase
	if m.discoveryPhase == PhaseServiceSelection && m.discoveryFilterMode {
		switch keyStr {
		case "esc":
			// Exit filter mode
			m.discoveryFilterMode = false
			m.discoveryFilterInput.Blur()
			m.discoveryFilterInput.SetValue("")
			m.refreshDiscoveryTable()
			m.discoveryTable.Focus()
			return m, nil
		case "enter":
			// Exit filter mode but keep filter applied
			m.discoveryFilterMode = false
			m.discoveryFilterInput.Blur()
			m.discoveryTable.Focus()
			return m, nil
		default:
			// Update filter input and apply filter
			var cmd tea.Cmd
			m.discoveryFilterInput, cmd = m.discoveryFilterInput.Update(msg)
			// No need to call applyDiscoveryFilter() - the refreshDiscoveryTable() will handle filtering
			m.refreshDiscoveryTable()
			return m, cmd
		}
	}

	// Directly handle space via KeyType (more reliable across terminals)
	if m.discoveryPhase == PhaseServiceSelection && !m.discoveryEditMode && !m.discoveryFilterMode {
		if msg.Type == tea.KeySpace {
			return m.handleServiceToggle()
		}
	}

	switch keyStr {
	case "esc":
		if m.discoveryPhase == PhaseClusterSelection {
			// Return to port forwards view
			m.uiState = StatePortForwards
			m.errorMsg = ""
			m.statusMsg = ""
			return m, nil
		} else if m.discoveryPhase == PhaseServiceSelection {
			// Return to cluster selection
			m.discoveryPhase = PhaseClusterSelection
			m.initializeClusterSelection()
			return m, nil
		}

	case "enter":
		if m.discoveryPhase == PhaseClusterSelection {
			// Select cluster and move to service discovery
			return m.handleClusterSelection()
		} else if m.discoveryPhase == PhaseServiceSelection {
			// Confirm service selection and add to config
			return m.handleServiceSelectionConfirm()
		}

	case " ", "space": // Space key for toggling service selection (handle both variants)
		if m.discoveryPhase == PhaseServiceSelection {
			return m.handleServiceToggle()
		}

	case "/": // Filter for services
		if m.discoveryPhase == PhaseServiceSelection {
			m.errorMsg = ""
			m.statusMsg = ""
			m.discoveryFilterMode = true
			m.discoveryFilterInput.Focus()
			m.discoveryTable.Blur()
			return m, nil
		}

	case "e": // Edit local port
		if m.discoveryPhase == PhaseServiceSelection {
			// Check if the selected port is an existing configuration
			selectedIdx := m.discoveryTable.Cursor()
			ports := m.discoveryPorts
			if m.discoveryFilterInput.Value() != "" {
				ports = m.applyDiscoveryPortFilter()
			}

			if selectedIdx < len(ports) {
				// Find the actual port in the full list
				var targetPort *PortSelection
				if m.discoveryFilterInput.Value() != "" {
					selectedPort := ports[selectedIdx]
					for i := range m.discoveryPorts {
						if m.discoveryPorts[i].GeneratedID == selectedPort.GeneratedID {
							targetPort = &m.discoveryPorts[i]
							break
						}
					}
				} else {
					targetPort = &m.discoveryPorts[selectedIdx]
				}

				// Prevent editing if this is an existing configuration
				if targetPort != nil && targetPort.ExistingConfigIndex != -1 {
					m.errorMsg = "Cannot edit local port: This service already exists in configuration. Edit it from the main view instead."
					return m, nil
				}
			}

			return m.handleDiscoveryEditStart()
		}

	case "up", "k", "down", "j":
		// Let the table handle navigation for both phases
		if m.discoveryPhase == PhaseClusterSelection {
			// Cluster selection navigation
			var cmd tea.Cmd
			m.discoveryTable, cmd = m.discoveryTable.Update(msg)
			return m, cmd
		} else if m.discoveryPhase == PhaseServiceSelection && !m.discoveryEditMode {
			// Service selection navigation (only if not in edit mode)
			var cmd tea.Cmd
			m.discoveryTable, cmd = m.discoveryTable.Update(msg)
			return m, cmd
		}

	default:
		// Let the table handle other keys for cluster selection or service selection (if not in edit mode)
		if m.discoveryPhase == PhaseClusterSelection {
			// Allow table to handle any other navigation keys during cluster selection
			var cmd tea.Cmd
			m.discoveryTable, cmd = m.discoveryTable.Update(msg)
			return m, cmd
		} else if m.discoveryPhase == PhaseServiceSelection && !m.discoveryEditMode {
			// Allow table to handle other keys during service selection (if not in edit mode)
			var cmd tea.Cmd
			m.discoveryTable, cmd = m.discoveryTable.Update(msg)
			return m, cmd
		}
	}

	return m, nil
}

// enterServiceDiscovery initializes the service discovery flow
func (m *Model) enterServiceDiscovery() (tea.Model, tea.Cmd) {
	m.uiState = StateServiceDiscovery
	m.discoveryPhase = PhaseClusterSelection
	m.errorMsg = ""
	m.statusMsg = ""

	// Initialize discovery filter input
	m.discoveryFilterInput = textinput.New()
	m.discoveryFilterInput.Placeholder = "Filter..."
	m.discoveryFilterInput.CharLimit = 156
	m.discoveryFilterInput.Width = m.width - 4
	if m.discoveryFilterInput.Width < 20 {
		m.discoveryFilterInput.Width = 20
	}

	// Initialize discovery edit input for local port editing
	m.discoveryEditInput = textinput.New()
	m.discoveryEditInput.Placeholder = "Port"
	m.discoveryEditInput.CharLimit = 5
	m.discoveryEditInput.Width = 8

	// Get available clusters and initialize cluster selection
	err := m.initializeClusterSelection()
	if err != nil {
		m.errorMsg = fmt.Sprintf("Failed to get clusters: %v", err)
		m.uiState = StatePortForwards
		return m, nil
	}

	return m, nil
}

// initializeClusterSelection gets available clusters and creates the selection table
func (m *Model) initializeClusterSelection() error {
	clusters, err := getAvailableClusters()
	if err != nil {
		return err
	}

	if len(clusters) == 0 {
		return fmt.Errorf("no Kubernetes contexts found")
	}

	m.discoveryClusters = clusters
	m.discoverySelectedCluster = 0

	// Find current context and select it by default
	currentContext, err := getCurrentContext()
	if err == nil {
		for i, cluster := range clusters {
			if cluster == currentContext {
				m.discoverySelectedCluster = i
				break
			}
		}
	}

	// Create table rows for clusters
	rows := make([]table.Row, len(clusters))
	for i, cluster := range clusters {
		status := ""
		if i == m.discoverySelectedCluster {
			status = "●"
		}
		rows[i] = table.Row{cluster, status}
	}

	// Create and configure the cluster selection table with dynamic columns
	columns := m.calculateClusterSelectionColumns()

	// Apply table styles
	s := table.DefaultStyles()
	s.Header = s.Header.
		BorderStyle(lipgloss.NormalBorder()).
		BorderForeground(lipgloss.Color(ColorBorder)).
		BorderBottom(true).
		Bold(false)
	s.Selected = s.Selected.
		Foreground(lipgloss.Color(ColorSelectedFg)).
		Background(lipgloss.Color(ColorSelectedBg)).
		Bold(false)

	m.discoveryTable = table.New(
		table.WithColumns(columns),
		table.WithRows(rows),
		table.WithFocused(true),
		table.WithHeight(min(len(rows)+2, m.height-6)),
		table.WithStyles(s),
	)

	return nil
}

// handleClusterSelection processes cluster selection and starts service discovery
func (m *Model) handleClusterSelection() (tea.Model, tea.Cmd) {
	selectedIdx := m.discoveryTable.Cursor()
	if selectedIdx >= len(m.discoveryClusters) {
		m.errorMsg = "Invalid cluster selection"
		return m, nil
	}

	selectedCluster := m.discoveryClusters[selectedIdx]
	m.discoverySelectedCluster = selectedIdx
	m.statusMsg = fmt.Sprintf("Discovering services in cluster '%s'...", selectedCluster)

	// Discover services in the selected cluster
	opts := discovery.Options{
		Context:         selectedCluster,
		NamespaceFilter: "*", // Discover all namespaces
		Verbose:         false,
	}

	result, err := discovery.DiscoverServices(opts)
	if err != nil {
		m.errorMsg = fmt.Sprintf("Service discovery failed: %v", err)
		return m, nil
	}

	if result.TotalCount == 0 {
		m.errorMsg = fmt.Sprintf("No services found in cluster '%s'", selectedCluster)
		return m, nil
	}

	// Get existing configs to check for pre-existing services
	existingConfigs := m.configStore.GetAll()
	existingServiceMap := make(map[string]bool)
	for _, cfg := range existingConfigs {
		if cfg.Context == selectedCluster {
			// Create a key for existing service identification
			key := fmt.Sprintf("%s/%s", cfg.Namespace, cfg.Service)
			existingServiceMap[key] = true
		}
	}

	// Store the existing service map for later use in confirmation
	m.discoveryExistingServices = existingServiceMap

	// Convert discovered services to individual port selections
	var portSelections []PortSelection
	for _, discoveredService := range result.Services {
		// Create a selection for each port on each service
		for _, port := range discoveredService.ServiceInfo.Ports {
			// Generate ID for this specific port
			generatedID := generateServicePortID(selectedCluster, discoveredService.ServiceInfo, port)

			// Default local port to remote port
			localPort := int(port.Port)

			// Check if this specific port already exists in config
			alreadyExists := false
			existingConfigIndex := -1
			for i, cfg := range existingConfigs {
				if cfg.Context == selectedCluster &&
					cfg.Namespace == discoveredService.ServiceInfo.Namespace &&
					cfg.Service == discoveredService.ServiceInfo.Name &&
					cfg.PortRemote == int(port.Port) {
					alreadyExists = true
					existingConfigIndex = i
					// Use the existing local port, not the remote port
					localPort = cfg.PortLocal
					break
				}
			}

			portSelection := PortSelection{
				ServiceName:      discoveredService.ServiceInfo.Name,
				ServiceNamespace: discoveredService.ServiceInfo.Namespace,
				ServiceType:      discoveredService.ServiceInfo.Type,
				ServiceLabels:    discoveredService.ServiceInfo.Labels,
				Port: ServicePortInfo{
					Name:       port.Name,
					Port:       port.Port,
					TargetPort: port.TargetPort,
					Protocol:   port.Protocol,
				},
				Selected:            alreadyExists, // Pre-select if already in config
				LocalPort:           localPort,
				GeneratedID:         generatedID,
				ExistingConfigIndex: existingConfigIndex, // Store config index or -1 if new
			}

			portSelections = append(portSelections, portSelection)
		}
	}

	// Store the port selections
	m.discoveryPorts = portSelections

	// Move to service selection phase
	m.discoveryPhase = PhaseServiceSelection
	m.statusMsg = fmt.Sprintf("Found %d ports in cluster '%s'", len(m.discoveryPorts), selectedCluster)
	m.refreshDiscoveryTable()

	return m, nil
}

// refreshDiscoveryTable updates the discovery table based on current phase
func (m *Model) refreshDiscoveryTable() {
	if m.discoveryPhase == PhaseServiceSelection {
		m.initializeServiceSelectionTable()
	}
}

// initializeServiceSelectionTable creates the port selection table (one row per port)
func (m *Model) initializeServiceSelectionTable() {
	// Apply filter if active
	ports := m.discoveryPorts
	if m.discoveryFilterInput.Value() != "" {
		ports = m.applyDiscoveryPortFilter()
	}

	// Create table rows for individual ports
	rows := make([]table.Row, len(ports))
	for i, port := range ports {
		checkbox := "□"
		if port.Selected {
			checkbox = "☑"
		}

		// Create service:port display name
		servicePortName := port.ServiceName
		if port.Port.Name != "" {
			servicePortName += ":" + port.Port.Name
		} else {
			servicePortName += fmt.Sprintf(":%d", port.Port.Port)
		}

		// Determine local port display - show edit input if this row is being edited
		localPortDisplay := fmt.Sprintf("%d", port.LocalPort)

		// Check if this row is being edited (need to find actual index in full list)
		if m.discoveryEditMode {
			// Find the port being edited in the filtered list
			var editingPortID string
			if m.discoveryEditIndex < len(m.discoveryPorts) {
				editingPortID = m.discoveryPorts[m.discoveryEditIndex].GeneratedID
			}

			// If this filtered row matches the port being edited, show input
			if port.GeneratedID == editingPortID {
				localPortDisplay = "[" + m.discoveryEditInput.View() + "]"
			}
		}

		rows[i] = table.Row{
			checkbox,
			servicePortName,
			port.ServiceNamespace,
			port.ServiceType,
			fmt.Sprintf("%d", port.Port.Port),
			localPortDisplay,
		}
	}

	// Create and configure the port selection table with dynamic columns
	columns := m.calculateDiscoveryServiceColumns()

	// Apply table styles (used only on first init)
	s := table.DefaultStyles()
	s.Header = s.Header.
		BorderStyle(lipgloss.NormalBorder()).
		BorderForeground(lipgloss.Color(ColorBorder)).
		BorderBottom(true).
		Bold(false)
	s.Selected = s.Selected.
		Foreground(lipgloss.Color(ColorSelectedFg)).
		Background(lipgloss.Color(ColorSelectedBg)).
		Bold(false)

	// Calculate proper table height accounting for all UI elements
	// Title (2 lines) + Filter (3 lines) + Instructions (2 lines) + Controls (2 lines) + margins
	availableHeight := m.height - 9 // More conservative height calculation
	if availableHeight < 4 {
		availableHeight = 4 // Minimum usable height
	}
	tableHeight := min(len(rows)+2, availableHeight)

	if m.discoveryTable.Rows() != nil {
		// Preserve cursor and viewport by updating in place
		currentCursor := m.discoveryTable.Cursor()
		m.discoveryTable.SetColumns(columns)
		m.discoveryTable.SetHeight(tableHeight)
		m.discoveryTable.SetRows(rows)
		// Restore cursor within bounds
		if currentCursor >= len(rows) {
			currentCursor = max(0, len(rows)-1)
		}
		m.discoveryTable.SetCursor(currentCursor)
	} else {
		// First-time initialization
		m.discoveryTable = table.New(
			table.WithColumns(columns),
			table.WithRows(rows),
			table.WithFocused(true),
			table.WithHeight(tableHeight),
			table.WithStyles(s),
		)
	}
}

// handleServiceToggle toggles port selection
func (m *Model) handleServiceToggle() (tea.Model, tea.Cmd) {
	selectedIdx := m.discoveryTable.Cursor()

	// Apply filter if active to get the correct port index
	if m.discoveryFilterInput.Value() != "" {
		filteredPorts := m.applyDiscoveryPortFilter()
		if selectedIdx >= len(filteredPorts) {
			m.errorMsg = "Invalid port selection"
			return m, nil
		}

		// Find the actual port in the full list
		selectedPort := filteredPorts[selectedIdx]
		for i := range m.discoveryPorts {
			if m.discoveryPorts[i].GeneratedID == selectedPort.GeneratedID {
				m.discoveryPorts[i].Selected = !m.discoveryPorts[i].Selected
				break
			}
		}
	} else {
		if selectedIdx >= len(m.discoveryPorts) {
			m.errorMsg = "Invalid port selection"
			return m, nil
		}
		m.discoveryPorts[selectedIdx].Selected = !m.discoveryPorts[selectedIdx].Selected
	}

	// Store current cursor position before refresh
	currentCursor := m.discoveryTable.Cursor()
	m.refreshDiscoveryTable()
	// Restore cursor position after refresh
	m.discoveryTable.SetCursor(currentCursor)
	return m, nil
}

// applyDiscoveryPortFilter filters ports based on the filter input
func (m *Model) applyDiscoveryPortFilter() []PortSelection {
	filterText := strings.ToLower(strings.TrimSpace(m.discoveryFilterInput.Value()))
	if filterText == "" {
		return m.discoveryPorts
	}

	var filtered []PortSelection
	for _, port := range m.discoveryPorts {
		// Search in service name, namespace, type, and port info
		if strings.Contains(strings.ToLower(port.ServiceName), filterText) ||
			strings.Contains(strings.ToLower(port.ServiceNamespace), filterText) ||
			strings.Contains(strings.ToLower(port.ServiceType), filterText) ||
			strings.Contains(strings.ToLower(port.Port.Name), filterText) ||
			strings.Contains(fmt.Sprintf("%d", port.Port.Port), filterText) {
			filtered = append(filtered, port)
		}
	}

	return filtered
}

// handleServiceSelectionConfirm processes the final port selection with add/update/remove support
func (m *Model) handleServiceSelectionConfirm() (tea.Model, tea.Cmd) {
	clusterName := m.discoveryClusters[m.discoverySelectedCluster]

	addedCount := 0
	updatedCount := 0
	removedCount := 0

	// Process each port selection
	for _, portSelection := range m.discoveryPorts {
		if portSelection.ExistingConfigIndex != -1 {
			// This port existed in config - handle selection/deselection only, never update local port
			if portSelection.Selected {
				// Port is selected but already exists - no action needed
				// Existing configurations should never be modified during service discovery
				logging.LogDebug("Port %s already exists in config, no changes needed", portSelection.GeneratedID)
				// Note: We intentionally don't increment any counters here since no actual change is made
			} else {
				// Port is deselected - remove from config
				existingCfg, exists := m.configStore.Get(portSelection.ExistingConfigIndex)
				if exists {
					if sqliteStore, ok := m.configStore.(*config.SQLiteConfigStore); ok {
						err := sqliteStore.DeletePortForward(existingCfg.ID)
						if err != nil {
							m.errorMsg = fmt.Sprintf("Failed to remove port: %v", err)
							continue
						}
						removedCount++
						logging.LogDebug("Removed port %s from config", portSelection.GeneratedID)
					}
				}
			}
		} else {
			// This is a new port - add if selected
			if portSelection.Selected {
				// Create port forward config for this new port
				cfg := config.PortForwardConfig{
					ID:         portSelection.GeneratedID,
					Context:    clusterName,
					Namespace:  portSelection.ServiceNamespace,
					Service:    portSelection.ServiceName,
					PortRemote: int(portSelection.Port.Port),
					PortLocal:  portSelection.LocalPort,
				}

				err := m.configStore.Add(cfg)
				if err != nil {
					m.errorMsg = fmt.Sprintf("Failed to add port: %v", err)
					continue
				}
				addedCount++
				logging.LogDebug("Added new port %s to config", portSelection.GeneratedID)
			}
			// If not selected, no action needed for new ports
		}
	}

	// Generate status message based on changes
	var statusParts []string
	if addedCount > 0 {
		statusParts = append(statusParts, fmt.Sprintf("%d added", addedCount))
	}
	if updatedCount > 0 {
		statusParts = append(statusParts, fmt.Sprintf("%d updated", updatedCount))
	}
	if removedCount > 0 {
		statusParts = append(statusParts, fmt.Sprintf("%d removed", removedCount))
	}

	if len(statusParts) > 0 {
		m.statusMsg = fmt.Sprintf("Port forwards: %s", strings.Join(statusParts, ", "))
		// Save config
		err := m.configStore.Save()
		if err != nil {
			m.errorMsg = fmt.Sprintf("Failed to save config: %v", err)
		}
	} else {
		m.statusMsg = "No changes made"
	}

	// Return to main view and refresh
	m.uiState = StatePortForwards
	m.refreshTable()
	return m, nil
}

// Helper functions

// generateServicePortID creates a unique ID for a service port
func generateServicePortID(context string, service discovery.ServiceInfo, port discovery.ServicePort) string {
	// Generate ID similar to the discovery package but for specific ports
	contextPart := sanitizeIDPart(context)
	serviceType := detectServiceTypeFromInfo(service)
	discriminator := sanitizeIDPart(service.Name)

	// Include port in the discriminator
	discriminator += fmt.Sprintf("-%d", port.Port)
	if port.Name != "" && port.Name != "http" && port.Name != "tcp" {
		discriminator += "-" + sanitizeIDPart(port.Name)
	}

	return contextPart + "." + serviceType + "." + discriminator
}

// detectServiceTypeFromInfo attempts to identify the type of service
func detectServiceTypeFromInfo(service discovery.ServiceInfo) string {
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

	nameLower := strings.ToLower(serviceName)
	for _, serviceType := range commonTypes {
		if strings.Contains(nameLower, serviceType) {
			return serviceType
		}
	}

	// Fallback to service type from Kubernetes
	if service.Type != "" {
		return sanitizeIDPart(service.Type)
	}

	// Last resort: use "service" as default
	return "service"
}

// sanitizeIDPart cleans a string for use in IDs
func sanitizeIDPart(input string) string {
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

// handleDiscoveryEditStart enters edit mode for the local port of the currently selected row
// NOTE: This function should only be called after checking that the port is not an existing configuration
func (m *Model) handleDiscoveryEditStart() (tea.Model, tea.Cmd) {
	selectedIdx := m.discoveryTable.Cursor()

	// Get the port list accounting for active filter
	ports := m.discoveryPorts
	if m.discoveryFilterInput.Value() != "" {
		ports = m.applyDiscoveryPortFilter()
	}

	if selectedIdx >= len(ports) {
		m.errorMsg = "Invalid port selection"
		return m, nil
	}

	// Find the actual port index in the full list if filtering is active
	var actualPortIndex int
	if m.discoveryFilterInput.Value() != "" {
		selectedPort := ports[selectedIdx]
		actualPortIndex = -1
		for i, port := range m.discoveryPorts {
			if port.GeneratedID == selectedPort.GeneratedID {
				actualPortIndex = i
				break
			}
		}
		if actualPortIndex == -1 {
			m.errorMsg = "Could not find port in full list"
			return m, nil
		}
	} else {
		actualPortIndex = selectedIdx
	}

	// Double-check that this is not an existing configuration (should have been checked by caller)
	if m.discoveryPorts[actualPortIndex].ExistingConfigIndex != -1 {
		m.errorMsg = "Cannot edit existing configuration during service discovery"
		return m, nil
	}

	// Enter edit mode
	m.discoveryEditMode = true
	m.discoveryEditIndex = actualPortIndex

	// Set the current local port value in the input
	currentLocalPort := m.discoveryPorts[actualPortIndex].LocalPort
	m.discoveryEditInput.SetValue(fmt.Sprintf("%d", currentLocalPort))
	m.discoveryEditInput.Focus()
	m.discoveryTable.Blur()

	// Clear any previous errors
	m.errorMsg = ""

	return m, textinput.Blink
}

// handleDiscoveryEditMode handles input while in edit mode
func (m *Model) handleDiscoveryEditMode(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	keyStr := msg.String()

	switch keyStr {
	case "esc":
		// Cancel edit mode
		m.discoveryEditMode = false
		m.discoveryEditInput.Blur()
		m.discoveryTable.Focus()
		m.errorMsg = ""
		return m, nil

	case "enter":
		// Confirm edit
		return m.handleDiscoveryEditConfirm()

	default:
		// Update the edit input and refresh table to show live updates
		var cmd tea.Cmd
		m.discoveryEditInput, cmd = m.discoveryEditInput.Update(msg)
		// Refresh table to show live edit updates
		currentCursor := m.discoveryTable.Cursor()
		m.refreshDiscoveryTable()
		m.discoveryTable.SetCursor(currentCursor)
		return m, cmd
	}
}

// handleDiscoveryEditConfirm validates and applies the local port edit
func (m *Model) handleDiscoveryEditConfirm() (tea.Model, tea.Cmd) {
	// Validate the input
	inputText := strings.TrimSpace(m.discoveryEditInput.Value())
	if inputText == "" {
		m.errorMsg = "Local port cannot be empty"
		return m, nil
	}

	// Parse the port number
	var newLocalPort int
	_, err := fmt.Sscanf(inputText, "%d", &newLocalPort)
	if err != nil {
		m.errorMsg = "Invalid port number"
		return m, nil
	}

	// Validate port range
	if newLocalPort < 1 || newLocalPort > 65535 {
		m.errorMsg = "Port must be between 1 and 65535"
		return m, nil
	}

	// Update the local port
	m.discoveryPorts[m.discoveryEditIndex].LocalPort = newLocalPort

	// Exit edit mode
	m.discoveryEditMode = false
	m.discoveryEditInput.Blur()
	m.discoveryTable.Focus()
	m.errorMsg = ""

	// Refresh table to show the updated port
	currentCursor := m.discoveryTable.Cursor()
	m.refreshDiscoveryTable()
	m.discoveryTable.SetCursor(currentCursor)

	return m, nil
}
