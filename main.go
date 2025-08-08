package main

import (
	"flag"
	"fmt"
	"os"

	"kprtfwd/pkg/discovery"
	"kprtfwd/pkg/logging"
	"kprtfwd/pkg/ui"

	tea "github.com/charmbracelet/bubbletea"
)

func main() {
	logging.LogDebug("Logger test: main started")

	// Parse command line arguments
	if len(os.Args) > 1 && os.Args[1] == "discover" {
		// Handle discovery subcommand
		handleDiscoverCommand()
		return
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
