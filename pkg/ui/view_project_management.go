package ui

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/lipgloss"
)

// renderProjectManagement renders the project management view
func (m *Model) renderProjectManagement() string {
	var b strings.Builder

	// Header
	titleStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorTitle)).
		Bold(true).
		Padding(0, 1)

	b.WriteString(titleStyle.Render("🛠️  Project Management"))
	b.WriteString("\n\n")

	// Instructions
	helpStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorHelp))

	b.WriteString(helpStyle.Render("Select a project to edit, or create a new one"))
	b.WriteString("\n\n")

	// Render the project management table
	b.WriteString(m.projectManagementTable.View())
	b.WriteString("\n\n")

	// Action hints
	actions := "↑/↓: Navigate | Enter: Select | N/C: New Project | D: Delete | Esc: Back"
	b.WriteString(helpStyle.Render(actions))
	b.WriteString("\n")

	// Error or status message
	if m.errorMsg != "" {
		errorStyle := lipgloss.NewStyle().
			Foreground(lipgloss.Color(ColorError)).
			Bold(true)
		b.WriteString(errorStyle.Render(fmt.Sprintf("Error: %s", m.errorMsg)))
		b.WriteString("\n")
	} else if m.statusMsg != "" {
		statusStyle := lipgloss.NewStyle().
			Foreground(lipgloss.Color("10")) // Green
		b.WriteString(statusStyle.Render(m.statusMsg))
		b.WriteString("\n")
	}

	return b.String()
}

// renderProjectCreation renders the project creation view
func (m *Model) renderProjectCreation() string {
	var b strings.Builder

	// Header
	titleStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorTitle)).
		Bold(true).
		Padding(0, 1)

	b.WriteString(titleStyle.Render("➕ Create New Project"))
	b.WriteString("\n\n")

	// Instructions
	helpStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorHelp))

	b.WriteString(helpStyle.Render("Enter a name for the new project:"))
	b.WriteString("\n\n")

	// Project name input
	inputStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color("6")) // Cyan

	b.WriteString(inputStyle.Render("Project Name: "))
	b.WriteString(m.projectNameInput.View())
	b.WriteString("\n\n")

	// Action hints
	actions := "Enter: Create Project | Esc: Cancel"
	b.WriteString(helpStyle.Render(actions))
	b.WriteString("\n")

	// Error or status message
	if m.errorMsg != "" {
		errorStyle := lipgloss.NewStyle().
			Foreground(lipgloss.Color(ColorError)).
			Bold(true)
		b.WriteString(errorStyle.Render(fmt.Sprintf("Error: %s", m.errorMsg)))
		b.WriteString("\n")
	} else if m.statusMsg != "" {
		statusStyle := lipgloss.NewStyle().
			Foreground(lipgloss.Color("10")) // Green
		b.WriteString(statusStyle.Render(m.statusMsg))
		b.WriteString("\n")
	}

	return b.String()
}

// renderProjectServiceSelection renders the project service selection view
func (m *Model) renderProjectServiceSelection() string {
	var b strings.Builder

	// Header
	titleStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorTitle)).
		Bold(true).
		Padding(0, 1)

	projectName := "Unknown"
	if m.currentProject != nil {
		projectName = m.currentProject.Name
	}

	b.WriteString(titleStyle.Render(fmt.Sprintf("🔧 Edit Project: %s", projectName)))
	b.WriteString("\n\n")

	// Instructions
	helpStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorHelp))

	b.WriteString(helpStyle.Render("Use Space to add/remove services from the project:"))
	b.WriteString("\n\n")

	// Render the service selection table
	b.WriteString(m.projectServiceTable.View())
	b.WriteString("\n\n")

	// Action hints
	actions := "↑/↓: Navigate | Space: Toggle Service | Esc: Back"
	b.WriteString(helpStyle.Render(actions))
	b.WriteString("\n")

	// Error or status message
	if m.errorMsg != "" {
		errorStyle := lipgloss.NewStyle().
			Foreground(lipgloss.Color(ColorError)).
			Bold(true)
		b.WriteString(errorStyle.Render(fmt.Sprintf("Error: %s", m.errorMsg)))
		b.WriteString("\n")
	} else if m.statusMsg != "" {
		statusStyle := lipgloss.NewStyle().
			Foreground(lipgloss.Color("10")) // Green
		b.WriteString(statusStyle.Render(m.statusMsg))
		b.WriteString("\n")
	}

	return b.String()
}
