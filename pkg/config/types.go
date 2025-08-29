package config

// PortForwardConfig represents a port-forward configuration persisted in SQLite
// Runtime status is managed in-memory by the PortForwarder
type PortForwardConfig struct {
	ID         string // Human-readable unique identifier
	Context    string
	Namespace  string
	Service    string
	PortRemote int
	PortLocal  int
}

// Project represents a collection of port forwards that can be activated together
type Project struct {
	Name     string   // Human-readable project name
	Forwards []string // List of port forward IDs
}
