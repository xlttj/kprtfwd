package config

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"sync"

	"github.com/xlttj/kprtfwd/pkg/logging"

	_ "modernc.org/sqlite"
)

// SQLiteConfigStore manages the collection of PortForwardConfig and Projects using SQLite
type SQLiteConfigStore struct {
	db            *sql.DB
	activeProject *Project     // In-memory state only
	mutex         sync.RWMutex // For thread-safe access
	dbPath        string
}

// NewSQLiteConfigStore creates and initializes a new SQLite-based config store
func NewSQLiteConfigStore() (*SQLiteConfigStore, error) {
	// Determine database path
	homeDir, err := os.UserHomeDir()
	if err != nil {
		return nil, fmt.Errorf("failed to get user home directory: %w", err)
	}

	configDir := filepath.Join(homeDir, ".kprtfwd")
	dbPath := filepath.Join(configDir, "kprtfwd.db")

	// Ensure config directory exists
	if err := os.MkdirAll(configDir, 0700); err != nil {
		return nil, fmt.Errorf("failed to create config directory: %w", err)
	}

	// Open SQLite database
	db, err := sql.Open("sqlite", dbPath)
	// Attempt to set restrictive permissions on first creation
	if _, statErr := os.Stat(dbPath); os.IsNotExist(statErr) {
		// Create empty file with 0600, then reopen via sql if needed
		f, ferr := os.OpenFile(dbPath, os.O_CREATE|os.O_RDONLY, 0600)
		if ferr == nil {
			_ = f.Close()
		}
	}
	if err != nil {
		return nil, fmt.Errorf("failed to open database: %w", err)
	}

	// Test connection
	if err := db.Ping(); err != nil {
		db.Close()
		return nil, fmt.Errorf("failed to ping database: %w", err)
	}

	store := &SQLiteConfigStore{
		db:     db,
		dbPath: dbPath,
	}

	// Initialize schema
	if err := store.initializeSchema(); err != nil {
		db.Close()
		return nil, fmt.Errorf("failed to initialize database schema: %w", err)
	}

	logging.LogDebug("SQLite config store initialized at: %s", dbPath)
	return store, nil
}

// initializeSchema creates the database tables and indexes
func (cs *SQLiteConfigStore) initializeSchema() error {
	schema := `
	-- Port forward configurations
	CREATE TABLE IF NOT EXISTS port_forwards (
		id TEXT PRIMARY KEY,
		context TEXT NOT NULL,
		namespace TEXT NOT NULL,
		service TEXT NOT NULL,
		port_remote INTEGER NOT NULL,
		port_local INTEGER NOT NULL
	);

	-- Projects for grouping
	CREATE TABLE IF NOT EXISTS projects (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		name TEXT UNIQUE NOT NULL
	);

	-- Many-to-many relationship
	CREATE TABLE IF NOT EXISTS project_port_forwards (
		project_id INTEGER,
		port_forward_id TEXT,
		FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
		FOREIGN KEY (port_forward_id) REFERENCES port_forwards(id) ON DELETE CASCADE,
		PRIMARY KEY (project_id, port_forward_id)
	);

	-- Indexes for performance
	CREATE INDEX IF NOT EXISTS idx_port_forwards_context ON port_forwards(context);
	CREATE INDEX IF NOT EXISTS idx_port_forwards_namespace ON port_forwards(namespace);
	CREATE INDEX IF NOT EXISTS idx_port_forwards_service ON port_forwards(service);
	`

	_, err := cs.db.Exec(schema)
	if err != nil {
		return fmt.Errorf("failed to execute schema: %w", err)
	}

	return nil
}

// Close closes the database connection
func (cs *SQLiteConfigStore) Close() error {
	if cs.db != nil {
		return cs.db.Close()
	}
	return nil
}

// Port Forward Operations

// Add adds a new port forward configuration
func (cs *SQLiteConfigStore) Add(cfg PortForwardConfig) error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	query := `
		INSERT INTO port_forwards (id, context, namespace, service, port_remote, port_local)
		VALUES (?, ?, ?, ?, ?, ?)
	`

	_, err := cs.db.Exec(query, cfg.ID, cfg.Context, cfg.Namespace, cfg.Service, cfg.PortRemote, cfg.PortLocal)
	if err != nil {
		return fmt.Errorf("failed to add port forward: %w", err)
	}

	logging.LogDebug("Added port forward: %s", cfg.ID)
	return nil
}

