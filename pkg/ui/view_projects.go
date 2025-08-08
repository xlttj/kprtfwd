package ui

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/lipgloss"
)

// renderProjectSelector renders the project selector view
func (m Model) renderProjectSelector() string {
	var b strings.Builder

	// Header
	titleStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorTitle)).
		Bold(true).
		Padding(0, 1)
	
	b.WriteString(titleStyle.Render("📁 Project Selector"))
	b.WriteString("\n\n")

	// Show current active project
	activeProject := m.configStore.GetActiveProjectName()
	if activeProject != "" {
		b.WriteString(fmt.Sprintf("Current: %s\n\n", activeProject))
	} else {
		b.WriteString("Current: All Projects\n\n")
	}

	// Render the project table
	b.WriteString(m.projectSelector.View())
	b.WriteString("\n\n")

	// Action hints
	helpStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorHelp))
	
	b.WriteString(helpStyle.Render(ActionProjectSelector))
	b.WriteString("\n")

	// Error or status message
	if m.errorMsg != "" {
		errorStyle := lipgloss.NewStyle().
			Foreground(lipgloss.Color(ColorError)).
			Bold(true)
		b.WriteString(errorStyle.Render(fmt.Sprintf("Error: %s", m.errorMsg)))
		b.WriteString("\n")
	} else if m.statusMsg != "" {
		b.WriteString(m.statusMsg)
		b.WriteString("\n")
	}

	return b.String()
}
