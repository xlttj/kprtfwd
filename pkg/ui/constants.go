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
	ActionPortForwardNav  = "↑/↓: Navigate | space: Toggle/Expand | e: Edit Port | g: Toggle Grouping | ctrl+d: Discover | ctrl+p: Projects | ctrl+r: Restart | q: Quit"
	ActionProjectSelector = "↑/↓: Navigate | Enter: Select Project | M: Manage Projects | Esc: Back"
	ActionExit            = "ctrl+x: Exit"
)

// Keyboard shortcuts
const (
	ShortcutExit            = "ctrl+x"
	ShortcutRestartForwards = "ctrl+r"
	ShortcutProjects        = "ctrl+p"
	ShortcutDiscovery       = "ctrl+d"
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
	StatusError   = "Error  " // padded to same width as "Running"/"Stopped" to keep column alignment
)

// ASCII Visual Indicators - Compatible across all terminals
const (
	// Checkbox symbols
	CheckboxUnchecked = "[ ]"
	CheckboxChecked   = "[X]"

	// Selection indicators
	IndicatorUnselected = "( )"
	IndicatorSelected   = "(*)"

	// Group expansion indicators
	ExpanderCollapsed = "[-]"
	ExpanderExpanded  = "[+]"
)

// Lipgloss Colors
const (
	ColorBorder     = "240"
	ColorSelectedFg = "229"
	ColorSelectedBg = "57"
	ColorTitle      = "14"  // Cyan for titles
	ColorHelp       = "245" // Grey for help text
	ColorError      = "9"   // Red for errors

	// Status column colors
	ColorStatusRunning = "2"   // Green
	ColorStatusStopped = "240" // Dim grey
	ColorStatusError   = "9"   // Red
)