// GetAll returns all port forward configurations
func (cs *SQLiteConfigStore) GetAll() []PortForwardConfig {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	query := `SELECT id, context, namespace, service, port_remote, port_local FROM port_forwards ORDER BY context, namespace, service`

	rows, err := cs.db.Query(query)
	if err != nil {
		logging.LogError("Failed to query port forwards: %v", err)
		return []PortForwardConfig{}
	}
	defer rows.Close()

	var configs []PortForwardConfig
	for rows.Next() {
		var cfg PortForwardConfig
		err := rows.Scan(&cfg.ID, &cfg.Context, &cfg.Namespace, &cfg.Service, &cfg.PortRemote, &cfg.PortLocal)
		if err != nil {
			logging.LogError("Failed to scan port forward row: %v", err)
			continue
		}
		configs = append(configs, cfg)
	}

	return configs
}

// Len returns the number of port forward configurations
func (cs *SQLiteConfigStore) Len() int {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	var count int
	err := cs.db.QueryRow("SELECT COUNT(*) FROM port_forwards").Scan(&count)
	if err != nil {
		logging.LogError("Failed to count port forwards: %v", err)
		return 0
	}

	return count
}

// Get returns the configuration at a specific index (for compatibility)
func (cs *SQLiteConfigStore) Get(index int) (PortForwardConfig, bool) {
	configs := cs.GetAll()
	if index < 0 || index >= len(configs) {
		return PortForwardConfig{}, false
	}
	return configs[index], true
}

// GetWithError returns the configuration at a specific index with error context
func (cs *SQLiteConfigStore) GetWithError(index int) (PortForwardConfig, error) {
	configs := cs.GetAll()
	if index < 0 || index >= len(configs) {
		return PortForwardConfig{}, fmt.Errorf("%w: index %d out of bounds (length %d)", ErrConfigNotFound, index, len(configs))
	}
	return configs[index], nil
}

// GetConfigByID returns the port forward configuration with the given ID
func (cs *SQLiteConfigStore) GetConfigByID(id string) (PortForwardConfig, bool) {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	query := `SELECT id, context, namespace, service, port_remote, port_local FROM port_forwards WHERE id = ?`

	var cfg PortForwardConfig
	err := cs.db.QueryRow(query, id).Scan(&cfg.ID, &cfg.Context, &cfg.Namespace, &cfg.Service, &cfg.PortRemote, &cfg.PortLocal)
	if err != nil {
		if err == sql.ErrNoRows {
			return PortForwardConfig{}, false
		}
		logging.LogError("Failed to query port forward by ID: %v", err)
		return PortForwardConfig{}, false
	}

	return cfg, true
}

// GetIndexByID returns the index of the port forward configuration with the given ID
func (cs *SQLiteConfigStore) GetIndexByID(id string) (int, bool) {
	configs := cs.GetAll()
	for i, cfg := range configs {
		if cfg.ID == id {
			return i, true
		}
	}
	return -1, false
}

// DeletePortForward removes a port forward configuration by ID
func (cs *SQLiteConfigStore) DeletePortForward(id string) error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	// Start transaction
	tx, err := cs.db.Begin()
	if err != nil {
		return fmt.Errorf("failed to start transaction: %w", err)
	}
	defer tx.Rollback()

	// Remove from project associations
	_, err = tx.Exec("DELETE FROM project_port_forwards WHERE port_forward_id = ?", id)
	if err != nil {
		return fmt.Errorf("failed to remove project associations: %w", err)
	}

	// Remove port forward
	result, err := tx.Exec("DELETE FROM port_forwards WHERE id = ?", id)
	if err != nil {
		return fmt.Errorf("failed to delete port forward: %w", err)
	}

	rowsAffected, err := result.RowsAffected()
	if err != nil {
		return fmt.Errorf("failed to get affected rows: %w", err)
	}

	if rowsAffected == 0 {
		return fmt.Errorf("port forward with ID '%s' not found", id)
	}

	// Commit transaction
	if err := tx.Commit(); err != nil {
		return fmt.Errorf("failed to commit transaction: %w", err)
	}

	logging.LogDebug("Deleted port forward: %s", id)
	return nil
}

