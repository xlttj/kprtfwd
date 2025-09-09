package ui

import (
	"fmt"
	"strings"

	"github.com/xlttj/kprtfwd/pkg/logging"

	"github.com/charmbracelet/lipgloss"
)

// View renders the current model state
func (m *Model) View() string {
	logging.LogDebug("View called with uiState = %d", m.uiState)

	switch m.uiState {
	case StatePortForwards:
		return m.viewPortForwards()
	case StateProjectSelector:
		return m.renderProjectSelector()
	case StateServiceDiscovery:
		return m.renderServiceDiscoveryView()
	case StateProjectManagement:
		return m.renderProjectManagement()
	case StateProjectCreation:
		return m.renderProjectCreation()
	case StateProjectServiceSelection:
		return m.renderProjectServiceSelection()
	}
	return "Unknown state"
}

// viewPortForwards renders the port-forward list view
func (m *Model) viewPortForwards() string {
	// Set page title with active project info
	var titleText string
	activeProject := m.configStore.GetActiveProjectName()
	if activeProject != "" {
		titleText = fmt.Sprintf("Port Forwards - Project: %s", activeProject)
	} else {
		titleText = "Port Forwards - All Projects"
	}
	title := lipgloss.NewStyle().Foreground(lipgloss.Color(ColorTitle)).Bold(true).Render(titleText)

	// Render help text based on screen width (include edit shortcut)
	help := "Space: Toggle/Expand | E: Edit Port | G: Group Mode | O: Open URL | /: Filter | Ctrl+P: Projects | Q: Quit"
	if m.width < 80 {
		help = "Space:Toggle | E:Edit | G:Group | O:Open | /:Filter | Ctrl+P:Projects | Q:Quit"
	}

	// Style help text
	helpStyle := lipgloss.NewStyle().Foreground(lipgloss.Color(ColorHelp))
	helpText := helpStyle.Render(help)

	// Render table
	tableView := lipgloss.PlaceHorizontal(m.width, lipgloss.Left, m.portForwardsTable.View())

	// Always reserve space for the filter input to prevent layout shift
	var filterView string
	if m.filterMode {
		// Show the filter input with styled box
		filterStyle := lipgloss.NewStyle().
			Border(lipgloss.NormalBorder()).
			BorderForeground(lipgloss.Color(ColorBorder)).
			Padding(0, 1)

		filterView = filterStyle.Render("Filter: " + m.filterInput.View())
	} else if m.filterInput.Value() != "" {
		// Show the current filter when not in edit mode with styled box
		filterStyle := lipgloss.NewStyle().
			Border(lipgloss.NormalBorder()).
			BorderForeground(lipgloss.Color("8")). // Grey border for inactive
			Foreground(lipgloss.Color("8")).       // Grey text for inactive
			Padding(0, 1)

		filterView = filterStyle.Render(fmt.Sprintf("Filter: %s (Press / to edit, Esc to clear)", m.filterInput.Value()))
	} else {
		// Create a placeholder box to maintain consistent layout
		placeholderStyle := lipgloss.NewStyle().
			Border(lipgloss.NormalBorder()).
			BorderForeground(lipgloss.Color("240")). // Very dim border
			Foreground(lipgloss.Color("240")).       // Very dim text
			Padding(0, 1)

		filterView = placeholderStyle.Render("Press / to filter...")
	}

	// Handle inline edit input display
	var editView string
	if m.editMode {
		// Show the edit input with a label
		editStyle := lipgloss.NewStyle().Foreground(lipgloss.Color("11")) // Yellow for edit label
		editLabel := editStyle.Render("Edit Local Port: ")
		editView = editLabel + m.editInput.View() + " (Enter to save, Esc to cancel)"
	}

	// Format top area: title and potentially help text (if room)
	var top string
	if m.width >= 80 {
		// Calculate spacing, ensure it's not negative
		spacing := m.width - lipgloss.Width(title) - lipgloss.Width(helpText)
		if spacing > 0 {
			top = lipgloss.JoinHorizontal(lipgloss.Left, title, strings.Repeat(" ", spacing), helpText)
		} else {
			// Not enough room for both, just show title
			top = title
		}
	} else {
		top = title
	}

	// Format bottom area (for narrower screens)
	var bottom string
	if m.width < 80 {
		bottom = helpStyle.Render(help)
	}

	// Generate message text (error or status)
	var messageText string
	if m.errorMsg != "" {
		errorStyle := lipgloss.NewStyle().Foreground(lipgloss.Color(ColorError))
		messageText = errorStyle.Render(fmt.Sprintf("ERROR: %s", m.errorMsg))
	} else if m.statusMsg != "" {
		// Use a different color for status messages (green for success)
		statusStyle := lipgloss.NewStyle().Foreground(lipgloss.Color("10")) // Green
		messageText = statusStyle.Render(m.statusMsg)
	}

	// Generate output with message, filter, and edit view
	var output string
	if m.editMode {
		// Include edit view when in edit mode
		if messageText != "" {
			if m.width < 80 {
				output = lipgloss.JoinVertical(lipgloss.Left, top, "", filterView, tableView, editView, messageText, bottom)
			} else {
				output = lipgloss.JoinVertical(lipgloss.Left, top, "", filterView, tableView, editView, messageText)
			}
		} else {
			if m.width < 80 {
				output = lipgloss.JoinVertical(lipgloss.Left, top, "", filterView, tableView, editView, bottom)
			} else {
				output = lipgloss.JoinVertical(lipgloss.Left, top, "", filterView, tableView, editView)
			}
		}
	} else {
		// Normal view without edit input
		if messageText != "" {
			if m.width < 80 {
				output = lipgloss.JoinVertical(lipgloss.Left, top, "", filterView, tableView, messageText, bottom)
			} else {
				output = lipgloss.JoinVertical(lipgloss.Left, top, "", filterView, tableView, messageText)
			}
		} else {
			if m.width < 80 {
				output = lipgloss.JoinVertical(lipgloss.Left, top, "", filterView, tableView, bottom)
			} else {
				output = lipgloss.JoinVertical(lipgloss.Left, top, "", filterView, tableView)
			}
		}
	}

	return output
}
