package ui

import (
	"errors"
	"fmt"

	"kprtfwd/pkg/config"
	"kprtfwd/pkg/k8s"
	"kprtfwd/pkg/logging"

	tea "github.com/charmbracelet/bubbletea"
)

// updatePortForwards handles updates for the StatePortForwards
func (m *Model) updatePortForwards(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	switch msg := msg.(type) {
	case tea.KeyMsg:
		// Handle filter mode first
		if m.filterMode {
			switch msg.String() {
			case "esc":
				// Exit filter mode
				m.filterMode = false
				m.filterInput.Blur()
				m.filterInput.SetValue("")
				m.filteredConfigs = nil
				m.refreshTable()
				m.portForwardsTable.Focus()
				return m, nil
			case "enter":
				// Exit filter mode but keep filter applied
				m.filterMode = false
				m.filterInput.Blur()
				m.portForwardsTable.Focus()
				return m, nil
			default:
				// Update filter input and apply filter
				m.filterInput, cmd = m.filterInput.Update(msg)
				m.applyFilter()
				m.refreshTable()
				return m, cmd
			}
		}
		
		switch msg.String() {
		case "/":
			// Enter filter mode
			m.errorMsg = "" // Clear any errors
			m.statusMsg = "" // Clear any status messages
			m.filterMode = true
			m.filterInput.Focus()
			m.portForwardsTable.Blur()
			// Don't add the "/" character to the input
			return m, nil
		case "q": // Keep 'q' for quit as an alternative?
			return m, tea.Quit
		case "esc":
			// If there's an active filter but we're not in filter mode, clear it
			if !m.filterMode && m.filterInput.Value() != "" {
				m.filterInput.SetValue("")
				m.filteredConfigs = nil
				m.refreshTable()
				return m, nil
			}
			// Do nothing, as there's no menu to go back to.
			// Previously: m.uiState = StateMenu
			return m, nil
		// Handle keys needed by the table component (like up/down/j/k/pgup/pgdown)
		// Let the table handle navigation first
		// case "up", "k": moved to table handling
		// case "down", "j": moved to table handling
		case " ": // Space key for toggling
			m.errorMsg = "" // Clear any previous error before attempting toggle
			m.statusMsg = "" // Clear any previous status message

			// Check if group header is selected (only in grouped mode)
			if m.groupingEnabled && m.isGroupHeaderSelected() {
				// Toggle group expand/collapse
				groupName := m.getSelectedGroupName()
				if state, exists := m.groupStates[groupName]; exists {
					state.Expanded = !state.Expanded
					// Refresh table with updated group state
					m.portForwardsTable.SetRows(m.generateGroupedRows(m.configStore.GetAll()))
				}
				return m, nil
			}

			// Get config index from the enhanced row
			selectedIdx, err := m.getConfigIndexFromTableRow()
			if err != nil {
				m.errorMsg = fmt.Sprintf("Cannot toggle: %v", err)
				return m, nil
			}

			// Use GetWithError to check existence and retrieve config
			cfg, err := m.configStore.GetWithError(selectedIdx)
			if err != nil {
				if errors.Is(err, config.ErrConfigNotFound) {
					m.errorMsg = fmt.Sprintf("Cannot toggle: %v", err) // Show specific config not found error
				} else {
					m.errorMsg = fmt.Sprintf("Error retrieving config %d: %v", selectedIdx, err) // Generic error
				}
				return m, nil
			}

			// Check current runtime state to determine toggle action
			if m.portForwarder.IsRunning(selectedIdx) { // Currently running - stop it
				err := m.portForwarder.Stop(selectedIdx)
				if err != nil {
					logging.LogError("Error stopping port-forward %d: %v", selectedIdx, err)
					m.errorMsg = fmt.Sprintf("Error stopping %s: %v", cfg.Service, err)
				}
				// Refresh table to show updated runtime status
				m.refreshTable()
				return m, nil
			} else { // Currently stopped - start it
				err := m.portForwarder.Start(selectedIdx, cfg)
				if err != nil {
					if errors.Is(err, k8s.ErrPortInUse) {
						m.errorMsg = fmt.Sprintf("Cannot start %s: %v", cfg.Service, err)
					} else {
						m.errorMsg = fmt.Sprintf("Error starting %s: %v", cfg.Service, err)
					}
					return m, nil
				}
				// Refresh table to show updated runtime status
				m.refreshTable()
				return m, nil
			}
		case "g": // Toggle grouping mode
			m.errorMsg = "" // Clear error
			m.statusMsg = "" // Clear status
			m.groupingEnabled = !m.groupingEnabled
			// Refresh table with new grouping mode
			m.refreshTable()
			return m, nil
		case "o": // Open in browser
			m.errorMsg = "" // Clear error
			m.statusMsg = "" // Clear status
			
			// Get config index from the selected row
			selectedIdx, err := m.getConfigIndexFromTableRow()
			if err != nil {
				m.errorMsg = fmt.Sprintf("Cannot open URL: %v", err)
				return m, nil
			}
			
			// Get the config for the selected port forward
			cfg, err := m.configStore.GetWithError(selectedIdx)
			if err != nil {
				m.errorMsg = fmt.Sprintf("Cannot get config: %v", err)
				return m, nil
			}
			
			// Check if the port forward is running
			if !m.portForwarder.IsRunning(selectedIdx) {
				m.errorMsg = fmt.Sprintf("Cannot open URL: %s is not running", cfg.Service)
				return m, nil
			}
			
			// Open the HTTP URL in browser
			err = m.openInBrowser(cfg)
			if err != nil {
				m.errorMsg = fmt.Sprintf("Failed to open browser: %v", err)
			} else {
				m.statusMsg = fmt.Sprintf("Opened http://localhost:%d in browser", cfg.PortLocal)
			}
			return m, nil
		case ShortcutReloadConfig: // ctrl+r
			m.errorMsg = "" // Clear any previous errors
			return m.handleConfigReload()
		case ShortcutProjects: // ctrl+p
			// Switch to project selector
			return m.enterProjectSelector()

		// Default case for keys not handled above: pass to table
		default:
			m.portForwardsTable, cmd = m.portForwardsTable.Update(msg)
			return m, cmd
		}
	}
	// Pass other non-key messages to the table
	m.portForwardsTable, cmd = m.portForwardsTable.Update(msg)
	return m, cmd
}
