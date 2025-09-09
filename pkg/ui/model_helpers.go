package ui

import (
	"fmt"
	"sort"

	"github.com/xlttj/kprtfwd/pkg/config"
	"github.com/xlttj/kprtfwd/pkg/logging"

	"github.com/charmbracelet/bubbles/table"
)

// generatePortForwardRows converts config slice to table.Row slice (ungrouped)
func (m *Model) generatePortForwardRows(configs []config.PortForwardConfig) []table.Row {
	// If no text filtering is active, respect active project filtering
	actualConfigs := configs
	if !(m.filterMode || m.filterInput.Value() != "") {
		actualConfigs = m.configStore.GetActiveProjectForwards()
	}

	rows := make([]table.Row, 0, len(actualConfigs))
	allConfigs := m.configStore.GetAll()

	for _, cfg := range actualConfigs {
		// Determine actual runtime status by checking if port forward is running
		statusText := StatusStopped

		// Find the original index in the full config store using ID
		originalIndex := -1
		for j, origCfg := range allConfigs {
			if origCfg.ID == cfg.ID {
				originalIndex = j
				break
			}
		}

		if originalIndex == -1 {
			logging.LogDebug("Warning: Could not find original index for config ID %s", cfg.ID)
			continue // Skip this config if we can't find its index
		}

		// Check actual runtime state from PortForwarder
		if m.portForwarder.IsRunning(originalIndex) {
			statusText = StatusRunning
		}

		rows = append(rows, table.Row{
			cfg.Context,
			cfg.Namespace,
			cfg.Service,
			fmt.Sprintf("%d", cfg.PortRemote),
			fmt.Sprintf("%d", cfg.PortLocal),
			statusText,
		})
	}
	return rows
}

// generateGroupedRows creates grouped table rows with collapsible sections
func (m *Model) generateGroupedRows(configs []config.PortForwardConfig) []table.Row {
	if !m.groupingEnabled {
		return m.generatePortForwardRows(configs)
	}

	// If no text filtering is active, respect active project filtering
	actualConfigs := configs
	if !(m.filterMode || m.filterInput.Value() != "") {
		actualConfigs = m.configStore.GetActiveProjectForwards()
	}

	// Group configs by context
	groups := make(map[string][]struct {
		config config.PortForwardConfig
		index  int
	})

	// Always get all configs for index mapping
	allConfigs := m.configStore.GetAll()

	for _, cfg := range actualConfigs {
		groupKey := cfg.Context
		if groupKey == "" {
			groupKey = "(no context)"
		}
		// Find the original index in the full config store using ID
		originalIndex := -1
		for j, origCfg := range allConfigs {
			if origCfg.ID == cfg.ID {
				originalIndex = j
				break
			}
		}

		if originalIndex == -1 {
			logging.LogDebug("Warning: Could not find original index for config ID %s", cfg.ID)
			continue // Skip this config if we can't find its index
		}

		groups[groupKey] = append(groups[groupKey], struct {
			config config.PortForwardConfig
			index  int
		}{cfg, originalIndex})
	}

	// Sort group names
	groupNames := make([]string, 0, len(groups))
	for groupName := range groups {
		groupNames = append(groupNames, groupName)
	}
	sort.Strings(groupNames)

	// Initialize group states for new groups
	for _, groupName := range groupNames {
		if _, exists := m.groupStates[groupName]; !exists {
			m.groupStates[groupName] = &GroupState{
				Expanded: true, // Default to expanded
				Count:    len(groups[groupName]),
				Active:   0,
			}
		}
	}

	// Update counts and calculate active counts based on runtime state
	for groupName, items := range groups {
		state := m.groupStates[groupName]
		state.Count = len(items)
		state.Active = 0
		for _, item := range items {
			// Check actual runtime state instead of config file status
			if m.portForwarder.IsRunning(item.index) {
				state.Active++
			}
		}
	}

	// Generate table rows and enhanced rows
	var tableRows []table.Row
	m.tableRows = []TableRow{} // Reset enhanced rows

	for _, groupName := range groupNames {
		items := groups[groupName]
		state := m.groupStates[groupName]

		// Add group header row
		expandIcon := "▼"
		if !state.Expanded {
			expandIcon = "▶"
		}

		groupStatus := fmt.Sprintf("%d total, %d active", state.Count, state.Active)
		groupHeader := table.Row{
			fmt.Sprintf("%s %s", expandIcon, groupName),
			groupStatus,
			"", "", "", "", // Empty cells for other columns (no ID column)
		}
		tableRows = append(tableRows, groupHeader)
		m.tableRows = append(m.tableRows, TableRow{
			Type:        RowTypeGroup,
			ConfigIndex: -1,
			GroupName:   groupName,
			Data:        groupHeader,
		})

		// Add items if group is expanded
		if state.Expanded {
			for _, item := range items {
				cfg := item.config
				index := item.index

				// Determine actual runtime status by checking if port forward is running
				statusText := StatusStopped
				isRunning := m.portForwarder.IsRunning(index)
				if isRunning {
					statusText = StatusRunning
				}
				logging.LogDebug("UI Refresh: Config %d (%s) - IsRunning=%t, Status='%s'", index, cfg.ID, isRunning, statusText)

				// Indent service name to show hierarchy
				indentedService := "  " + cfg.Service

				itemRow := table.Row{
					"", // Empty context since it's shown in group header
					cfg.Namespace,
					indentedService,
					fmt.Sprintf("%d", cfg.PortRemote),
					fmt.Sprintf("%d", cfg.PortLocal),
					statusText,
				}
				tableRows = append(tableRows, itemRow)
				m.tableRows = append(m.tableRows, TableRow{
					Type:        RowTypeItem,
					ConfigIndex: index,
					GroupName:   groupName,
					Data:        itemRow,
				})
			}
		}
	}

	return tableRows
}

