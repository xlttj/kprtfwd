package ui

import (
	"fmt"

	"github.com/xlttj/kprtfwd/pkg/discovery"

	"github.com/charmbracelet/bubbles/table"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// This file keeps all the kubectl-backed work in service discovery OFF the
// BubbleTea event loop. Previously the cluster list and `kubectl get services`
// calls ran synchronously inside Update(), which froze the whole UI (no render,
// no Esc, no quit) for up to 60 seconds. Now those calls run inside tea.Cmds and
// deliver their results as messages that are handled below.

// clustersLoadedMsg is delivered when the async kubectl context lookup finishes.
type clustersLoadedMsg struct {
	clusters []string
	current  string
	err      error
}

// servicesDiscoveredMsg is delivered when async service discovery for a cluster finishes.
type servicesDiscoveredMsg struct {
	cluster string
	result  *discovery.DiscoveryResult
	err     error
}

// loadClustersCmd fetches the available kubectl contexts without blocking the UI.
func loadClustersCmd() tea.Cmd {
	return func() tea.Msg {
		clusters, err := getAvailableClusters()
		if err != nil {
			return clustersLoadedMsg{err: err}
		}
		// Current context is best-effort; failing to read it is non-fatal.
		current, _ := discovery.CurrentContext()
		return clustersLoadedMsg{clusters: clusters, current: current}
	}
}

// discoverServicesCmd runs service discovery for a cluster without blocking the UI.
func discoverServicesCmd(cluster string) tea.Cmd {
	return func() tea.Msg {
		opts := discovery.Options{
			Context:         cluster,
			NamespaceFilter: "*", // Discover all namespaces
			Verbose:         false,
		}
		result, err := discovery.DiscoverServices(opts)
		return servicesDiscoveredMsg{cluster: cluster, result: result, err: err}
	}
}

// handleClustersLoaded builds the cluster-selection table from async results.
func (m *Model) handleClustersLoaded(msg clustersLoadedMsg) (tea.Model, tea.Cmd) {
	m.discoveryLoading = false

	// The user may have pressed Esc while loading; don't yank them back.
	if m.uiState != StateServiceDiscovery {
		return m, nil
	}

	if msg.err != nil {
		m.errorMsg = fmt.Sprintf("Failed to get clusters: %v", msg.err)
		m.statusMsg = ""
		m.uiState = StatePortForwards
		return m, nil
	}
	if len(msg.clusters) == 0 {
		m.errorMsg = "No Kubernetes contexts found"
		m.statusMsg = ""
		m.uiState = StatePortForwards
		return m, nil
	}

	m.statusMsg = ""
	m.buildClusterTable(msg.clusters, msg.current)
	return m, nil
}

// handleServicesDiscovered converts async discovery results into port selections.
// The conversion logic (pre-existing service detection, local-port defaulting) is
// pure given the configStore, which makes it unit-testable without kubectl.
func (m *Model) handleServicesDiscovered(msg servicesDiscoveredMsg) (tea.Model, tea.Cmd) {
	m.discoveryLoading = false

	// Ignore late results if the user navigated away while we were discovering.
	if m.uiState != StateServiceDiscovery {
		return m, nil
	}

	if msg.err != nil {
		m.errorMsg = fmt.Sprintf("Service discovery failed: %v", msg.err)
		m.statusMsg = ""
		return m, nil
	}

	selectedCluster := msg.cluster
	result := msg.result
	if result == nil || result.TotalCount == 0 {
		m.errorMsg = fmt.Sprintf("No services found in cluster '%s'", selectedCluster)
		m.statusMsg = ""
		return m, nil
	}

	// Get existing configs to check for pre-existing services
	existingConfigs := m.configStore.GetAll()
	existingServiceMap := make(map[string]bool)
	for _, cfg := range existingConfigs {
		if cfg.Context == selectedCluster {
			key := fmt.Sprintf("%s/%s", cfg.Namespace, cfg.Service)
			existingServiceMap[key] = true
		}
	}
	m.discoveryExistingServices = existingServiceMap

	// Convert discovered services to individual port selections
	var portSelections []PortSelection
	for _, discoveredService := range result.Services {
		for _, port := range discoveredService.ServiceInfo.Ports {
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

			portSelections = append(portSelections, PortSelection{
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
				ExistingConfigIndex: existingConfigIndex, // Config index or -1 if new
			})
		}
	}

	m.discoveryPorts = portSelections

	// Move to service selection phase
	m.discoveryPhase = PhaseServiceSelection
	m.statusMsg = fmt.Sprintf("Found %d ports in cluster '%s'", len(m.discoveryPorts), selectedCluster)
	m.refreshDiscoveryTable()

	return m, nil
}

// buildClusterTable constructs the cluster-selection table from already-fetched
// data. It performs no network I/O, so it is safe to call from the event loop
// (e.g. when navigating back from service selection).
func (m *Model) buildClusterTable(clusters []string, current string) {
	m.discoveryClusters = clusters
	m.discoverySelectedCluster = 0
	for i, cluster := range clusters {
		if cluster == current {
			m.discoverySelectedCluster = i
			break
		}
	}

	rows := make([]table.Row, len(clusters))
	for i, cluster := range clusters {
		status := IndicatorUnselected
		if i == m.discoverySelectedCluster {
			status = IndicatorSelected
		}
		rows[i] = table.Row{cluster, status}
	}

	columns := m.calculateClusterSelectionColumns()

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
		table.WithKeyMap(navTableKeyMap()),
		table.WithStyles(s),
	)
}
