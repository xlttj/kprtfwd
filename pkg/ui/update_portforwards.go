package ui

import (
	"errors"
	"fmt"
	"strconv"
	"strings"

	"github.com/xlttj/kprtfwd/pkg/config"
	"github.com/xlttj/kprtfwd/pkg/k8s"
	"github.com/xlttj/kprtfwd/pkg/logging"

	tea "github.com/charmbracelet/bubbletea"
)

// updatePortForwards handles updates for the StatePortForwards
func (m *Model) updatePortForwards(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	switch msg := msg.(type) {
	case tea.KeyMsg:
		// Handle edit mode first
		if m.editMode {
			switch msg.String() {
			case "esc":
				// Cancel edit mode
				m.editMode = false
				m.editInput.Blur()
				m.portForwardsTable.Focus()
				return m, nil
			case "enter":
				// Commit the edit
				return m.commitPortEdit()
			default:
				// Update edit input
				m.editInput, cmd = m.editInput.Update(msg)
				return m, cmd
			}
		}

		// Handle filter mode second
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
			m.errorMsg = ""  // Clear any errors
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
		case " ": // Space key for toggling
			m.errorMsg = ""  // Clear any previous error before attempting toggle
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
			m.errorMsg = ""  // Clear error
			m.statusMsg = "" // Clear status
			m.groupingEnabled = !m.groupingEnabled
			// Refresh table with new grouping mode
			m.refreshTable()
			return m, nil
		case "o": // Open in browser
			m.errorMsg = ""  // Clear error
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
		case "e": // Edit local port
			m.errorMsg = ""  // Clear any previous errors
			m.statusMsg = "" // Clear any previous status

			// Check if we can edit (not a group header)
			if m.groupingEnabled && m.isGroupHeaderSelected() {
				m.errorMsg = "Cannot edit group headers"
				return m, nil
			}

			// Get config index from the selected row
			selectedIdx, err := m.getConfigIndexFromTableRow()
			if err != nil {
				m.errorMsg = fmt.Sprintf("Cannot edit: %v", err)
				return m, nil
			}

			// Get the config to edit
			cfg, err := m.configStore.GetWithError(selectedIdx)
			if err != nil {
				m.errorMsg = fmt.Sprintf("Cannot get config to edit: %v", err)
				return m, nil
			}

			// Enter edit mode
			m.editMode = true
			m.editConfigIndex = selectedIdx
			m.editInput.SetValue(fmt.Sprintf("%d", cfg.PortLocal))
			m.editInput.Focus()
			m.portForwardsTable.Blur()
			return m, nil
		case ShortcutRestartForwards: // ctrl+r
			m.errorMsg = "" // Clear any previous errors
			return m.handlePortForwardsRestart()
		case ShortcutProjects: // ctrl+p
			// Switch to project selector
			return m.enterProjectSelector()
		case ShortcutDiscovery: // ctrl+d
			// Switch to service discovery
			return m.enterServiceDiscovery()

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

// commitPortEdit validates and applies the edited local port
func (m *Model) commitPortEdit() (tea.Model, tea.Cmd) {
	// Validate the input
	portStr := strings.TrimSpace(m.editInput.Value())
	if portStr == "" {
		m.errorMsg = "Port cannot be empty"
		m.editMode = false
		m.editInput.Blur()
		m.portForwardsTable.Focus()
		return m, nil
	}

	// Parse the port number
	newPort, err := strconv.Atoi(portStr)
	if err != nil {
		m.errorMsg = "Port must be a number"
		m.editMode = false
		m.editInput.Blur()
		m.portForwardsTable.Focus()
		return m, nil
	}

	// Validate port range
	if newPort < 1 || newPort > 65535 {
		m.errorMsg = "Port must be between 1 and 65535"
		m.editMode = false
		m.editInput.Blur()
		m.portForwardsTable.Focus()
		return m, nil
	}

	// Get the current config
	cfg, err := m.configStore.GetWithError(m.editConfigIndex)
	if err != nil {
		m.errorMsg = fmt.Sprintf("Cannot get config to update: %v", err)
		m.editMode = false
		m.editInput.Blur()
		m.portForwardsTable.Focus()
		return m, nil
	}

	// Check if port has actually changed
	if cfg.PortLocal == newPort {
		// No change, just exit edit mode
		m.editMode = false
		m.editInput.Blur()
		m.portForwardsTable.Focus()
		return m, nil
	}

	// Stop the port forward if it's currently running
	wasRunning := m.portForwarder.IsRunning(m.editConfigIndex)
	if wasRunning {
		err := m.portForwarder.Stop(m.editConfigIndex)
		if err != nil {
			logging.LogError("Error stopping port-forward %d for edit: %v", m.editConfigIndex, err)
			m.errorMsg = fmt.Sprintf("Error stopping %s for editing: %v", cfg.Service, err)
			m.editMode = false
			m.editInput.Blur()
			m.portForwardsTable.Focus()
			return m, nil
		}
	}

	// Update the config - use delete + add since we don't have update method
	// First, delete the old config
	if sqliteStore, ok := m.configStore.(*config.SQLiteConfigStore); ok {
		err = sqliteStore.DeletePortForward(cfg.ID)
		if err != nil {
			m.errorMsg = fmt.Sprintf("Error deleting old config: %v", err)
			m.editMode = false
			m.editInput.Blur()
			m.portForwardsTable.Focus()
			return m, nil
		}

		// Create updated config with new port
		updatedCfg := cfg
		updatedCfg.PortLocal = newPort

		// Add the updated config back
		err = m.configStore.Add(updatedCfg)
		if err != nil {
			m.errorMsg = fmt.Sprintf("Error updating config: %v", err)
			m.editMode = false
			m.editInput.Blur()
			m.portForwardsTable.Focus()
			return m, nil
		}

		// Find the new index after adding the config back
		newIndex, found := m.configStore.GetIndexByID(updatedCfg.ID)
		if !found {
			logging.LogError("Error: Could not find newly added config with ID %s", updatedCfg.ID)
			m.errorMsg = "Error: Could not locate updated config"
			m.editMode = false
			m.editInput.Blur()
			m.portForwardsTable.Focus()
			return m, nil
		}

		// If it was running before, start it with the new port
		if wasRunning {
			err = m.portForwarder.Start(newIndex, updatedCfg)
			if err != nil {
				logging.LogError("Error restarting port-forward %d after edit: %v", newIndex, err)
				m.errorMsg = fmt.Sprintf("Updated port but failed to restart %s: %v", cfg.Service, err)
			} else {
				m.statusMsg = fmt.Sprintf("Updated %s local port to %d and restarted", cfg.Service, newPort)
			}
		} else {
			m.statusMsg = fmt.Sprintf("Updated %s local port to %d", cfg.Service, newPort)
		}
	} else {
		m.errorMsg = "Update not supported with current config store"
	}

	// Exit edit mode and refresh table
	m.editMode = false
	m.editInput.Blur()
	m.portForwardsTable.Focus()
	m.refreshTable()
	return m, nil
}
