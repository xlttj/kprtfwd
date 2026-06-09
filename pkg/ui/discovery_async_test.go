package ui

import (
	"testing"

	"github.com/xlttj/kprtfwd/pkg/config"
	"github.com/xlttj/kprtfwd/pkg/discovery"
)

// fakeConfigStore is a minimal ConfigStoreInterface implementation for tests.
// Only the read methods used by the discovery handlers carry real behaviour;
// the rest satisfy the interface as no-ops.
type fakeConfigStore struct {
	configs []config.PortForwardConfig
}

func (f *fakeConfigStore) Add(cfg config.PortForwardConfig) error { return nil }
func (f *fakeConfigStore) GetAll() []config.PortForwardConfig     { return f.configs }
func (f *fakeConfigStore) Len() int                               { return len(f.configs) }
func (f *fakeConfigStore) Get(index int) (config.PortForwardConfig, bool) {
	if index < 0 || index >= len(f.configs) {
		return config.PortForwardConfig{}, false
	}
	return f.configs[index], true
}
func (f *fakeConfigStore) GetWithError(index int) (config.PortForwardConfig, error) {
	if index < 0 || index >= len(f.configs) {
		return config.PortForwardConfig{}, config.ErrConfigNotFound
	}
	return f.configs[index], nil
}
func (f *fakeConfigStore) GetConfigByID(id string) (config.PortForwardConfig, bool) {
	return config.PortForwardConfig{}, false
}
func (f *fakeConfigStore) GetIndexByID(id string) (int, bool)            { return 0, false }
func (f *fakeConfigStore) CreateProject(name string, ids []string) error { return nil }
func (f *fakeConfigStore) GetProjects() []config.Project                 { return nil }
func (f *fakeConfigStore) GetAllProjects() []config.Project              { return nil }
func (f *fakeConfigStore) DeleteProject(name string) error               { return nil }
func (f *fakeConfigStore) SetActiveProject(name string) error            { return nil }
func (f *fakeConfigStore) GetActiveProject() *config.Project             { return nil }
func (f *fakeConfigStore) ClearActiveProject()                           {}
func (f *fakeConfigStore) GetActiveProjectName() string                  { return "" }
func (f *fakeConfigStore) GetActiveProjectForwards() []config.PortForwardConfig {
	return f.configs
}
func (f *fakeConfigStore) Load() error { return nil }
func (f *fakeConfigStore) Save() error { return nil }

// newDiscoveryResult builds a single-service discovery result with the given ports.
func newDiscoveryResult(cluster, namespace, service string, ports ...discovery.ServicePort) *discovery.DiscoveryResult {
	return &discovery.DiscoveryResult{
		Context:    cluster,
		TotalCount: 1,
		Services: []discovery.DiscoveredService{
			{
				ServiceInfo: discovery.ServiceInfo{
					Name:      service,
					Namespace: namespace,
					Type:      "ClusterIP",
					Ports:     ports,
				},
			},
		},
	}
}

