package main

import (
	"bufio"
	"flag"
	"fmt"
	"os"
	"strings"

	"kprtfwd/pkg/config"
	"kprtfwd/pkg/discovery"
	"kprtfwd/pkg/logging"
	"kprtfwd/pkg/ui"

	tea "github.com/charmbracelet/bubbletea"
)

func main() {
	logging.LogDebug("Logger test: main started")

	// Parse command line arguments
	if len(os.Args) > 1 {
		sub := os.Args[1]
		switch sub {
		case "discover":
			// Handle discovery subcommand
			handleDiscoverCommand()
			return
		case "prune":
			handlePruneCommand()
			return
		}
	}

	// Default behavior - start TUI
	model := ui.NewModel()
	p := tea.NewProgram(model, tea.WithAltScreen())
	if _, err := p.Run(); err != nil {
		fmt.Printf("Error: %v\n", err)
		os.Exit(1)
	}
	model.Cleanup() // if needed
}

func handleDiscoverCommand() {
	// Create a new flag set for the discover subcommand
	discoverCmd := flag.NewFlagSet("discover", flag.ExitOnError)

	// Define flags
	namespaceFilter := discoverCmd.String("namespace", "*", "Namespace filter with wildcard support (e.g., 'my-app-*')")
	context := discoverCmd.String("context", "", "Kubernetes context to use (defaults to current context)")
	outputFile := discoverCmd.String("o", "", "Output file (defaults to stdout)")
	acceptAll := discoverCmd.Bool("y", false, "Accept all discovered services without prompting")
	verbose := discoverCmd.Bool("v", false, "Verbose output")

	// Add usage information
	discoverCmd.Usage = func() {
		fmt.Fprintf(os.Stderr, "Usage: %s discover [options]\n\n", os.Args[0])
		fmt.Fprintf(os.Stderr, "Discover Kubernetes services and generate port-forward configuration.\n\n")
		fmt.Fprintf(os.Stderr, "Options:\n")
		discoverCmd.PrintDefaults()
		fmt.Fprintf(os.Stderr, "\nExamples:\n")
		fmt.Fprintf(os.Stderr, "  %s discover --namespace 'my-app-*' --context staging\n", os.Args[0])
		fmt.Fprintf(os.Stderr, "  %s discover --namespace 'production-*' -y -o config.yaml\n", os.Args[0])
		fmt.Fprintf(os.Stderr, "  %s discover --context local --namespace '*' -v\n", os.Args[0])
	}

	// Parse the discover command arguments (skip the "discover" part)
	err := discoverCmd.Parse(os.Args[2:])
	if err != nil {
		fmt.Printf("Error parsing arguments: %v\n", err)
		os.Exit(1)
	}

	// Set up discovery options
	opts := discovery.Options{
		NamespaceFilter: *namespaceFilter,
		Context:         *context,
		OutputFile:      *outputFile,
		AcceptAll:       *acceptAll,
		Verbose:         *verbose,
	}

	if opts.Verbose {
		fmt.Printf("🔍 Starting service discovery...\n")
		fmt.Printf("   Context: %s\n", getContextDisplay(opts.Context))
		fmt.Printf("   Namespace filter: %s\n", opts.NamespaceFilter)
		fmt.Printf("   Accept all: %v\n", opts.AcceptAll)
		fmt.Printf("   Output: %s\n\n", getOutputDisplay(opts.OutputFile))
	}

	// Run the discovery process
	err = discovery.RunDiscovery(opts)
	if err != nil {
		fmt.Printf("Error during discovery: %v\n", err)
		os.Exit(1)
	}
}

