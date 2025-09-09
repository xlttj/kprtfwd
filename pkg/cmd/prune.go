package cmd

import (
	"bufio"
	"flag"
	"fmt"
	"os"
	"strings"

	"github.com/xlttj/kprtfwd/pkg/config"
	"github.com/xlttj/kprtfwd/pkg/discovery"
)

// HandlePruneCommand handles the prune subcommand logic
func HandlePruneCommand() {
	// Check for help flag in prune subcommand
	if len(os.Args) > 2 {
		for _, arg := range os.Args[2:] {
			if arg == "-h" || arg == "--help" {
				showPruneHelp()
				os.Exit(0)
			}
		}
	}

	pruneCmd := flag.NewFlagSet("prune", flag.ExitOnError)
	namespaceFilter := pruneCmd.String("namespace", "*", "Namespace filter with wildcard support (e.g., 'my-app-*')")
	ctxFlag := pruneCmd.String("context", "", "Kubernetes context to use (defaults to current context)")
	acceptAll := pruneCmd.Bool("y", false, "Delete without prompting")
	verbose := pruneCmd.Bool("v", false, "Verbose output")

	pruneCmd.Usage = showPruneHelp

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

// wildcardMatch checks if text matches a wildcard pattern
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

// getContextDisplay formats the context name for display
func getContextDisplay(context string) string {
	if context == "" {
		return "(current context)"
	}
	return context
}

// showPruneHelp displays help for the prune command
func showPruneHelp() {
	programName := os.Args[0]
	fmt.Fprintf(os.Stderr, `%s prune - Remove stale port forward configurations

Remove local port forward configurations for services that no longer 
exist in the Kubernetes cluster.

Usage:
  %s prune [options]

Options:
  --context string      Kubernetes context to use (defaults to current context)
  --namespace string    Namespace filter with wildcard support (default "*")
                        Examples: 'app-*', '*-prod', 'staging'
  -y                    Delete without prompting for confirmation
  -v                    Enable verbose output
  -h, --help            Show this help message

Examples:
  %s prune                                     Prune all contexts and namespaces
  %s prune --context staging                   Prune staging context only
  %s prune --namespace 'app-*'                 Prune services in app-* namespaces
  %s prune --context prod --namespace 'api'    Prune api namespace in prod context
  %s prune -y -v                               Auto-confirm with verbose output

How it works:
  1. Discovers current services in the specified cluster/namespaces
  2. Compares against your local port forward configurations
  3. Identifies configurations for services that no longer exist
  4. Prompts for confirmation before removal (unless -y is used)

This helps keep your local configuration in sync with your cluster state.
`, programName, programName, programName, programName, programName, programName, programName)
}
