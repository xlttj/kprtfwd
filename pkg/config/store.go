package config

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"kprtfwd/pkg/logging"

	"gopkg.in/yaml.v3"
)

// Sentinel error for config not found at index
var ErrConfigNotFound = errors.New("configuration not found at the specified index")

// ConfigStore manages the collection of PortForwardConfig and Projects
type ConfigStore struct {
	configs         []PortForwardConfig
	previousConfigs []PortForwardConfig // For reload comparison
	projects        []Project
	previousProjects []Project    // For reload comparison
	activeProject   *Project     // Only one active project at a time
	mutex           sync.RWMutex // For thread-safe access
	filePath        string
}

// expandHomeDir replaces the leading ~ with the user's home directory
func expandHomeDir(path string) (string, error) {
	if !strings.HasPrefix(path, "~") {
		return path, nil
	}

	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("failed to get user home directory: %w", err)
	}

	// Replace the ~ with the home directory
	path = filepath.Join(home, path[1:])
	return path, nil
}

// ensureConfigDir ensures the config directory exists
func ensureConfigDir(configPath string) error {
	// Get the directory part of the path
	dirPath := filepath.Dir(configPath)

	// Create the directory with all necessary parent directories
	if err := os.MkdirAll(dirPath, 0755); err != nil {
		return fmt.Errorf("failed to create config directory: %w", err)
	}

	return nil
}

// NewConfigStore loads configuration and returns a new ConfigStore
func NewConfigStore() (*ConfigStore, error) {
	expandedPath, err := expandHomeDir(ConfigFilePath)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve config path: %w", err)
	}

	store := &ConfigStore{filePath: expandedPath}
	err = store.Load()
	if err != nil {
		return nil, fmt.Errorf("failed to initialize config store: %w", err)
	}
	return store, nil
}

// Load reads the configuration from the file
func (cs *ConfigStore) Load() error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	return cs.loadFromDisk()
}

// GetAll returns a copy of all port forward configurations
func (cs *ConfigStore) GetAll() []PortForwardConfig {
	cs.mutex.RLock() // Use read lock
	defer cs.mutex.RUnlock()

	// Return a copy to prevent external modification of the internal slice
	configsCopy := make([]PortForwardConfig, len(cs.configs))
	copy(configsCopy, cs.configs)
	return configsCopy
}

// Len returns the number of configurations
func (cs *ConfigStore) Len() int {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()
	return len(cs.configs)
}

// Get returns the configuration at a specific index
func (cs *ConfigStore) Get(index int) (PortForwardConfig, bool) {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()
	if index < 0 || index >= len(cs.configs) {
		return PortForwardConfig{}, false // Keep returning bool for simple existence check
	}
	return cs.configs[index], true
}

// GetWithError is similar to Get but returns an error for better context
func (cs *ConfigStore) GetWithError(index int) (PortForwardConfig, error) {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()
	if index < 0 || index >= len(cs.configs) {
		return PortForwardConfig{}, fmt.Errorf("%w: index %d out of bounds (length %d)", ErrConfigNotFound, index, len(cs.configs))
	}
	return cs.configs[index], nil
}


// Reload re-reads the configuration from disk
func (cs *ConfigStore) Reload() error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	// Store previous configs and projects for comparison
	cs.previousConfigs = make([]PortForwardConfig, len(cs.configs))
	copy(cs.previousConfigs, cs.configs)
	cs.previousProjects = make([]Project, len(cs.projects))
	copy(cs.previousProjects, cs.projects)

	// Store active project name (it might be cleared if project no longer exists)
	var activeProjectName string
	if cs.activeProject != nil {
		activeProjectName = cs.activeProject.Name
	}

	// Re-read from disk using the same logic as Load()
	err := cs.loadFromDisk()
	if err != nil {
		// Restore previous configs and projects on error
		cs.configs = cs.previousConfigs
		cs.projects = cs.previousProjects
		cs.previousConfigs = nil
		cs.previousProjects = nil
		return fmt.Errorf("config reload failed, kept previous config: %w", err)
	}

	// Try to restore active project if it still exists
	if activeProjectName != "" {
		err := cs.setActiveProjectUnsafe(activeProjectName) // internal method without mutex
		if err != nil {
			logging.LogDebug("Active project '%s' no longer exists after reload, cleared", activeProjectName)
			cs.activeProject = nil
		}
	}

	logging.LogDebug("Configuration reloaded: %d configs (was %d), %d projects (was %d)", 
		len(cs.configs), len(cs.previousConfigs), len(cs.projects), len(cs.previousProjects))
	return nil
}

// GetPrevious returns the previously loaded configuration for comparison
func (cs *ConfigStore) GetPrevious() []PortForwardConfig {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	if cs.previousConfigs == nil {
		return nil
	}

	// Return a copy to prevent external modification
	previousCopy := make([]PortForwardConfig, len(cs.previousConfigs))
	copy(previousCopy, cs.previousConfigs)
	return previousCopy
}