func TestHandleServicesDiscovered_BuildsPortSelections(t *testing.T) {
	// Existing config already maps ctx1/default/api remote 8080 -> local 18080.
	store := &fakeConfigStore{configs: []config.PortForwardConfig{
		{Context: "ctx1", Namespace: "default", Service: "api", PortRemote: 8080, PortLocal: 18080},
	}}
	m := &Model{
		configStore:      store,
		uiState:          StateServiceDiscovery,
		discoveryLoading: true,
	}

	result := newDiscoveryResult("ctx1", "default", "api",
		discovery.ServicePort{Port: 8080, Protocol: "TCP"},
		discovery.ServicePort{Port: 9090, Protocol: "TCP"},
	)

	m.handleServicesDiscovered(servicesDiscoveredMsg{cluster: "ctx1", result: result})

	if m.discoveryLoading {
		t.Fatal("expected discoveryLoading to be cleared")
	}
	if m.discoveryPhase != PhaseServiceSelection {
		t.Fatalf("expected PhaseServiceSelection, got %v", m.discoveryPhase)
	}
	if len(m.discoveryPorts) != 2 {
		t.Fatalf("expected 2 port selections, got %d", len(m.discoveryPorts))
	}

	// Find each port by remote port number (order follows service port order here).
	var p8080, p9090 *PortSelection
	for i := range m.discoveryPorts {
		switch m.discoveryPorts[i].Port.Port {
		case 8080:
			p8080 = &m.discoveryPorts[i]
		case 9090:
			p9090 = &m.discoveryPorts[i]
		}
	}
	if p8080 == nil || p9090 == nil {
		t.Fatal("expected port selections for both 8080 and 9090")
	}

	// Pre-existing port: pre-selected, keeps existing local port, knows its config index.
	if !p8080.Selected {
		t.Error("expected pre-existing port 8080 to be pre-selected")
	}
	if p8080.LocalPort != 18080 {
		t.Errorf("expected existing local port 18080, got %d", p8080.LocalPort)
	}
	if p8080.ExistingConfigIndex != 0 {
		t.Errorf("expected ExistingConfigIndex 0, got %d", p8080.ExistingConfigIndex)
	}

	// New port: unselected, local defaults to remote, no existing config.
	if p9090.Selected {
		t.Error("expected new port 9090 to be unselected")
	}
	if p9090.LocalPort != 9090 {
		t.Errorf("expected default local port 9090, got %d", p9090.LocalPort)
	}
	if p9090.ExistingConfigIndex != -1 {
		t.Errorf("expected ExistingConfigIndex -1 for new port, got %d", p9090.ExistingConfigIndex)
	}
}

func TestHandleServicesDiscovered_IgnoredWhenNavigatedAway(t *testing.T) {
	store := &fakeConfigStore{}
	m := &Model{
		configStore:      store,
		uiState:          StatePortForwards, // user already left discovery
		discoveryLoading: true,
		discoveryPhase:   PhaseClusterSelection,
	}

	result := newDiscoveryResult("ctx1", "default", "api",
		discovery.ServicePort{Port: 8080, Protocol: "TCP"})
	m.handleServicesDiscovered(servicesDiscoveredMsg{cluster: "ctx1", result: result})

	if m.discoveryLoading {
		t.Error("expected discoveryLoading to be cleared even when ignored")
	}
	if m.discoveryPhase == PhaseServiceSelection {
		t.Error("expected phase to be unchanged after navigating away")
	}
	if len(m.discoveryPorts) != 0 {
		t.Error("expected no port selections to be built after navigating away")
	}
}

func TestHandleServicesDiscovered_Error(t *testing.T) {
	store := &fakeConfigStore{}
	m := &Model{
		configStore:      store,
		uiState:          StateServiceDiscovery,
		discoveryLoading: true,
		discoveryPhase:   PhaseClusterSelection,
	}

	m.handleServicesDiscovered(servicesDiscoveredMsg{
		cluster: "ctx1",
		err:     config.ErrConfigNotFound, // any error
	})

	if m.discoveryLoading {
		t.Error("expected discoveryLoading to be cleared on error")
	}
	if m.errorMsg == "" {
		t.Error("expected an error message to be set")
	}
	if m.discoveryPhase == PhaseServiceSelection {
		t.Error("expected to stay out of service selection on error")
	}
}

func TestHandleClustersLoaded_SelectsCurrentContext(t *testing.T) {
	m := &Model{uiState: StateServiceDiscovery, discoveryLoading: true}

	m.handleClustersLoaded(clustersLoadedMsg{
		clusters: []string{"ctx-a", "ctx-b", "ctx-c"},
		current:  "ctx-b",
	})

	if m.discoveryLoading {
		t.Error("expected discoveryLoading to be cleared")
	}
	if len(m.discoveryClusters) != 3 {
		t.Fatalf("expected 3 clusters cached, got %d", len(m.discoveryClusters))
	}
	if m.discoverySelectedCluster != 1 {
		t.Errorf("expected current context ctx-b (index 1) selected, got %d", m.discoverySelectedCluster)
	}
}

func TestHandleClustersLoaded_EmptyReturnsToMain(t *testing.T) {
	m := &Model{uiState: StateServiceDiscovery, discoveryLoading: true}

	m.handleClustersLoaded(clustersLoadedMsg{clusters: nil})

	if m.uiState != StatePortForwards {
		t.Errorf("expected to return to StatePortForwards, got %v", m.uiState)
	}
	if m.errorMsg == "" {
		t.Error("expected an error message when no clusters are found")
	}
}
