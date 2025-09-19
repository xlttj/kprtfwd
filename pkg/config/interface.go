package config

import "errors"

// Sentinel error for config not found at index
var ErrConfigNotFound = errors.New("configuration not found at the specified index")

// ConfigStoreInterface defines the interface for configuration storage
type ConfigStoreInterface interface {
	// Port Forward Operations
	Add(cfg PortForwardConfig) error
	GetAll() []PortForwardConfig
	Len() int
	Get(index int) (PortForwardConfig, bool)
	GetWithError(index int) (PortForwardConfig, error)
	GetConfigByID(id string) (PortForwardConfig, bool)
	GetIndexByID(id string) (int, bool)

	// Project Operations
	CreateProject(name string, portForwardIDs []string) error
	GetProjects() []Project
	GetAllProjects() []Project
	DeleteProject(name string) error

	// Active Project Management (in-memory state)
	SetActiveProject(name string) error
	GetActiveProject() *Project
	ClearActiveProject()
	GetActiveProjectName() string
	GetActiveProjectForwards() []PortForwardConfig

	// Compatibility methods
	Load() error
	Save() error
}

// NewConfigStore creates a new config store (defaults to SQLite)
func NewConfigStore() (ConfigStoreInterface, error) {
	return NewSQLiteConfigStore()
}
