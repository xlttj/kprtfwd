package ui

import (
	"fmt"
	"os/exec"
	"runtime"
	"strings"

	"github.com/xlttj/kprtfwd/pkg/config"
	"github.com/xlttj/kprtfwd/pkg/k8s"
	"github.com/xlttj/kprtfwd/pkg/logging"

	"github.com/charmbracelet/bubbles/table"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// // Render method for minimalDelegate - MOVED to types.go
// func (d minimalDelegate) Render(w io.Writer, m list.Model, index int, listItem list.Item) {
// 	// ... implementation ...
// }

// Model represents the state of the UI
type Model struct {
	uiState UIState

	// Core components
	configStore   config.ConfigStoreInterface
	portForwarder *k8s.PortForwarder
	width         int
	height        int

	// Central error message
	errorMsg string
	// Status/info message (non-error feedback)
	statusMsg string

	// Port forwards table
	portForwardsTable table.Model

	// Grouping state
	groupStates     map[string]*GroupState // Map of group name to state
	tableRows       []TableRow             // Enhanced rows with metadata
	groupingEnabled bool                   // Whether grouping is enabled

	// Filter state
	filterMode      bool                       // Whether filtering is active
	filterInput     textinput.Model            // The search input component
	filteredConfigs []config.PortForwardConfig // Cached filtered results

	// Inline editing state for local ports in main view
	editMode        bool            // Whether we're in inline edit mode
	editConfigIndex int             // Config index being edited
	editInput       textinput.Model // Text input for editing local port

	// Project management state
	projectSelector        table.Model     // Project selection table
	projectManagementTable table.Model     // Project management table
	projectNameInput       textinput.Model // Input for new project name
	projectServiceTable    table.Model     // Service selection for project editing
	currentProject         *config.Project // Project being edited

	// Service discovery state
	discoveryPhase            DiscoveryPhase
	discoveryClusters         []string
	discoverySelectedCluster  int
	discoveryPorts            []PortSelection // Changed from services to individual ports
	discoveryTable            table.Model
	discoveryFilterInput      textinput.Model
	discoveryFilterMode       bool
	discoveryExistingServices map[string]bool

	// Inline editing state for local ports in discovery
	discoveryEditMode  bool            // Whether we're in inline edit mode
	discoveryEditIndex int             // Index of the port being edited
	discoveryEditInput textinput.Model // Text input for editing local port
}

// calculateProjectSelectorColumns returns columns for project selector with dynamic widths
func (m *Model) calculateProjectSelectorColumns() []table.Column {
	// Calculate available width (subtract padding for borders)
	availableWidth := m.width - 8
	availableWidth = max(availableWidth, 45) // Minimum total width

	// Distribute widths: PROJECT gets most space, others fixed minimum
	minForwards := 8 // "FORWARDS"
	minActive := 6   // "ACTIVE"
	projectWidth := availableWidth - minForwards - minActive
	projectWidth = max(projectWidth, 15) // Minimum for PROJECT

	return []table.Column{
		{Title: "PROJECT", Width: projectWidth},
		{Title: "FORWARDS", Width: minForwards},
		{Title: "ACTIVE", Width: minActive},
	}
}

// calculateProjectManagementColumns returns columns for project management with dynamic widths
func (m *Model) calculateProjectManagementColumns() []table.Column {
	// Calculate available width (subtract padding for borders)
	availableWidth := m.width - 8
	availableWidth = max(availableWidth, 50) // Minimum total width

	// Distribute widths: PROJECT and ACTIONS get more space
	minForwards := 8 // "FORWARDS"
	remainingWidth := availableWidth - minForwards
	projectWidth := remainingWidth * 40 / 100     // 40% for PROJECT
	actionsWidth := remainingWidth - projectWidth // Rest for ACTIONS

	// Ensure minimums
	projectWidth = max(projectWidth, 15)
	actionsWidth = max(actionsWidth, 15)

	return []table.Column{
		{Title: "PROJECT", Width: projectWidth},
		{Title: "FORWARDS", Width: minForwards},
		{Title: "ACTIONS", Width: actionsWidth},
	}
}

// calculateServiceSelectionColumns returns columns for service selection with dynamic widths
func (m *Model) calculateServiceSelectionColumns() []table.Column {
	// Calculate available width (subtract padding for borders)
	availableWidth := m.width - 8
	availableWidth = max(availableWidth, 60) // Minimum total width

	// Fixed minimums for some columns
	minInProject := 10 // "IN PROJECT"
	minPorts := 12     // "PORTS" (for "1234→5678")

	// Remaining width distributed among SERVICE, NAMESPACE, CONTEXT
	remainingWidth := availableWidth - minInProject - minPorts
	serviceWidth := remainingWidth * 40 / 100
	namespaceWidth := remainingWidth * 30 / 100
	contextWidth := remainingWidth - serviceWidth - namespaceWidth

	// Ensure minimums
	serviceWidth = max(serviceWidth, 12)
	namespaceWidth = max(namespaceWidth, 10)
	contextWidth = max(contextWidth, 10)

	return []table.Column{
		{Title: "IN PROJECT", Width: minInProject},
		{Title: "SERVICE", Width: serviceWidth},
		{Title: "NAMESPACE", Width: namespaceWidth},
		{Title: "CONTEXT", Width: contextWidth},
		{Title: "PORTS", Width: minPorts},
	}
}

// calculateClusterSelectionColumns returns columns for cluster selection with dynamic widths
func (m *Model) calculateClusterSelectionColumns() []table.Column {
	// Calculate available width (subtract padding for borders)
	availableWidth := m.width - 8
	availableWidth = max(availableWidth, 30) // Minimum total width

	// CURRENT column gets fixed small width, CLUSTER gets the rest
	minCurrent := 8 // "CURRENT"
	clusterWidth := availableWidth - minCurrent
	clusterWidth = max(clusterWidth, 15)

	return []table.Column{
		{Title: "CLUSTER", Width: clusterWidth},
		{Title: "CURRENT", Width: minCurrent},
	}
}

// calculateDiscoveryServiceColumns returns columns for service discovery with dynamic widths
func (m *Model) calculateDiscoveryServiceColumns() []table.Column {
	// Calculate available width (standardized padding for borders)
	availableWidth := m.width - 8
	availableWidth = max(availableWidth, 50) // Minimum total width

	// Fixed minimums for some columns
	minSel := 4    // "SEL"
	minRemote := 6 // "REMOTE"
	minLocal := 8  // "LOCAL" (increased to avoid truncation)
	minType := 8   // "TYPE"

	// Calculate total fixed width
	fixedWidth := minSel + minRemote + minLocal + minType

	// Remaining width distributed between SERVICE:PORT and NAMESPACE
	remainingWidth := availableWidth - fixedWidth
	if remainingWidth < 0 {
		remainingWidth = 0
	}

	serviceWidth := remainingWidth * 60 / 100 // SERVICE:PORT gets more space
	namespaceWidth := remainingWidth - serviceWidth

	// Ensure minimums
	serviceWidth = max(serviceWidth, 15)
	namespaceWidth = max(namespaceWidth, 8) // Reduced minimum for namespace

	return []table.Column{
		{Title: "SEL", Width: minSel},
		{Title: "SERVICE:PORT", Width: serviceWidth},
		{Title: "NAMESPACE", Width: namespaceWidth},
		{Title: "TYPE", Width: minType},
		{Title: "REMOTE", Width: minRemote},
		{Title: "LOCAL", Width: minLocal},
	}
}

// calculateColumnWidths returns column widths based on terminal width
func (m *Model) calculateColumnWidths() []table.Column {
	// Minimum widths for each column
	minWidths := map[string]int{
		ColContext:    8, // "CONTEXT"
		ColNamespace:  9, // "NAMESPACE"
		ColService:    7, // "SERVICE"
		ColPortRemote: 6, // "REMOTE"
		ColPortLocal:  5, // "LOCAL"
		ColStatus:     7, // "STATUS"
	}

	// Calculate available width (standardized padding for borders)
	availableWidth := m.width - 8
	availableWidth = max(availableWidth, 60) // Minimum total width

	// Calculate total minimum width needed
	totalMinWidth := 0
	for _, width := range minWidths {
		totalMinWidth += width
	}

	// If we have extra space, distribute it proportionally
	extraSpace := availableWidth - totalMinWidth
	extraSpace = max(extraSpace, 0)

	// Priority order for expanding columns (most important first)
	expandPriority := []string{ColService, ColNamespace, ColContext, ColStatus, ColPortRemote, ColPortLocal}

	// Calculate final widths
	finalWidths := make(map[string]int)
	for col, minWidth := range minWidths {
		finalWidths[col] = minWidth
	}

	// Distribute extra space more efficiently
	remainingSpace := extraSpace
	for _, col := range expandPriority {
		if remainingSpace <= 0 {
			break
		}

		// Give more space to service and namespace columns
		var extraForCol int
		switch col {
		case ColService:
			extraForCol = remainingSpace * 35 / 100 // 35% of remaining space
		case ColNamespace:
			extraForCol = remainingSpace * 30 / 100 // 30% of remaining space
		case ColContext:
			extraForCol = remainingSpace * 20 / 100 // 20% of remaining space
		case ColStatus:
			extraForCol = remainingSpace * 10 / 100 // 10% of remaining space
		default:
			extraForCol = remainingSpace * 2 / 100 // 2.5% of remaining space each for ports
		}

		if extraForCol > remainingSpace {
			extraForCol = remainingSpace
		}

		finalWidths[col] += extraForCol
		remainingSpace -= extraForCol
	}

	// Distribute any remaining space evenly across all columns
	if remainingSpace > 0 {
		extraPerCol := remainingSpace / len(minWidths)
		for col := range minWidths {
			finalWidths[col] += extraPerCol
		}
	}

	// Return columns with calculated widths (without ID column)
	return []table.Column{
		{Title: ColContext, Width: finalWidths[ColContext]},
		{Title: ColNamespace, Width: finalWidths[ColNamespace]},
		{Title: ColService, Width: finalWidths[ColService]},
		{Title: ColPortRemote, Width: finalWidths[ColPortRemote]},
		{Title: ColPortLocal, Width: finalWidths[ColPortLocal]},
		{Title: ColStatus, Width: finalWidths[ColStatus]},
	}
}

func NewModel() *Model {
	// Load config using the new store interface
	cfgStore, err := config.NewSQLiteConfigStore()
	// Initialize error string - will be overwritten if loading fails
	initialError := ""
	if err != nil {
		initialError = fmt.Sprintf("Config load error: %v", err)
		// Ensure cfgStore is usable even if loading failed
		if cfgStore == nil {
			// Handle possibility that NewConfigStore returns nil on error
			// If so, create an empty one here. Assume non-nil for now.
		}
	}
	if cfgStore == nil { // Defensive check if NewSQLiteConfigStore could return nil
		// Create a new instance if needed - though NewSQLiteConfigStore shouldn't return nil
		// This is a fallback to prevent panic
		logging.LogError("Critical error: Config store failed to initialize")
		initialError = "Critical error: Config store failed to initialize."
		return nil // Can't proceed without a config store
	}

	// Get initial configs slice
	initialCfgs := cfgStore.GetAll()

	// --- Initialize PortForwarder and Sync ---
	pf := k8s.NewPortForwarder()
	// Call sync with the initially loaded configs
	startFailures := pf.SyncPortForwards(initialCfgs)

	// *** Handle Start Failures ***
	// Since SyncPortForwards is now a no-op, startFailures should be empty,
	// but we'll keep this logic for compatibility
	if len(startFailures) > 0 {
		logging.LogError("SyncPortForwards reported %d start failures during initialization", len(startFailures))
		for index, startErr := range startFailures {
			// Retrieve the config that failed
			failedCfg, exists := cfgStore.Get(index)
			if !exists {
				// This shouldn't happen if index came from the initial list
				logging.LogError("Internal inconsistency: Config index %d from startFailures not found in store", index)
				continue
			}
			// Note: Since status is no longer in config, we just log the failure
			logging.LogDebug("Config %d (%s/%s) sync start failure: %v", index, failedCfg.Namespace, failedCfg.Service, startErr)
		}
	}

	// Get the potentially updated configs *after* handling failures
	finalInitialCfgs := cfgStore.GetAll()

	// *** Log configs AFTER sync and failure handling ***
	logging.LogDebug("NewModel: Configs loaded before UI init:")
	for i, cfg := range finalInitialCfgs {
		logging.LogDebug("  Index %d: %s/%s", i, cfg.Namespace, cfg.Service)
	}

	// --- Initialize Tables --- (Define styles first)
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

	// --- Create Model --- (Initialize with all components)
	ti := textinput.New()
	ti.Placeholder = "Filter..."
	ti.CharLimit = 156
	// Initialize to match resize behavior: width - 4 with a floor of 20 (default width is 80)
	ti.Width = max(20, 80-4)

	// Initialize edit input for local port editing
	ei := textinput.New()
	ei.Placeholder = "Port"
	ei.CharLimit = 5
	ei.Width = 8

	// Initialize project name input
	pni := textinput.New()
	pni.Placeholder = "Project name..."
	pni.CharLimit = 50
	pni.Width = 30

	m := &Model{
		uiState:          StatePortForwards,
		configStore:      cfgStore,
		portForwarder:    pf,
		errorMsg:         initialError,
		width:            80, // Default width, will be updated on first WindowSizeMsg
		height:           24, // Default height, will be updated on first WindowSizeMsg
		groupStates:      make(map[string]*GroupState),
		groupingEnabled:  true, // Enable grouping by default
		filterInput:      ti,
		editInput:        ei,
		projectNameInput: pni,
	}

	// Initialize Port Forwards Table with dynamic columns
	pfCols := m.calculateColumnWidths()
	pfTable := table.New(
		table.WithColumns(pfCols),
		table.WithRows(m.generateGroupedRows(finalInitialCfgs)), // Use grouped rows
		table.WithFocused(true),
		table.WithHeight(10),
		table.WithStyles(s),
	)
	m.portForwardsTable = pfTable

	return m
}

func (m *Model) Cleanup() {
	if m.portForwarder != nil {
		m.portForwarder.CleanupAll()
	}
}

func (m *Model) Init() tea.Cmd {
	return nil
}

func (m *Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width = msg.Width
		m.height = msg.Height
		// Update table dimensions based on new window size
		pfTableHeight := m.height - PortForwardsViewOffset
		if pfTableHeight < MinTableHeight {
			pfTableHeight = MinTableHeight
		}
		m.portForwardsTable.SetHeight(pfTableHeight)

		// Recalculate and update column widths based on new terminal width
		newCols := m.calculateColumnWidths()
		m.portForwardsTable.SetColumns(newCols)

		// Update other table column widths as well
		if m.projectSelector.Rows() != nil {
			m.projectSelector.SetColumns(m.calculateProjectSelectorColumns())
			// Update height too
			m.projectSelector.SetHeight(min(len(m.projectSelector.Rows())+2, m.height-6))
		}
		if m.projectManagementTable.Rows() != nil {
			m.projectManagementTable.SetColumns(m.calculateProjectManagementColumns())
			// Update height too
			m.projectManagementTable.SetHeight(min(len(m.projectManagementTable.Rows())+2, m.height-8))
		}
		if m.projectServiceTable.Rows() != nil {
			m.projectServiceTable.SetColumns(m.calculateServiceSelectionColumns())
			// Update height too
			m.projectServiceTable.SetHeight(min(len(m.projectServiceTable.Rows())+2, m.height-10))
		}
		if m.discoveryTable.Rows() != nil {
			if m.discoveryPhase == PhaseClusterSelection {
				m.discoveryTable.SetColumns(m.calculateClusterSelectionColumns())
				// Update height for cluster selection
				m.discoveryTable.SetHeight(min(len(m.discoveryTable.Rows())+2, m.height-6))
			} else if m.discoveryPhase == PhaseServiceSelection {
				m.discoveryTable.SetColumns(m.calculateDiscoveryServiceColumns())
				// Update height for service selection with proper calculation
				availableHeight := m.height - 9
				if availableHeight < 4 {
					availableHeight = 4
				}
				m.discoveryTable.SetHeight(min(len(m.discoveryTable.Rows())+2, availableHeight))
			}
		}

		// Update filter input widths to match terminal width (with some padding)
		filterWidth := m.width - 4 // Leave some padding
		if filterWidth < 20 {
			filterWidth = 20
		}
		m.filterInput.Width = filterWidth
		m.discoveryFilterInput.Width = filterWidth

		return m, nil

	case tea.KeyMsg:
		keyStr := msg.String()

		// Global shortcuts that work in any state
		switch keyStr {
		case "ctrl+c", ShortcutExit: // ctrl+x
			return m, tea.Quit
		}

		// Delegate to state-specific handlers
		switch m.uiState {
		case StatePortForwards:
			return m.updatePortForwards(msg)
		case StateProjectSelector:
			return m.updateProjectSelector(msg)
		case StateServiceDiscovery:
			return m.updateServiceDiscovery(msg)
		case StateProjectManagement:
			return m.updateProjectManagement(msg)
		case StateProjectCreation:
			return m.updateProjectCreation(msg)
		case StateProjectServiceSelection:
			return m.updateProjectServiceSelection(msg)
		}

	// Handle messages specific to certain operations/states
	case error: // General error handling
		// Log or display the error appropriately
		// For now, maybe just set a generic error message if no specific handler caught it
		// m.someGenericErrorField = msg.Error()
		return m, nil
	}

	return m, nil
}