func handlePruneCommand() {
	pruneCmd := flag.NewFlagSet("prune", flag.ExitOnError)
	namespaceFilter := pruneCmd.String("namespace", "*", "Namespace filter with wildcard support (e.g., 'my-app-*')")
	ctxFlag := pruneCmd.String("context", "", "Kubernetes context to use (defaults to current context)")
	acceptAll := pruneCmd.Bool("y", false, "Delete without prompting")
	verbose := pruneCmd.Bool("v", false, "Verbose output")
	
	pruneCmd.Usage = func() {
		fmt.Fprintf(os.Stderr, "Usage: %s prune [options]\n\n", os.Args[0])
		fmt.Fprintf(os.Stderr, "Remove local services that no longer exist in the cluster.\n\n")
		fmt.Fprintf(os.Stderr, "Options:\n")
		pruneCmd.PrintDefaults()
		fmt.Fprintf(os.Stderr, "\nExamples:\n")
		fmt.Fprintf(os.Stderr, "  %s prune --context staging --namespace 'app-*'\n", os.Args[0])
		fmt.Fprintf(os.Stderr, "  %s prune --context prod -y\n", os.Args[0])
	}
	
	if err := pruneCmd.Parse(os.Args[2:]); err != nil {
		fmt.Printf("Error parsing arguments: %v\n", err)
		os.Exit(1)
	}
	
	// Discover current services in the cluster
	discoveryOpts := discovery.Options{
		NamespaceFilter: *namespaceFilter,
		Context:         *ctxFlag,
	}
	result, err := discovery.DiscoverServices(discoveryOpts)
	if err != nil {
		fmt.Printf("Error discovering services: %v\n", err)
		os.Exit(1)
	}
	actualContext := result.Context // effective context used
	if *verbose {
		fmt.Printf("Prune in context: %s, namespace filter: %s\n", getContextDisplay(actualContext), *namespaceFilter)
	}
	// Build discovered service set namespace/name
	discovered := make(map[string]bool)
	for _, svc := range result.Services {
		key := svc.ServiceInfo.Namespace + "/" + svc.ServiceInfo.Name
		discovered[key] = true
	}
	// Load local configs
	store, err := config.NewSQLiteConfigStore()
	if err != nil {
		fmt.Printf("Error opening config store: %v\n", err)
		os.Exit(1)
	}
	defer store.Close()
	configs := store.GetAll()
	// Find stale entries
	var stale []config.PortForwardConfig
	for _, cfg := range configs {
		if cfg.Context != actualContext {
			continue
		}
		if !wildcardMatch(cfg.Namespace, *namespaceFilter) {
			continue
		}
		key := cfg.Namespace + "/" + cfg.Service
		if !discovered[key] {
			stale = append(stale, cfg)
		}
	}
	if len(stale) == 0 {
		fmt.Printf("✅ No stale services to remove.\n")
		return
	}
	fmt.Printf("Found %d stale service(s):\n", len(stale))
	for _, s := range stale {
		fmt.Printf("  - %s (%s/%s:%d)\n", s.ID, s.Namespace, s.Service, s.PortRemote)
	}
	if !*acceptAll {
		fmt.Print("Delete these services from local config? [y/N]: ")
		reader := bufio.NewReader(os.Stdin)
		resp, _ := reader.ReadString('\n')
		resp = strings.TrimSpace(strings.ToLower(resp))
		if resp != "y" && resp != "yes" {
			fmt.Println("Aborted.")
			return
		}
	}
	// Delete
	deleted := 0
	for _, s := range stale {
		if err := store.DeletePortForward(s.ID); err != nil {
			fmt.Printf("Error deleting %s: %v\n", s.ID, err)
			continue
		}
		deleted++
	}
	fmt.Printf("🧹 Removed %d stale service(s).\n", deleted)
}

func wildcardMatch(text, pattern string) bool {
	if pattern == "*" {
		return true
	}
	if pattern == "" {
		return text == ""
	}
	if strings.HasPrefix(pattern, "*") && strings.HasSuffix(pattern, "*") {
		mid := pattern[1 : len(pattern)-1]
		return strings.Contains(text, mid)
	}
	if strings.HasPrefix(pattern, "*") {
		suf := pattern[1:]
		return strings.HasSuffix(text, suf)
	}
	if strings.HasSuffix(pattern, "*") {
		pre := pattern[:len(pattern)-1]
		return strings.HasPrefix(text, pre)
	}
	return text == pattern
}

func getContextDisplay(context string) string {
	if context == "" {
		return "(current context)"
	}
	return context
}

func getOutputDisplay(outputFile string) string {
	if outputFile == "" {
		return "stdout"
	}
	return outputFile
}