// loadFromDisk performs the actual file reading and parsing
// This is the core logic extracted from Load() for reuse in Reload()
func (cs *ConfigStore) loadFromDisk() error {
	_, err := os.Stat(cs.filePath)
	if os.IsNotExist(err) {
		logging.LogDebug("Config file %s does not exist, creating empty config directory", cs.filePath)
		// Ensure the config directory exists
		if err := ensureConfigDir(cs.filePath); err != nil {
			return fmt.Errorf("failed to prepare config directory: %w", err)
		}

		// Initialize with empty config
		cs.configs = []PortForwardConfig{} // Start with empty slice
		logging.LogDebug("Initialized with empty configuration, file must be created manually")
		return nil
	} else if err != nil {
		// Other stat error (e.g., permission denied)
		return fmt.Errorf("failed to stat config file %s: %w", cs.filePath, err)
	}

	data, err := os.ReadFile(cs.filePath)
	if err != nil {
		return fmt.Errorf("failed to read config file %s: %w", cs.filePath, err)
	}

	// Use gopkg.in/yaml.v3 for unmarshaling
	var cfgFile ConfigFile
	err = yaml.Unmarshal(data, &cfgFile)
	if err != nil {
		return fmt.Errorf("failed to unmarshal config file %s: %w", cs.filePath, err)
	}

	// Handle backward compatibility: use old format if new format is empty
	if len(cfgFile.PortForwards) == 0 && len(cfgFile.PortForwardsOld) > 0 {
		logging.LogDebug("Loading from legacy 'portforwards' field for backward compatibility")
		cs.configs = cfgFile.PortForwardsOld
	} else {
		cs.configs = cfgFile.PortForwards
	}
	cs.projects = cfgFile.Projects

	// Check for missing IDs and provide helpful error message
	for i, cfg := range cs.configs {
		if cfg.ID == "" {
			return fmt.Errorf("\n\nCONFIG MIGRATION REQUIRED:\n\nYour configuration file uses the old format without 'id' fields.\n\nPort forward at index %d is missing an 'id' field.\n\nPlease update your config file to the new format:\n\nOLD FORMAT:\nportforwards:\n  - context: staging\n    namespace: mysql\n    service: mysql-svc\n    port_remote: 3306\n    port_local: 3306\n\nNEW FORMAT:\nport_forwards:\n  - id: \"mysql-staging\"\n    context: staging\n    namespace: mysql\n    service: mysql-svc\n    port_remote: 3306\n    port_local: 3306\n\nprojects:\n  - name: \"database-dev\"\n    forwards: [\"mysql-staging\"]\n\nConfig file location: %s", i, cs.filePath)
		}
	}

	// Validate configuration after loading
	err = cs.validateConfig()
	if err != nil {
		return fmt.Errorf("config validation failed: %w", err)
	}

	logging.LogDebug("Loaded %d configurations and %d projects from %s", len(cs.configs), len(cs.projects), cs.filePath)
	return nil
}

// validateConfig validates the loaded configuration for logical consistency
// Must be called with mutex already locked
func (cs *ConfigStore) validateConfig() error {
	// Validate port forward IDs are unique and non-empty
	idMap := make(map[string]bool)
	for i, cfg := range cs.configs {
		if cfg.ID == "" {
			return fmt.Errorf("port forward at index %d has empty ID", i)
		}
		if strings.TrimSpace(cfg.ID) != cfg.ID {
			return fmt.Errorf("port forward ID '%s' contains leading/trailing whitespace", cfg.ID)
		}
		if idMap[cfg.ID] {
			return fmt.Errorf("duplicate port forward ID: '%s'", cfg.ID)
		}
		idMap[cfg.ID] = true
	}

	// Validate project names are unique and non-empty
	projectMap := make(map[string]bool)
	for i, project := range cs.projects {
		if project.Name == "" {
			return fmt.Errorf("project at index %d has empty name", i)
		}
		if strings.TrimSpace(project.Name) != project.Name {
			return fmt.Errorf("project name '%s' contains leading/trailing whitespace", project.Name)
		}
		if projectMap[project.Name] {
			return fmt.Errorf("duplicate project name: '%s'", project.Name)
		}
		projectMap[project.Name] = true

		// Validate that all forward IDs referenced by projects exist
		for _, forwardID := range project.Forwards {
			if !idMap[forwardID] {
				return fmt.Errorf("project '%s' references non-existent port forward ID: '%s'", project.Name, forwardID)
			}
		}
	}

	return nil
}

// GetProjects returns a copy of all projects
func (cs *ConfigStore) GetProjects() []Project {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	projectsCopy := make([]Project, len(cs.projects))
	copy(projectsCopy, cs.projects)
	return projectsCopy
}

