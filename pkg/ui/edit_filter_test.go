package ui

import (
	"testing"

	"github.com/xlttj/kprtfwd/pkg/config"
	"github.com/xlttj/kprtfwd/pkg/k8s"

	"github.com/charmbracelet/bubbles/textinput"
)

// Regression: editing a port while a filter is active must update the visible
// list. The filtered list is a cached snapshot; commitPortEdit mutates the
// store, so the cache has to be rebuilt or the row keeps showing the old port.
func TestCommitPortEditWithActiveFilterUpdatesList(t *testing.T) {
	t.Setenv("HOME", t.TempDir()) // isolate the SQLite store from the real home

	store, err := config.NewSQLiteConfigStore()
	if err != nil {
		t.Fatalf("failed to create store: %v", err)
	}
	defer store.Close()

	cfg := config.PortForwardConfig{
		ID: "ctx.ns.web", Context: "ctx", Namespace: "ns",
		Service: "web", PortRemote: 80, PortLocal: 8080,
	}
	if err := store.Add(cfg); err != nil {
		t.Fatalf("failed to add config: %v", err)
	}

	filterInput := textinput.New()
	filterInput.SetValue("web") // filter on the service name, not the port

	editInput := textinput.New()
	editInput.SetValue("9090")

	m := &Model{
		configStore:     store,
		portForwarder:   k8s.NewPortForwarder(),
		filterInput:     filterInput,
		editInput:       editInput,
		editMode:        true,
		editConfigIndex: 0,
	}
	m.applyFilter() // populate the (soon-to-be-stale) cache, as typing would

	_, _ = m.commitPortEdit()

	// The store must hold the new port...
	updated, ok := store.GetConfigByID("ctx.ns.web")
	if !ok || updated.PortLocal != 9090 {
		t.Fatalf("store should have port 9090, got ok=%v port=%d", ok, updated.PortLocal)
	}

	// ...and the filtered cache that drives the visible list must reflect it.
	if len(m.filteredConfigs) != 1 {
		t.Fatalf("expected 1 filtered config, got %d", len(m.filteredConfigs))
	}
	if m.filteredConfigs[0].PortLocal != 9090 {
		t.Fatalf("filtered list still shows stale port %d, want 9090", m.filteredConfigs[0].PortLocal)
	}
}
