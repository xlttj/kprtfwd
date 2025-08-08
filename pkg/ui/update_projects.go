package ui

import (
	"fmt"

	"kprtfwd/pkg/config"
	"kprtfwd/pkg/logging"

	"github.com/charmbracelet/bubbles/table"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// updateProjectSelector handles updates in the project selector view
func (m *Model) updateProjectSelector(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	keyStr := msg.String()

	switch keyStr {
	case "esc":
		// Return to port forwards view
		m.uiState = StatePortForwards
		m.errorMsg = ""
		m.statusMsg = ""
		return m, nil

	case "enter":
		// Select the highlighted project
		return m.handleProjectSelection()


	case "up", "k":
		// Move up in project list
		m.projectSelector, _ = m.projectSelector.Update(msg)
		return m, nil

	case "down", "j":
		// Move down in project list
		m.projectSelector, _ = m.projectSelector.Update(msg)
		return m, nil

	default:
		// Let the table handle other keys
		m.projectSelector, _ = m.projectSelector.Update(msg)
		return m, nil
	}
}


// initializeProjectSelector initializes the project selector table
func (m *Model) initializeProjectSelector() {
	projects := m.configStore.GetAllProjects()
	activeProjectName := m.configStore.GetActiveProjectName()

	// Create table columns for projects
	columns := []table.Column{
		{Title: "PROJECT", Width: 30},
		{Title: "FORWARDS", Width: 15},
		{Title: "ACTIVE", Width: 10},
	}

	// Create table rows
	rows := make([]table.Row, len(projects)+1) // +1 for "All Projects" option

	// Add "All Projects" option at the top
	allStatus := ""
	if activeProjectName == "" {
		allStatus = "●"
	}
	rows[0] = table.Row{"All Projects", fmt.Sprintf("%d", len(m.configStore.GetAll())), allStatus}

	// Add actual projects
	for i, project := range projects {
		activeStatus := ""
		if project.Name == activeProjectName {
			activeStatus = "●"
		}
		rows[i+1] = table.Row{project.Name, fmt.Sprintf("%d", len(project.Forwards)), activeStatus}
	}

	// Create and configure the table
	m.projectSelector = table.New(
		table.WithColumns(columns),
		table.WithRows(rows),
		table.WithFocused(true),
		table.WithHeight(min(len(rows)+2, m.height-6)),
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

	m.projectSelector.SetStyles(s)
}

// handleProjectSelection processes project selection
func (m *Model) handleProjectSelection() (tea.Model, tea.Cmd) {
	selectedIdx := m.projectSelector.Cursor()
	
	// Step 1: Stop all currently running port forwards
	m.stopAllRunningPortForwards()
	
	if selectedIdx == 0 {
		// "All Projects" selected - clear active project
		m.configStore.ClearActiveProject()
		m.statusMsg = "Showing all port forwards (all running forwards stopped)"
	} else {
		// Actual project selected
		projects := m.configStore.GetAllProjects()
		if selectedIdx-1 < len(projects) {
			selectedProject := projects[selectedIdx-1]
			err := m.configStore.SetActiveProject(selectedProject.Name)
			if err != nil {
				m.errorMsg = fmt.Sprintf("Failed to set active project: %v", err)
			} else {
				// Step 2: Start all port forwards in the selected project
				startedCount, startErrors := m.startProjectPortForwards(selectedProject)
				
				if len(startErrors) > 0 {
					m.errorMsg = fmt.Sprintf("Project '%s' activated, started %d/%d forwards. Errors: %s", 
						selectedProject.Name, startedCount, len(selectedProject.Forwards), 
						startErrors[0]) // Show first error
				} else {
					m.statusMsg = fmt.Sprintf("Project '%s' activated, started %d forwards", 
						selectedProject.Name, startedCount)
				}
			}
		}
	}

	// Refresh the port forwards table and return to main view
	m.refreshTable()
	m.uiState = StatePortForwards
	return m, nil
}


// enterProjectSelector switches to project selector view
func (m *Model) enterProjectSelector() (tea.Model, tea.Cmd) {
	m.uiState = StateProjectSelector
	m.errorMsg = ""
	m.statusMsg = ""
	m.initializeProjectSelector()
	return m, nil
}

// stopAllRunningPortForwards stops all currently running port forwards
func (m *Model) stopAllRunningPortForwards() {
	allConfigs := m.configStore.GetAll()
	stoppedCount := 0
	
	for i := range allConfigs {
		if m.portForwarder.IsRunning(i) {
			err := m.portForwarder.Stop(i)
			if err != nil {
				logging.LogError("Failed to stop port forward %d during project selection: %v", i, err)
			} else {
				stoppedCount++
				logging.LogDebug("Stopped port forward %d during project selection", i)
			}
		}
	}
	
	if stoppedCount > 0 {
		logging.LogDebug("Stopped %d running port forwards during project selection", stoppedCount)
	}
}

// startProjectPortForwards starts all port forwards in the given project
// Returns the number of successfully started forwards and a list of error messages
func (m *Model) startProjectPortForwards(project config.Project) (int, []string) {
	startedCount := 0
	var errorMessages []string
	
	logging.LogDebug("Project '%s': Starting %d port forwards: %v", project.Name, len(project.Forwards), project.Forwards)
	
	for _, forwardID := range project.Forwards {
		logging.LogDebug("Project '%s': Processing forward ID '%s'", project.Name, forwardID)
		
		// Get the config index for this forward ID
		index, found := m.configStore.GetIndexByID(forwardID)
		if !found {
			errorMsg := fmt.Sprintf("Port forward ID '%s' not found", forwardID)
			errorMessages = append(errorMessages, errorMsg)
			logging.LogError("Project '%s': %s", project.Name, errorMsg)
			continue
		}
		logging.LogDebug("Project '%s': Found forward ID '%s' at index %d", project.Name, forwardID, index)
		
		// Check if already running
		if m.portForwarder.IsRunning(index) {
			logging.LogDebug("Project '%s': Forward '%s' (index %d) is already running, skipping", project.Name, forwardID, index)
			startedCount++
			continue
		}
		
		// Get the config for starting the port forward
		cfg, err := m.configStore.GetWithError(index)
		if err != nil {
			errorMsg := fmt.Sprintf("Failed to get config for '%s': %v", forwardID, err)
			errorMessages = append(errorMessages, errorMsg)
			logging.LogError("Project '%s': %s", project.Name, errorMsg)
			continue
		}
		logging.LogDebug("Project '%s': Retrieved config for '%s' (index %d): %s:%d -> %s:%d", project.Name, forwardID, index, cfg.Context, cfg.PortLocal, cfg.Service, cfg.PortRemote)
		
		// Start the port forward
		logging.LogDebug("Project '%s': Attempting to start '%s' (index %d)", project.Name, forwardID, index)
		err = m.portForwarder.Start(index, cfg)
		if err != nil {
			errorMsg := fmt.Sprintf("Failed to start '%s': %v", forwardID, err)
			errorMessages = append(errorMessages, errorMsg)
			logging.LogError("Project '%s': %s", project.Name, errorMsg)
		} else {
			startedCount++
			logging.LogDebug("Project '%s': Successfully started port forward '%s' (index %d)", project.Name, forwardID, index)
			
			// Verify it's actually running
			if m.portForwarder.IsRunning(index) {
				logging.LogDebug("Project '%s': Verified '%s' (index %d) is running", project.Name, forwardID, index)
			} else {
				logging.LogError("Project '%s': Started '%s' (index %d) but IsRunning() returned false!", project.Name, forwardID, index)
			}
		}
	}
	
	logging.LogDebug("Project '%s': Finished starting port forwards. Started %d/%d successfully", project.Name, startedCount, len(project.Forwards))
	return startedCount, errorMessages
}