// applyFilter filters configs based on the current filter text
func (m *Model) applyFilter() {
	filterText := strings.ToLower(strings.TrimSpace(m.filterInput.Value()))
	// Use base configs that respect active project filtering
	baseConfigs := m.configStore.GetActiveProjectForwards()

	if filterText == "" {
		// No filter, show base configs (which respect active project)
		m.filteredConfigs = baseConfigs
		return
	}

	// Filter configs by searching across visible fields (excluding ID)
	m.filteredConfigs = []config.PortForwardConfig{}
	for _, cfg := range baseConfigs {
		// Convert search fields to lowercase for case-insensitive matching
		context := strings.ToLower(cfg.Context)
		namespace := strings.ToLower(cfg.Namespace)
		service := strings.ToLower(cfg.Service)
		portRemote := fmt.Sprintf("%d", cfg.PortRemote)
		portLocal := fmt.Sprintf("%d", cfg.PortLocal)

		// Check if filter text is found in any of the visible fields
		if strings.Contains(context, filterText) ||
			strings.Contains(namespace, filterText) ||
			strings.Contains(service, filterText) ||
			strings.Contains(portRemote, filterText) ||
			strings.Contains(portLocal, filterText) {
			m.filteredConfigs = append(m.filteredConfigs, cfg)
		}
	}
}

