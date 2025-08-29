package ui

import (
	"fmt"
	"strings"

	"kprtfwd/pkg/config"
	"kprtfwd/pkg/logging"

	"github.com/charmbracelet/bubbles/table"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// enterProjectManagement switches to project management view
func (m *Model) enterProjectManagement() (tea.Model, tea.Cmd) {
	m.uiState = StateProjectManagement
	m.errorMsg = ""
	m.statusMsg = ""
	m.initializeProjectManagement()
	return m, nil
}

// initializeProjectManagement initializes the project management table
func (m *Model) initializeProjectManagement() {
	projects := m.configStore.GetAllProjects()

	// Create table columns for project management with dynamic widths
	columns := m.calculateProjectManagementColumns()

	// Create table rows - include "Create New Project" option at top
	rows := make([]table.Row, len(projects)+1)

	// Add "Create New Project" option at the top
	rows[0] = table.Row{"+ Create New Project", "", ""}

	// Add existing projects
	for i, project := range projects {
		actions := "Edit • Delete"
		rows[i+1] = table.Row{project.Name, fmt.Sprintf("%d", len(project.Forwards)), actions}
	}

	// Create and configure the table
	m.projectManagementTable = table.New(
		table.WithColumns(columns),
		table.WithRows(rows),
		table.WithFocused(true),
		table.WithHeight(min(len(rows)+2, m.height-8)),
	)

	// Apply table styles
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

	m.projectManagementTable.SetStyles(s)
}

// updateProjectManagement handles updates in the project management view
func (m *Model) updateProjectManagement(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	keyStr := msg.String()

	switch keyStr {
	case "esc":
		// Return to project selector
		m.uiState = StateProjectSelector
		m.errorMsg = ""
		m.statusMsg = ""
		m.initializeProjectSelector()
		return m, nil

	case "enter":
		// Handle project management action
		return m.handleProjectManagementAction()

	case "n", "c": // 'n' for new, 'c' for create
		// Create new project
		return m.enterProjectCreation()

	case "d": // 'd' for delete
		// Delete selected project
		return m.deleteSelectedProject()

	case "up", "k":
		// Move up in project list
		m.projectManagementTable, _ = m.projectManagementTable.Update(msg)
		return m, nil

	case "down", "j":
		// Move down in project list
		m.projectManagementTable, _ = m.projectManagementTable.Update(msg)
		return m, nil

	default:
		// Let the table handle other keys
		m.projectManagementTable, _ = m.projectManagementTable.Update(msg)
		return m, nil
	}
}

// handleProjectManagementAction processes the selected action in project management
func (m *Model) handleProjectManagementAction() (tea.Model, tea.Cmd) {
	selectedIdx := m.projectManagementTable.Cursor()

	if selectedIdx == 0 {
		// "Create New Project" selected
		return m.enterProjectCreation()
	}

	// Get the selected project (offset by 1 for the "Create New" option)
	projects := m.configStore.GetAllProjects()
	if selectedIdx-1 >= len(projects) {
		m.errorMsg = "Invalid project selection"
		return m, nil
	}

	selectedProject := projects[selectedIdx-1]

	// For now, we'll directly go to service selection for the project
	// Later we could add a submenu for Edit/Delete
	return m.enterProjectServiceSelection(selectedProject)
}

// enterProjectCreation switches to project creation view
func (m *Model) enterProjectCreation() (tea.Model, tea.Cmd) {
	m.uiState = StateProjectCreation
	m.errorMsg = ""
	m.statusMsg = ""
	m.projectNameInput.SetValue("")
	m.projectNameInput.Focus()
	return m, nil
}

// updateProjectCreation handles updates in the project creation view
func (m *Model) updateProjectCreation(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	keyStr := msg.String()

	switch keyStr {
	case "esc":
		// Cancel project creation
		m.uiState = StateProjectManagement
		m.projectNameInput.Blur()
		m.projectNameInput.SetValue("")
		m.errorMsg = ""
		m.statusMsg = ""
		m.initializeProjectManagement()
		return m, nil

	case "enter":
		// Create the project
		return m.createProject()

	default:
		// Update project name input
		m.projectNameInput, cmd = m.projectNameInput.Update(msg)
		return m, cmd
	}
}

// createProject validates and creates a new project
func (m *Model) createProject() (tea.Model, tea.Cmd) {
	projectName := strings.TrimSpace(m.projectNameInput.Value())

	// Validate project name
	if projectName == "" {
		m.errorMsg = "Project name cannot be empty"
		return m, nil
	}

	// Check if project already exists
	existingProjects := m.configStore.GetAllProjects()
	for _, project := range existingProjects {
		if project.Name == projectName {
			m.errorMsg = fmt.Sprintf("Project '%s' already exists", projectName)
			return m, nil
		}
	}

	// Create the project with no port forwards initially
	err := m.configStore.CreateProject(projectName, []string{})
	if err != nil {
		m.errorMsg = fmt.Sprintf("Failed to create project: %v", err)
		return m, nil
	}

	// Show success message and return to project management
	m.statusMsg = fmt.Sprintf("Created project '%s'", projectName)
	m.uiState = StateProjectManagement
	m.projectNameInput.Blur()
	m.projectNameInput.SetValue("")
	m.initializeProjectManagement()
	return m, nil
}

// enterProjectServiceSelection switches to service selection for a project
func (m *Model) enterProjectServiceSelection(project config.Project) (tea.Model, tea.Cmd) {
	m.uiState = StateProjectServiceSelection
	m.errorMsg = ""
	m.statusMsg = ""
	m.currentProject = &project
	m.initializeProjectServiceSelection()
	return m, nil
}

// initializeProjectServiceSelection initializes the service selection table for project editing
func (m *Model) initializeProjectServiceSelection() {
	allConfigs := m.configStore.GetAll()

	// Create table columns with dynamic widths
	columns := m.calculateServiceSelectionColumns()

	// Create table rows for all available services
	rows := make([]table.Row, len(allConfigs))

	// Create a map of port forward IDs in the current project for quick lookup
	projectForwards := make(map[string]bool)
	if m.currentProject != nil {
		for _, forwardID := range m.currentProject.Forwards {
			projectForwards[forwardID] = true
		}
	}

	for i, cfg := range allConfigs {
		checkbox := "☐"
		if projectForwards[cfg.ID] {
			checkbox = "☑"
		}

		ports := fmt.Sprintf("%d→%d", cfg.PortLocal, cfg.PortRemote)
		rows[i] = table.Row{checkbox, cfg.Service, cfg.Namespace, cfg.Context, ports}
	}

	// Create and configure the table
	m.projectServiceTable = table.New(
		table.WithColumns(columns),
		table.WithRows(rows),
		table.WithFocused(true),
		table.WithHeight(min(len(rows)+2, m.height-10)),
	)

	// Apply table styles
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

	m.projectServiceTable.SetStyles(s)
}

// updateProjectServiceSelection handles updates in the project service selection view
func (m *Model) updateProjectServiceSelection(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	keyStr := msg.String()

	switch keyStr {
	case "esc":
		// Return to project management
		m.uiState = StateProjectManagement
		m.errorMsg = ""
		m.statusMsg = ""
		m.currentProject = nil
		m.initializeProjectManagement()
		return m, nil

	case " ": // Space to toggle service in/out of project
		return m.toggleServiceInProject()

	case "up", "k":
		// Move up in service list
		m.projectServiceTable, _ = m.projectServiceTable.Update(msg)
		return m, nil

	case "down", "j":
		// Move down in service list
		m.projectServiceTable, _ = m.projectServiceTable.Update(msg)
		return m, nil

	default:
		// Let the table handle other keys
		m.projectServiceTable, _ = m.projectServiceTable.Update(msg)
		return m, nil
	}
}

// toggleServiceInProject adds or removes a service from the current project
func (m *Model) toggleServiceInProject() (tea.Model, tea.Cmd) {
	if m.currentProject == nil {
		m.errorMsg = "No project selected"
		return m, nil
	}

	selectedIdx := m.projectServiceTable.Cursor()
	allConfigs := m.configStore.GetAll()

	if selectedIdx < 0 || selectedIdx >= len(allConfigs) {
		m.errorMsg = "Invalid service selection"
		return m, nil
	}

	selectedConfig := allConfigs[selectedIdx]

	// Check if service is currently in project
	serviceInProject := false
	for _, forwardID := range m.currentProject.Forwards {
		if forwardID == selectedConfig.ID {
			serviceInProject = true
			break
		}
	}

	if serviceInProject {
		// Remove service from project
		err := m.removeServiceFromProject(selectedConfig.ID)
		if err != nil {
			m.errorMsg = fmt.Sprintf("Failed to remove service: %v", err)
		} else {
			m.statusMsg = fmt.Sprintf("Removed %s from project %s", selectedConfig.Service, m.currentProject.Name)
		}
	} else {
		// Add service to project
		err := m.addServiceToProject(selectedConfig.ID)
		if err != nil {
			m.errorMsg = fmt.Sprintf("Failed to add service: %v", err)
		} else {
			m.statusMsg = fmt.Sprintf("Added %s to project %s", selectedConfig.Service, m.currentProject.Name)
		}
	}

	// Preserve cursor position and refresh the service table
	cursorPos := m.projectServiceTable.Cursor()
	m.initializeProjectServiceSelection()
	m.projectServiceTable.SetCursor(cursorPos)
	return m, nil
}

// addServiceToProject adds a service to the current project
func (m *Model) addServiceToProject(serviceID string) error {
	if m.currentProject == nil {
		return fmt.Errorf("no project selected")
	}

	// Update the project with the new service
	updatedForwards := append(m.currentProject.Forwards, serviceID)

	// Delete and recreate project (since we don't have an update method)
	err := m.configStore.DeleteProject(m.currentProject.Name)
	if err != nil {
		return fmt.Errorf("failed to delete project for update: %w", err)
	}

	err = m.configStore.CreateProject(m.currentProject.Name, updatedForwards)
	if err != nil {
		return fmt.Errorf("failed to recreate project: %w", err)
	}

	// Update our local project reference
	m.currentProject.Forwards = updatedForwards

	logging.LogDebug("Added service %s to project %s", serviceID, m.currentProject.Name)
	return nil
}

// removeServiceFromProject removes a service from the current project
func (m *Model) removeServiceFromProject(serviceID string) error {
	if m.currentProject == nil {
		return fmt.Errorf("no project selected")
	}

	// Create new forwards list without the specified service
	updatedForwards := make([]string, 0, len(m.currentProject.Forwards))
	found := false
	for _, forwardID := range m.currentProject.Forwards {
		if forwardID != serviceID {
			updatedForwards = append(updatedForwards, forwardID)
		} else {
			found = true
		}
	}

	if !found {
		return fmt.Errorf("service not found in project")
	}

	// Delete and recreate project (since we don't have an update method)
	err := m.configStore.DeleteProject(m.currentProject.Name)
	if err != nil {
		return fmt.Errorf("failed to delete project for update: %w", err)
	}

	err = m.configStore.CreateProject(m.currentProject.Name, updatedForwards)
	if err != nil {
		return fmt.Errorf("failed to recreate project: %w", err)
	}

	// Update our local project reference
	m.currentProject.Forwards = updatedForwards

	logging.LogDebug("Removed service %s from project %s", serviceID, m.currentProject.Name)
	return nil
}

// deleteSelectedProject deletes the currently selected project
func (m *Model) deleteSelectedProject() (tea.Model, tea.Cmd) {
	selectedIdx := m.projectManagementTable.Cursor()

	if selectedIdx == 0 {
		// "Create New Project" selected - can't delete
		m.errorMsg = "Cannot delete the 'Create New Project' option"
		return m, nil
	}

	// Get the selected project (offset by 1 for the "Create New" option)
	projects := m.configStore.GetAllProjects()
	if selectedIdx-1 >= len(projects) {
		m.errorMsg = "Invalid project selection"
		return m, nil
	}

	selectedProject := projects[selectedIdx-1]

	// Check if this is the currently active project
	activeProjectName := m.configStore.GetActiveProjectName()
	if activeProjectName == selectedProject.Name {
		// Clear active project before deleting
		m.configStore.ClearActiveProject()
		m.refreshTable() // Refresh main table to show all projects
	}

	// Delete the project
	err := m.configStore.DeleteProject(selectedProject.Name)
	if err != nil {
		m.errorMsg = fmt.Sprintf("Failed to delete project '%s': %v", selectedProject.Name, err)
		return m, nil
	}

	// Show success message and refresh the table
	m.statusMsg = fmt.Sprintf("Deleted project '%s'", selectedProject.Name)
	m.initializeProjectManagement()
	return m, nil
}
