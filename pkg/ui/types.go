package ui

// UIState represents the different views/states of the UI
type UIState int

const (
	StatePortForwards            UIState = iota // Port forwards table view
	StateProjectSelector                        // Project selection view (Ctrl+P)
	StateServiceDiscovery                       // Service discovery view (Ctrl+D)
	StateProjectManagement                      // Project management view
	StateProjectCreation                        // Project creation form
	StateProjectServiceSelection                // Add/remove services to/from project
)

// GroupState represents whether a group is expanded or collapsed
type GroupState struct {
	Expanded bool
	Count    int // Total items in group
	Active   int // Active items in group
}

// RowType represents the type of row in the table
type RowType int

const (
	RowTypeGroup RowType = iota
	RowTypeItem
)

// TableRow represents a row with metadata
type TableRow struct {
	Type        RowType
	ConfigIndex int    // Original config index, -1 for group headers
	GroupName   string // Group name for group headers
	Data        []string
}

// DiscoveryPhase represents the current phase of service discovery
type DiscoveryPhase int

const (
	PhaseClusterSelection DiscoveryPhase = iota
	PhaseServiceSelection
)

// ServiceSelection represents a service with selection state and customizable local port
type ServiceSelection struct {
	Service   DiscoveredServiceWithPorts
	Selected  bool
	LocalPort int
}

// PortSelection represents an individual port selection within a service
type PortSelection struct {
	ServiceName         string
	ServiceNamespace    string
	ServiceType         string
	ServiceLabels       map[string]string
	Port                ServicePortInfo
	Selected            bool
	LocalPort           int
	GeneratedID         string
	ExistingConfigIndex int // Index in config store if port already exists, -1 if new
}

// DiscoveredServiceWithPorts wraps discovery.DiscoveredService with additional UI state
type DiscoveredServiceWithPorts struct {
	Name      string
	Namespace string
	Ports     []ServicePortInfo
	Labels    map[string]string
	Type      string
}

// ServicePortInfo represents a port with UI-specific information
type ServicePortInfo struct {
	Name       string
	Port       int32
	TargetPort string
	Protocol   string
}