// GetActiveProject returns the currently active project (or nil if none)
func (cs *ConfigStore) GetActiveProject() *Project {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	return cs.activeProject
}

// SetActiveProject sets the active project by name
func (cs *ConfigStore) SetActiveProject(name string) error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	if name == "" {
		cs.activeProject = nil
		logging.LogDebug("Cleared active project")
		return nil
	}

	for _, project := range cs.projects {
		if project.Name == name {
			cs.activeProject = &project
			logging.LogDebug("Set active project to: %s", name)
			return nil
		}
	}

	return fmt.Errorf("project not found: %s", name)
}

// setActiveProjectUnsafe sets the active project without acquiring mutex (for internal use)
func (cs *ConfigStore) setActiveProjectUnsafe(name string) error {
	for _, project := range cs.projects {
		if project.Name == name {
			cs.activeProject = &project
			return nil
		}
	}
	return fmt.Errorf("project not found: %s", name)
}

// GetActiveProjectForwards returns port forward configs for the active project
// Returns all configs if no project is active
func (cs *ConfigStore) GetActiveProjectForwards() []PortForwardConfig {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	if cs.activeProject == nil {
		// No active project - return all configs
		configsCopy := make([]PortForwardConfig, len(cs.configs))
		copy(configsCopy, cs.configs)
		return configsCopy
	}

	// Build map of IDs to configs for quick lookup
	idToConfig := make(map[string]PortForwardConfig)
	for _, cfg := range cs.configs {
		idToConfig[cfg.ID] = cfg
	}

	// Get configs for active project forwards
	var activeConfigs []PortForwardConfig
	for _, forwardID := range cs.activeProject.Forwards {
		if cfg, exists := idToConfig[forwardID]; exists {
			activeConfigs = append(activeConfigs, cfg)
		}
	}

	return activeConfigs
}

// GetConfigByID returns the port forward configuration with the given ID
func (cs *ConfigStore) GetConfigByID(id string) (PortForwardConfig, bool) {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	for _, cfg := range cs.configs {
		if cfg.ID == id {
			return cfg, true
		}
	}
	return PortForwardConfig{}, false
}

// GetIndexByID returns the index of the port forward configuration with the given ID
func (cs *ConfigStore) GetIndexByID(id string) (int, bool) {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	for i, cfg := range cs.configs {
		if cfg.ID == id {
			return i, true
		}
	}
	return -1, false
}

// Project CRUD Operations

// CreateProject creates a new project with the given name and port forward IDs
func (cs *ConfigStore) CreateProject(name string, portForwardIDs []string) error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	// Validate project name
	name = strings.TrimSpace(name)
	if name == "" {
		return fmt.Errorf("project name cannot be empty")
	}

	// Check if project already exists
	for _, project := range cs.projects {
		if project.Name == name {
			return fmt.Errorf("project '%s' already exists", name)
		}
	}

	// Validate that all port forward IDs exist
	for _, id := range portForwardIDs {
		found := false
		for _, cfg := range cs.configs {
			if cfg.ID == id {
				found = true
				break
			}
		}
		if !found {
			return fmt.Errorf("port forward ID '%s' does not exist", id)
		}
	}

	// Create and add the new project
	newProject := Project{
		Name:     name,
		Forwards: make([]string, len(portForwardIDs)),
	}
	copy(newProject.Forwards, portForwardIDs)

	cs.projects = append(cs.projects, newProject)
	logging.LogDebug("Created project '%s' with %d port forwards", name, len(portForwardIDs))
	return nil
}

// UpdateProject updates an existing project's port forward IDs
func (cs *ConfigStore) UpdateProject(name string, portForwardIDs []string) error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	// Find the project
	projectIndex := -1
	for i, project := range cs.projects {
		if project.Name == name {
			projectIndex = i
			break
		}
	}

	if projectIndex == -1 {
		return fmt.Errorf("project '%s' does not exist", name)
	}

	// Validate that all port forward IDs exist
	for _, id := range portForwardIDs {
		found := false
		for _, cfg := range cs.configs {
			if cfg.ID == id {
				found = true
				break
			}
		}
		if !found {
			return fmt.Errorf("port forward ID '%s' does not exist", id)
		}
	}

	// Update the project
	cs.projects[projectIndex].Forwards = make([]string, len(portForwardIDs))
	copy(cs.projects[projectIndex].Forwards, portForwardIDs)
	logging.LogDebug("Updated project '%s' with %d port forwards", name, len(portForwardIDs))
	return nil
}

