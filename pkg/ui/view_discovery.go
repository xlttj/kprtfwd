package ui

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/lipgloss"
)

// renderServiceDiscoveryView renders the service discovery interface
func (m *Model) renderServiceDiscoveryView() string {
	switch m.discoveryPhase {
	case PhaseClusterSelection:
		return m.renderClusterSelectionView()
	case PhaseServiceSelection:
		return m.renderServiceSelectionView()
	default:
		return "Unknown discovery phase"
	}
}

// renderClusterSelectionView renders the cluster selection phase
func (m *Model) renderClusterSelectionView() string {
	var content strings.Builder

	// Title
	titleStyle := lipgloss.NewStyle().
		Bold(true).
		Foreground(lipgloss.Color(ColorTitle)).
		MarginBottom(1)

	content.WriteString(titleStyle.Render("Service Discovery - Select Cluster"))
	content.WriteString("\n\n")

	// Instructions
	helpStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorHelp)).
		MarginBottom(1)

	content.WriteString(helpStyle.Render("Select a Kubernetes cluster to discover services:"))
	content.WriteString("\n\n")

	// Table
	content.WriteString(m.discoveryTable.View())
	content.WriteString("\n\n")

	// Controls
	controlsStyle := lipgloss.NewStyle().
		Foreground(lipgloss.Color(ColorHelp))

	content.WriteString(controlsStyle.Render("↑/↓: Navigate | Enter: Select | Esc: Cancel"))

	return content.String()
}

// renderServiceSelectionView renders the service selection phase
func (m *Model) renderServiceSelectionView() string {
	var content strings.Builder

	// Headline (forced two lines to ensure visibility across terminals)
	titleStyle := lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color(ColorTitle))
	helpStyle := lipgloss.NewStyle().Foreground(lipgloss.Color(ColorHelp))
	clusterName := ""
	if m.discoverySelectedCluster >= 0 && m.discoverySelectedCluster < len(m.discoveryClusters) {
		clusterName = m.discoveryClusters[m.discoverySelectedCluster]
	}
	content.WriteString(titleStyle.Render(fmt.Sprintf("Service Discovery — %s", clusterName)))
	content.WriteString("\n")
	content.WriteString(helpStyle.Render("Space: Toggle | e: Edit local port (new only) | /: Filter | Enter: Confirm | Esc: Back"))
	content.WriteString("\n\n")

	// Always show filter area to prevent layout shift
	if m.discoveryFilterMode {
		// Show the filter input with styled box
		filterStyle := lipgloss.NewStyle().
			Border(lipgloss.NormalBorder()).
			BorderForeground(lipgloss.Color(ColorBorder)).
			Padding(0, 1)

		content.WriteString(filterStyle.Render("Filter: " + m.discoveryFilterInput.View()))
		content.WriteString("\n\n")
	} else if m.discoveryFilterInput.Value() != "" {
		// Show the current filter when not in edit mode with styled box
		filterStyle := lipgloss.NewStyle().
			Border(lipgloss.NormalBorder()).
			BorderForeground(lipgloss.Color("8")). // Grey border for inactive
			Foreground(lipgloss.Color("8")).       // Grey text for inactive
			Padding(0, 1)

		content.WriteString(filterStyle.Render(fmt.Sprintf("Filter: %s (Press / to edit, Esc to clear)", m.discoveryFilterInput.Value())))
		content.WriteString("\n\n")
	} else {
		// Create a placeholder box to maintain consistent layout
		placeholderStyle := lipgloss.NewStyle().
			Border(lipgloss.NormalBorder()).
			BorderForeground(lipgloss.Color("240")). // Very dim border
			Foreground(lipgloss.Color("240")).       // Very dim text
			Padding(0, 1)

		content.WriteString(placeholderStyle.Render("Press / to filter..."))
		content.WriteString("\n\n")
	}

	// Instructions for selection count
	selectedCount := 0
	for _, port := range m.discoveryPorts {
		if port.Selected {
			selectedCount++
		}
	}
	content.WriteString(helpStyle.Render(fmt.Sprintf("Select ports to add (%d selected):", selectedCount)))
	content.WriteString("\n\n")

	// Table
	content.WriteString(m.discoveryTable.View())
	content.WriteString("\n\n")

	// Controls at bottom (for narrower screens or reinforcement)
	if m.discoveryEditMode {
		content.WriteString(helpStyle.Render("Type port number | Enter: Confirm | Esc: Cancel edit"))
	} else if m.discoveryFilterMode {
		content.WriteString(helpStyle.Render("Type to filter | Enter: Apply filter | Esc: Clear filter"))
	} else {
		content.WriteString(helpStyle.Render("↑/↓: Navigate | Space: Toggle | e: Edit local port (new only) | /: Filter | Enter: Confirm | Esc: Back"))
	}

	return content.String()
}
