package ui

import (
	"fmt"
	"os/exec"
	"runtime"
	"strings"

	"kprtfwd/pkg/config"
	"kprtfwd/pkg/k8s"
	"kprtfwd/pkg/logging"

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
	configStore   *config.ConfigStore
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
	groupStates map[string]*GroupState // Map of group name to state
	tableRows   []TableRow             // Enhanced rows with metadata
	groupingEnabled bool               // Whether grouping is enabled

	// Filter state
	filterMode     bool                       // Whether filtering is active
	filterInput    textinput.Model            // The search input component
	filteredConfigs []config.PortForwardConfig // Cached filtered results

	// Project management state
	projectSelector   table.Model      // Project selection table
}

// calculateColumnWidths returns column widths based on terminal width
func (m *Model) calculateColumnWidths() []table.Column {
	// Minimum widths for each column
	minWidths := map[string]int{
		ColContext:    8,  // "CONTEXT"
		ColNamespace:  9,  // "NAMESPACE"
		ColService:    7,  // "SERVICE"
		ColPortRemote: 6,  // "REMOTE"
		ColPortLocal:  5,  // "LOCAL"
		ColStatus:     7,  // "STATUS"
	}

	// Calculate available width (subtract some padding for borders and spacing)
	availableWidth := m.width - 10
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

	// Distribute extra space
	remainingSpace := extraSpace
	for _, col := range expandPriority {
		if remainingSpace <= 0 {
			break
		}

		// Give more space to service and namespace columns
		var extraForCol int
		switch col {
		case ColService:
			extraForCol = remainingSpace * 40 / 100 // 40% of remaining space
		case ColNamespace:
			extraForCol = remainingSpace * 25 / 100 // 25% of remaining space
		case ColContext:
			extraForCol = remainingSpace * 20 / 100 // 20% of remaining space
		default:
			extraForCol = remainingSpace * 5 / 100 // 5% of remaining space
		}

		if extraForCol > remainingSpace {
			extraForCol = remainingSpace
		}

		finalWidths[col] += extraForCol
		remainingSpace -= extraForCol
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
	// Load config using the new store
	cfgStore, err := config.NewConfigStore()
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
	if cfgStore == nil { // Defensive check if NewConfigStore could return nil
		cfgStore = &config.ConfigStore{} // Needs definition if it can be nil
		initialError = "Critical error: Config store failed to initialize."
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
	ti.Width = 20

	m := &Model{
		uiState:       StatePortForwards,
		configStore:   cfgStore,
		portForwarder: pf,
		errorMsg:      initialError,
		width:         80, // Default width, will be updated on first WindowSizeMsg
		height:        24, // Default height, will be updated on first WindowSizeMsg
		groupStates:   make(map[string]*GroupState),
		groupingEnabled: true, // Enable grouping by default
		filterInput: ti,
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

		// Update filter input width to match terminal width (with some padding)
		filterWidth := m.width - 4 // Leave some padding
		if filterWidth < 20 {
			filterWidth = 20
		}
		m.filterInput.Width = filterWidth

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

// handleConfigReload processes Ctrl+R reload request
func (m *Model) handleConfigReload() (tea.Model, tea.Cmd) {
	// Clear previous messages
	m.errorMsg = ""
	m.statusMsg = ""

	// 1. Attempt config reload
	err := m.configStore.Reload()
	if err != nil {
		m.errorMsg = fmt.Sprintf("Config reload failed: %v", err)
		return m, nil
	}

	// 2. Get old and new configurations
	oldConfigs := m.configStore.GetPrevious()
	newConfigs := m.configStore.GetAll()

	// 3. Perform smart sync
	result, err := m.portForwarder.ReloadSync(oldConfigs, newConfigs)
	if err != nil {
		m.errorMsg = fmt.Sprintf("Reload sync failed: %v", err)
		return m, nil
	}

	// 4. Update UI state
	m.refreshTable()

	// 5. Show reload summary - use appropriate message field
	if len(result.Errors) > 0 {
		m.errorMsg = m.formatReloadSummary(result)
	} else {
		m.statusMsg = m.formatReloadSummary(result)
	}

	return m, nil
}

// formatReloadSummary creates user-friendly reload summary
func (m *Model) formatReloadSummary(result *k8s.ReloadResult) string {
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
		return fmt.Sprintf("Reload errors: %s", strings.Join(errorMsgs, "; "))
	}

	// Show successful reload summary
	parts := []string{}
	if len(result.Stopped) > 0 {
		parts = append(parts, fmt.Sprintf("%d stopped", len(result.Stopped)))
	}
	if len(result.Started) > 0 {
		parts = append(parts, fmt.Sprintf("%d started", len(result.Started)))
	}
	if len(result.Updated) > 0 {
		parts = append(parts, fmt.Sprintf("%d updated", len(result.Updated)))
	}

	if len(parts) > 0 {
		return fmt.Sprintf("Config reloaded: %s", strings.Join(parts, ", "))
	} else {
		return "Config reloaded: no changes needed"
	}
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