// DeleteProject deletes a project by name
func (cs *ConfigStore) DeleteProject(name string) error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	// Find the project
	projectIndex := -1
	for i, project := range cs.projects {
		if project.Name == name {
			projectIndex = i
			break
		}
	}

	if projectIndex == -1 {
		return fmt.Errorf("project '%s' does not exist", name)
	}

	// Clear active project if it's the one being deleted
	if cs.activeProject != nil && cs.activeProject.Name == name {
		cs.activeProject = nil
		logging.LogDebug("Cleared active project because '%s' was deleted", name)
	}

	// Remove the project from the slice
	cs.projects = append(cs.projects[:projectIndex], cs.projects[projectIndex+1:]...)
	logging.LogDebug("Deleted project '%s'", name)
	return nil
}

// GetProject returns a project by name
func (cs *ConfigStore) GetProject(name string) (*Project, bool) {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	for _, project := range cs.projects {
		if project.Name == name {
			// Return a copy to prevent external modification
			projectCopy := Project{
				Name:     project.Name,
				Forwards: make([]string, len(project.Forwards)),
			}
			copy(projectCopy.Forwards, project.Forwards)
			return &projectCopy, true
		}
	}
	return nil, false
}

// GetAllProjects returns all projects (alias for GetProjects for consistency)
func (cs *ConfigStore) GetAllProjects() []Project {
	return cs.GetProjects()
}

// ClearActiveProject clears the currently active project
func (cs *ConfigStore) ClearActiveProject() {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	cs.activeProject = nil
	logging.LogDebug("Cleared active project")
}

// GetActiveProjectName returns the name of the active project (empty string if none)
func (cs *ConfigStore) GetActiveProjectName() string {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	if cs.activeProject == nil {
		return ""
	}
	return cs.activeProject.Name
}

// Cascading cleanup methods

// RemovePortForwardFromProjects removes a port forward ID from all projects that reference it
// This should be called when a port forward is deleted to maintain data integrity
func (cs *ConfigStore) RemovePortForwardFromProjects(portForwardID string) {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	for i := range cs.projects {
		// Remove the port forward ID from the project's forwards list
		newForwards := make([]string, 0, len(cs.projects[i].Forwards))
		for _, forwardID := range cs.projects[i].Forwards {
			if forwardID != portForwardID {
				newForwards = append(newForwards, forwardID)
			}
		}
		
		// Update the project's forwards list
		if len(newForwards) != len(cs.projects[i].Forwards) {
			cs.projects[i].Forwards = newForwards
			logging.LogDebug("Removed port forward ID '%s' from project '%s'", portForwardID, cs.projects[i].Name)
		}
	}

	// Clear active project if it no longer has any forwards
	if cs.activeProject != nil && len(cs.activeProject.Forwards) == 0 {
		logging.LogDebug("Cleared active project '%s' because it has no more port forwards", cs.activeProject.Name)
		cs.activeProject = nil
	}
}

// CleanupEmptyProjects removes projects that have no port forwards
func (cs *ConfigStore) CleanupEmptyProjects() int {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	var keptProjects []Project
	removedCount := 0
	
	for _, project := range cs.projects {
		if len(project.Forwards) > 0 {
			keptProjects = append(keptProjects, project)
		} else {
			removedCount++
			logging.LogDebug("Removed empty project '%s'", project.Name)
			
			// Clear active project if it was the empty one being removed
			if cs.activeProject != nil && cs.activeProject.Name == project.Name {
				cs.activeProject = nil
				logging.LogDebug("Cleared active project because '%s' was empty", project.Name)
			}
		}
	}
	
	cs.projects = keptProjects
	return removedCount
}

// ValidateProjectIntegrity checks and fixes project integrity issues
// Returns a list of issues found and fixed
func (cs *ConfigStore) ValidateProjectIntegrity() []string {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	var issues []string
	
	// Build set of valid port forward IDs
	validIDs := make(map[string]bool)
	for _, cfg := range cs.configs {
		validIDs[cfg.ID] = true
	}
	
	// Check each project for invalid references
	for i := range cs.projects {
		project := &cs.projects[i]
		var validForwards []string
		
		for _, forwardID := range project.Forwards {
			if validIDs[forwardID] {
				validForwards = append(validForwards, forwardID)
			} else {
				issues = append(issues, fmt.Sprintf("Removed invalid port forward ID '%s' from project '%s'", forwardID, project.Name))
			}
		}
		
		project.Forwards = validForwards
	}
	
	// Clear active project if it references invalid forwards
	if cs.activeProject != nil {
		var validActiveForwards []string
		for _, forwardID := range cs.activeProject.Forwards {
			if validIDs[forwardID] {
				validActiveForwards = append(validActiveForwards, forwardID)
			}
		}
		
		if len(validActiveForwards) != len(cs.activeProject.Forwards) {
			issues = append(issues, fmt.Sprintf("Active project '%s' had invalid references, cleared", cs.activeProject.Name))
			cs.activeProject = nil
		}
	}
	
	return issues
}
