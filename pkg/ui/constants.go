package ui

// Table Column Titles
const (
	ColContext    = "CONTEXT"
	ColNamespace  = "NAMESPACE"
	ColService    = "SERVICE"
	ColPortRemote = "REMOTE"
	ColPortLocal  = "LOCAL"
	ColStatus     = "STATUS"
)

// Action Lines / Key Hints
const (
	ActionPortForwardNav = "↑/↓: Navigate | space: Toggle/Expand | g: Toggle Grouping | ctrl+p: Projects | ctrl+r: Reload | q: Quit"
	ActionProjectSelector = "↑/↓: Navigate | enter: Select Project | esc: Back"
	ActionExit            = "ctrl+x: Exit"
)

// Keyboard shortcuts
const (
	ShortcutExit           = "ctrl+x"
	ShortcutReloadConfig   = "ctrl+r"
	ShortcutProjects       = "ctrl+p"
)

// Numeric Constants for Layout/Indexing
const (
	HeaderHeightEstimate   = 3 // Estimated lines used by the header section
	MinTableHeight         = 4 // Minimum height for tables after calculation
	PortForwardsViewOffset = 8 // Estimated non-table lines in PortForwards view for height calc (including filter line)
)

// Status Strings - these are display-only, not stored in config
const (
	StatusStopped = "Stopped"
	StatusRunning = "Running"
)

// Lipgloss Colors
const (
	ColorBorder     = "240"
	ColorSelectedFg = "229"
	ColorSelectedBg = "57"
	ColorTitle      = "14"  // Cyan for titles
	ColorHelp       = "245" // Grey for help text
	ColorError      = "9"   // Red for errors
)