// getConfigIndexFromTableRow returns the config index for the current table selection
func (m *Model) getConfigIndexFromTableRow() (int, error) {
	selectedIdx := m.portForwardsTable.Cursor()

	// In ungrouped mode, handle filtered vs unfiltered data
	if !m.groupingEnabled {
		var configs []config.PortForwardConfig
		if (m.filterMode || m.filterInput.Value() != "") && m.filteredConfigs != nil {
			configs = m.filteredConfigs
		} else {
			configs = m.configStore.GetAll()
		}

		if selectedIdx < 0 || selectedIdx >= len(configs) {
			return -1, fmt.Errorf("invalid table selection")
		}

		// If filtering is active, find the original index
		if (m.filterMode || m.filterInput.Value() != "") && m.filteredConfigs != nil {
			selectedCfg := configs[selectedIdx]
			allConfigs := m.configStore.GetAll()
			for i, origCfg := range allConfigs {
				if origCfg.Context == selectedCfg.Context && origCfg.Namespace == selectedCfg.Namespace && origCfg.Service == selectedCfg.Service {
					return i, nil
				}
			}
			return -1, fmt.Errorf("could not find original config index for filtered item")
		}

		return selectedIdx, nil
	}

	// In grouped mode, use enhanced rows
	if selectedIdx < 0 || selectedIdx >= len(m.tableRows) {
		return -1, fmt.Errorf("invalid table selection")
	}

	row := m.tableRows[selectedIdx]
	if row.Type != RowTypeItem {
		return -1, fmt.Errorf("selected row is not a port forward item")
	}

	return row.ConfigIndex, nil
}

// isGroupHeaderSelected returns true if a group header is currently selected
func (m *Model) isGroupHeaderSelected() bool {
	selectedIdx := m.portForwardsTable.Cursor()
	if selectedIdx < 0 || selectedIdx >= len(m.tableRows) {
		return false
	}
	return m.tableRows[selectedIdx].Type == RowTypeGroup
}

// getSelectedGroupName returns the group name of the currently selected row
func (m *Model) getSelectedGroupName() string {
	selectedIdx := m.portForwardsTable.Cursor()
	if selectedIdx < 0 || selectedIdx >= len(m.tableRows) {
		return ""
	}
	return m.tableRows[selectedIdx].GroupName
}

// refreshTable refreshes the table based on current grouping mode and filter state
func (m *Model) refreshTable() {
	var configs []config.PortForwardConfig

	// Use filtered configs if filtering is active and we have filtered results
	if (m.filterMode || m.filterInput.Value() != "") && m.filteredConfigs != nil {
		configs = m.filteredConfigs
	} else {
		// Use all configs for proper index mapping, but we'll filter later if needed
		configs = m.configStore.GetAll()
	}

	if m.groupingEnabled {
		m.portForwardsTable.SetRows(m.generateGroupedRows(configs))
	} else {
		m.portForwardsTable.SetRows(m.generatePortForwardRows(configs))
	}
}

// getBaseConfigs returns the base set of configs respecting active project
func (m *Model) getBaseConfigs() []config.PortForwardConfig {
	return m.configStore.GetActiveProjectForwards()
}
