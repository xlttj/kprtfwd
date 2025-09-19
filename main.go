package main

import (
	"fmt"
	"os"

	"github.com/xlttj/kprtfwd/pkg/cmd"
	"github.com/xlttj/kprtfwd/pkg/logging"
	"github.com/xlttj/kprtfwd/pkg/ui"

	tea "github.com/charmbracelet/bubbletea"
)

func main() {
	logging.LogDebug("Logger test: main started")

	// Check for help flags first
	if len(os.Args) > 1 {
		arg := os.Args[1]
		if arg == "-h" || arg == "--help" {
			cmd.ShowMainHelpAndExit()
		}
	}

	// Parse command line arguments
	if len(os.Args) > 1 {
		sub := os.Args[1]
		switch sub {
		case "help":
			cmd.HandleHelpCommand()
			return
		case "prune":
			cmd.HandlePruneCommand()
			return
		default:
			// Unknown command
			fmt.Printf("Error: unknown command '%s'\n\n", sub)
			cmd.ShowMainHelpAndExit()
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
