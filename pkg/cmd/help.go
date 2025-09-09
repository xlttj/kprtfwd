package cmd

import (
	"fmt"
	"os"
)

// HandleHelpCommand displays help information for the application
func HandleHelpCommand() {
	showMainHelp()
}

// showMainHelp displays the main application help
func showMainHelp() {
	programName := os.Args[0]
	fmt.Printf(`kprtfwd - Kubernetes Port Forward Manager

A terminal-based UI application for managing Kubernetes port forwards 
with project support and browser integration.

Usage:
  %s [command]

Available Commands:
  prune    Remove local services that no longer exist in the cluster
  help     Show help information

Options:
  -h, --help  Show help information

Interactive Mode:
  Run without any command to start the interactive TUI where you can:
  - Manage port forwards with Space to start/stop
  - Use Ctrl+D to discover new services from your cluster
  - Use Ctrl+P to manage projects
  - Press 'o' to open running services in browser
  - Use '/' to filter services

Examples:
  %s                            Start interactive TUI
  %s prune --context staging    Remove stale services from staging
  %s help                       Show this help message

For more information about a specific command, use:
  %s <command> --help

Project Repository: https://github.com/xlttj/kprtfwd
`, programName, programName, programName, programName, programName)
}

// ShowMainHelpAndExit displays help and exits with code 0
func ShowMainHelpAndExit() {
	showMainHelp()
	os.Exit(0)
}