// Project Operations

// CreateProject creates a new project
func (cs *SQLiteConfigStore) CreateProject(name string, portForwardIDs []string) error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	// Start transaction
	tx, err := cs.db.Begin()
	if err != nil {
		return fmt.Errorf("failed to start transaction: %w", err)
	}
	defer tx.Rollback()

	// Insert project
	result, err := tx.Exec("INSERT INTO projects (name) VALUES (?)", name)
	if err != nil {
		return fmt.Errorf("failed to create project: %w", err)
	}

	projectID, err := result.LastInsertId()
	if err != nil {
		return fmt.Errorf("failed to get project ID: %w", err)
	}

	// Add port forward associations
	for _, pfID := range portForwardIDs {
		_, err = tx.Exec("INSERT INTO project_port_forwards (project_id, port_forward_id) VALUES (?, ?)", projectID, pfID)
		if err != nil {
			return fmt.Errorf("failed to add port forward to project: %w", err)
		}
	}

	// Commit transaction
	if err := tx.Commit(); err != nil {
		return fmt.Errorf("failed to commit transaction: %w", err)
	}

	logging.LogDebug("Created project: %s with %d port forwards", name, len(portForwardIDs))
	return nil
}

// GetProjects returns all projects with their associated port forwards
func (cs *SQLiteConfigStore) GetProjects() []Project {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	query := `SELECT id, name FROM projects ORDER BY name`

	rows, err := cs.db.Query(query)
	if err != nil {
		logging.LogError("Failed to query projects: %v", err)
		return []Project{}
	}
	defer rows.Close()

	var projects []Project
	for rows.Next() {
		var project Project
		var id int64
		if err := rows.Scan(&id, &project.Name); err != nil {
			logging.LogError("Failed to scan project row: %v", err)
			continue
		}

		// Get associated port forward IDs
		pfQuery := `SELECT port_forward_id FROM project_port_forwards WHERE project_id = ?`
		pfRows, err := cs.db.Query(pfQuery, id)
		if err != nil {
			logging.LogError("Failed to query project port forwards: %v", err)
			continue
		}

		var forwards []string
		for pfRows.Next() {
			var pfID string
			if err := pfRows.Scan(&pfID); err != nil {
				logging.LogError("Failed to scan port forward ID: %v", err)
				continue
			}
			forwards = append(forwards, pfID)
		}
		pfRows.Close()

		project.Forwards = forwards
		projects = append(projects, project)
	}

	return projects
}

// GetAllProjects returns all projects (alias for compatibility)
func (cs *SQLiteConfigStore) GetAllProjects() []Project {
	return cs.GetProjects()
}

// DeleteProject deletes a project by name
func (cs *SQLiteConfigStore) DeleteProject(name string) error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	// Clear active project if it's being deleted
	if cs.activeProject != nil && cs.activeProject.Name == name {
		cs.activeProject = nil
		logging.LogDebug("Cleared active project because '%s' was deleted", name)
	}

	result, err := cs.db.Exec("DELETE FROM projects WHERE name = ?", name)
	if err != nil {
		return fmt.Errorf("failed to delete project: %w", err)
	}

	rowsAffected, err := result.RowsAffected()
	if err != nil {
		return fmt.Errorf("failed to get affected rows: %w", err)
	}

	if rowsAffected == 0 {
		return fmt.Errorf("project '%s' does not exist", name)
	}

	logging.LogDebug("Deleted project: %s", name)
	return nil
}

// In-Memory State Management

// SetActiveProject sets the active project by name (in-memory only)
func (cs *SQLiteConfigStore) SetActiveProject(name string) error {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	if name == "" {
		cs.activeProject = nil
		logging.LogDebug("Cleared active project")
		return nil
	}

	// Find project
	projects := cs.getProjectsUnsafe()
	for i := range projects {
		p := projects[i]
		if p.Name == name {
			copyProj := Project{Name: p.Name, Forwards: append([]string{}, p.Forwards...)}
			cs.activeProject = &copyProj
			logging.LogDebug("Set active project to: %s", name)
			return nil
		}
	}

	return fmt.Errorf("project not found: %s", name)
}

// GetActiveProject returns the currently active project (in-memory only)
func (cs *SQLiteConfigStore) GetActiveProject() *Project {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	return cs.activeProject
}

