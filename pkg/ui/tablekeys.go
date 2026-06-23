package ui

import (
	"github.com/charmbracelet/bubbles/key"
	"github.com/charmbracelet/bubbles/table"
)

// navTableKeyMap is a trimmed key map for the list tables. The bubbles table
// default binds single letters (d/u = half page, f/b = page, g/G = top/bottom)
// in addition to the arrows. Those letters are surprising in a list and collide
// with application shortcuts (e.g. 'g' toggles grouping, 'e' edits), so we keep
// only the unambiguous navigation keys: arrows + j/k for line movement and
// PageUp/PageDown + Home/End for jumping. Unset bindings match nothing.
func navTableKeyMap() table.KeyMap {
	return table.KeyMap{
		LineUp:     key.NewBinding(key.WithKeys("up", "k")),
		LineDown:   key.NewBinding(key.WithKeys("down", "j")),
		PageUp:     key.NewBinding(key.WithKeys("pgup")),
		PageDown:   key.NewBinding(key.WithKeys("pgdown")),
		GotoTop:    key.NewBinding(key.WithKeys("home")),
		GotoBottom: key.NewBinding(key.WithKeys("end")),
		// HalfPageUp / HalfPageDown intentionally left unbound.
	}
}
