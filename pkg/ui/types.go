package ui

// UIState represents the different views/states of the UI
type UIState int

const (
	StatePortForwards UIState = iota // Port forwards table view
	StateProjectSelector             // Project selection view (Ctrl+P)
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
