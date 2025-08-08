package ui

import (
	"fmt"
	"strings"

	"kprtfwd/pkg/logging"

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

	// Render help text based on screen width (include filter shortcut)
	help := "Space: Toggle/Expand | G: Group Mode | O: Open URL | /: Filter | Ctrl+P: Projects | Q: Quit"
	if m.width < 80 {
		help = "Space:Toggle | G:Group | O:Open | /:Filter | Ctrl+P:Projects | Q:Quit"
	}

	// Style help text
	helpStyle := lipgloss.NewStyle().Foreground(lipgloss.Color(ColorHelp))
	helpText := helpStyle.Render(help)

	// Render table
	tableView := lipgloss.PlaceHorizontal(m.width, lipgloss.Left, m.portForwardsTable.View())

	// Reserve space for the filter input to prevent layout shift
	var filterView string
	if m.filterMode {
		// Show the filter input with a label
		filterStyle := lipgloss.NewStyle().Foreground(lipgloss.Color("6")) // Cyan for filter label
		filterLabel := filterStyle.Render("Filter: ")
		filterView = filterLabel + m.filterInput.View()
	} else if m.filterInput.Value() != "" {
		// Show the current filter when not in edit mode
		filterStyle := lipgloss.NewStyle().Foreground(lipgloss.Color("8")) // Grey for inactive filter
		filterView = filterStyle.Render("Filter: " + m.filterInput.Value() + " (Press / to edit, Esc to clear)")
	} else {
		// Create an empty line with the same height as the filter input
		filterView = ""
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

	// Generate output with message and filter
	var output string
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

	return output
}
