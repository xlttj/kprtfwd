package config

// PortForwardConfig represents a port-forward configuration
// Runtime status is managed in-memory by the PortForwarder
type PortForwardConfig struct {
	ID         string `yaml:"id"`         // Human-readable unique identifier
	Context    string `yaml:"context"`
	Namespace  string `yaml:"namespace"`
	Service    string `yaml:"service"`
	PortRemote int    `yaml:"port_remote"`
	PortLocal  int    `yaml:"port_local"`
}

// Project represents a collection of port forwards that can be activated together
type Project struct {
	Name     string   `yaml:"name"`     // Human-readable project name
	Forwards []string `yaml:"forwards"` // List of port forward IDs
}

// ConfigFile represents the YAML configuration file structure
type ConfigFile struct {
	PortForwards []PortForwardConfig `yaml:"port_forwards"`
	PortForwardsOld []PortForwardConfig `yaml:"portforwards,omitempty"` // Backward compatibility
	Projects     []Project           `yaml:"projects,omitempty"`
}

// The path to the config file is now in the user's home directory
const ConfigFilePath = "~/.kprtfwd/config.yaml"