// handlePortForwardsRestart processes Ctrl+R restart request
func (m *Model) handlePortForwardsRestart() (tea.Model, tea.Cmd) {
	// Clear previous messages
	m.errorMsg = ""
	m.statusMsg = ""

	// Get current configurations
	configs := m.configStore.GetAll()

	// Restart all running port forwards
	result := m.portForwarder.RestartRunningForwards(configs)

	// Update UI state to reflect any changes
	m.refreshTable()

	// Show restart summary
	if len(result.Errors) > 0 {
		m.errorMsg = m.formatRestartSummary(result)
	} else {
		m.statusMsg = m.formatRestartSummary(result)
	}

	return m, nil
}

// formatRestartSummary creates user-friendly restart summary
func (m *Model) formatRestartSummary(result *k8s.RestartResult) string {
	if len(result.Errors) > 0 {
		// Show errors first
		errorMsgs := []string{}
		for idx, err := range result.Errors {
			if cfg, exists := m.configStore.Get(idx); exists {
				errorMsgs = append(errorMsgs, fmt.Sprintf("%s: %v", cfg.Service, err))
			} else {
				errorMsgs = append(errorMsgs, fmt.Sprintf("Index %d: %v", idx, err))
			}
		}
		return fmt.Sprintf("Restart errors: %s", strings.Join(errorMsgs, "; "))
	}

	// Show successful restart summary
	if result.RestartedCount == 0 {
		return "No running port forwards to restart"
	}

	return fmt.Sprintf("Restarted %d port forward(s)", result.RestartedCount)
}

// openInBrowser opens the HTTP URL for the given port forward configuration
func (m *Model) openInBrowser(cfg config.PortForwardConfig) error {
	url := fmt.Sprintf("http://localhost:%d", cfg.PortLocal)
	logging.LogDebug("Opening URL in browser: %s", url)

	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "darwin":
		cmd = exec.Command("open", url)
	case "linux":
		cmd = exec.Command("xdg-open", url)
	case "windows":
		cmd = exec.Command("rundll32", "url.dll,FileProtocolHandler", url)
	default:
		return fmt.Errorf("unsupported operating system: %s", runtime.GOOS)
	}

	return cmd.Run()
}