// ClearActiveProject clears the currently active project (in-memory only)
func (cs *SQLiteConfigStore) ClearActiveProject() {
	cs.mutex.Lock()
	defer cs.mutex.Unlock()

	cs.activeProject = nil
	logging.LogDebug("Cleared active project")
}

// GetActiveProjectName returns the name of the active project (empty string if none)
func (cs *SQLiteConfigStore) GetActiveProjectName() string {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	if cs.activeProject == nil {
		return ""
	}
	return cs.activeProject.Name
}

// GetActiveProjectForwards returns port forward configs for the active project
func (cs *SQLiteConfigStore) GetActiveProjectForwards() []PortForwardConfig {
	cs.mutex.RLock()
	defer cs.mutex.RUnlock()

	if cs.activeProject == nil {
		// No active project - return all configs
		return cs.getAllUnsafe()
	}

	// Get configs for active project forwards
	var configs []PortForwardConfig
	for _, forwardID := range cs.activeProject.Forwards {
		if cfg, exists := cs.getConfigByIDUnsafe(forwardID); exists {
			configs = append(configs, cfg)
		}
	}

	return configs
}

// Helper methods (must be called with mutex already held)

func (cs *SQLiteConfigStore) getAllUnsafe() []PortForwardConfig {
	query := `SELECT id, context, namespace, service, port_remote, port_local FROM port_forwards ORDER BY context, namespace, service`

	rows, err := cs.db.Query(query)
	if err != nil {
		logging.LogError("Failed to query port forwards: %v", err)
		return []PortForwardConfig{}
	}
	defer rows.Close()

	var configs []PortForwardConfig
	for rows.Next() {
		var cfg PortForwardConfig
		err := rows.Scan(&cfg.ID, &cfg.Context, &cfg.Namespace, &cfg.Service, &cfg.PortRemote, &cfg.PortLocal)
		if err != nil {
			logging.LogError("Failed to scan port forward row: %v", err)
			continue
		}
		configs = append(configs, cfg)
	}

	return configs
}

func (cs *SQLiteConfigStore) getConfigByIDUnsafe(id string) (PortForwardConfig, bool) {
	query := `SELECT id, context, namespace, service, port_remote, port_local FROM port_forwards WHERE id = ?`

	var cfg PortForwardConfig
	err := cs.db.QueryRow(query, id).Scan(&cfg.ID, &cfg.Context, &cfg.Namespace, &cfg.Service, &cfg.PortRemote, &cfg.PortLocal)
	if err != nil {
		if err == sql.ErrNoRows {
			return PortForwardConfig{}, false
		}
		logging.LogError("Failed to query port forward by ID: %v", err)
		return PortForwardConfig{}, false
	}

	return cfg, true
}

func (cs *SQLiteConfigStore) getProjectsUnsafe() []Project {
	query := `SELECT id, name FROM projects ORDER BY name`

	rows, err := cs.db.Query(query)
	if err != nil {
		logging.LogError("Failed to query projects: %v", err)
		return []Project{}
	}
	defer rows.Close()

	var projects []Project
	for rows.Next() {
		var project Project
		var id int64
		err := rows.Scan(&id, &project.Name)
		if err != nil {
			logging.LogError("Failed to scan project row: %v", err)
			continue
		}

		// Get associated port forward IDs
		pfQuery := `SELECT port_forward_id FROM project_port_forwards WHERE project_id = ?`
		pfRows, err := cs.db.Query(pfQuery, id)
		if err != nil {
			logging.LogError("Failed to query project port forwards: %v", err)
			continue
		}

		var forwards []string
		for pfRows.Next() {
			var pfID string
			err := pfRows.Scan(&pfID)
			if err != nil {
				logging.LogError("Failed to scan port forward ID: %v", err)
				continue
			}
			forwards = append(forwards, pfID)
		}
		pfRows.Close()

		project.Forwards = forwards
		projects = append(projects, project)
	}

	return projects
}

// Compatibility methods for existing interface

// Load is a no-op for SQLite (database is always "loaded")
func (cs *SQLiteConfigStore) Load() error {
	return nil
}

// Save is a no-op for SQLite (changes are immediately persisted)
func (cs *SQLiteConfigStore) Save() error {
	return nil
}
