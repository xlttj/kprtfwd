# kprtfwd - Kubernetes Port Forward Manager

A terminal-based UI application for managing Kubernetes port forwards with project support and browser integration.

## 🚀 Features

- **Interactive Terminal UI** - Navigate port forwards with keyboard shortcuts
- **Service Discovery** - Automatically discover and generate configurations from Kubernetes services
- **Project Management** - Group port forwards into projects for easy activation
- **Browser Integration** - Open HTTP URLs directly from running port forwards
- **Context Grouping** - Organize port forwards by Kubernetes context
- **Real-time Status** - See which forwards are running, stopped, or errored, with automatic health checks
- **Smart Filtering** - Search and filter port forwards by any field
- **Self-healing** - Detects dropped connections and broken tunnels, and auto-restarts transient failures with backoff
- **Port Forward Restart** - Manually restart running or errored forwards (Ctrl+R) when connectivity is lost
- **Cross-platform** - Works on macOS, Linux, and Windows

## 📋 Table of Contents

- [Installation](#installation)
- [Configuration](#configuration)
- [Service Discovery](#service-discovery)
- [Usage](#usage)
- [Projects](#projects)
- [Keyboard Shortcuts](#keyboard-shortcuts)
- [Features](#features-detailed)
- [Examples](#examples)
- [Troubleshooting](#troubleshooting)

## 🛠 Installation

### Prerequisites

- kubectl configured with access to your Kubernetes clusters
- Access to the Kubernetes contexts you want to port-forward from
- Go 1.25.10 or newer — only needed to install via `go install` or build from source (with the default `GOTOOLCHAIN=auto`, the toolchain is fetched automatically)

### Homebrew (macOS) - Recommended

```bash
brew tap xlttj/tap
brew install xlttj/tap/kprtfwd
```

### Go Install

```bash
go install github.com/xlttj/kprtfwd@latest
```

### Manual Installation

1. Download the latest release for your platform from [GitHub Releases](https://github.com/xlttj/kprtfwd/releases)
2. Extract and make executable:
   ```bash
   # For macOS/Linux
   chmod +x kprtfwd-*
   mv kprtfwd-* /usr/local/bin/kprtfwd
   ```

### Build from Source

```bash
git clone https://github.com/xlttj/kprtfwd.git
cd kprtfwd
go build -o kprtfwd main.go
```

## ⚙️ Configuration

kprtfwd stores its configuration in a local SQLite database at `~/.kprtfwd/kprtfwd.db`. The TUI manages configuration (adding/removing services, editing ports, managing projects), and changes are persisted automatically.

## 🔍 Service Discovery

Service discovery is fully integrated into the TUI. It scans your Kubernetes
cluster for services and lets you interactively select which ones to add as
port-forward configurations. Discovered services are saved to the local SQLite
store automatically—no YAML export/import required.

The cluster and service lookups run in the background so the UI stays
responsive even on large clusters; a loading screen is shown while they run and
you can cancel with Esc.

### How to open discovery

From the main view, press Ctrl+D

1) Cluster selection
   - Choose the Kubernetes context to discover
   - Navigation: Up/Down or j/k
   - Select: Enter
   - Back: Esc (returns to the main view)

2) Service selection
   - See a list of services in the selected context (with namespace, type, ports)
   - Toggle selection: Space
   - Filter the list: Press /, type text, Enter to apply (Esc to clear/cancel)
   - Edit proposed local port for a highlighted service: e
     - You can only edit newly discovered entries here; existing configs should be
       edited from the main view
   - Confirm and add selected services: Enter
   - Back to cluster selection: Esc

3) Save and use
   - After confirming, selected services are added to your configuration and
     persisted to ~/.kprtfwd/kprtfwd.db
   - You’ll return to the main view where you can start/stop them with Space

Tip: You can always re-open discovery (Ctrl+D) to add more services. Use
filtering (/) to quickly narrow down large clusters.

## 🎮 Usage

### Starting the Application

```bash
# Run from anywhere
kprtfwd

# Or with debug logging
DEBUG=1 kprtfwd
```

### Basic Navigation

1. Use **arrow keys** or **j/k** to navigate through port forwards
2. Press **Space** to start/stop individual port forwards
3. Press **q** to quit the application

### Quick Start Example

1. Run `kprtfwd`
2. Press Ctrl+D to open service discovery and add desired services
3. Navigate to a port forward using arrow keys
4. Press **Space** to start it
5. Press **o** to open it in your browser (if it's HTTP)

## 📁 Projects

Projects allow you to group related port forwards and manage them as a unit.

### Creating Projects

You create and manage projects entirely in the TUI:

- Press Ctrl+P to open the Project Selector
- Press m to open Project Management
- Press n (or c) to create a new project, enter a name, then select services to include
- Press d to delete a project
- Use arrow keys and Enter to navigate and confirm

### Using Projects

1. Press **Ctrl+P** to open the project selector
2. Use **arrow keys** to select a project
3. Press **Enter** to activate the project
4. All port forwards in the project will start automatically
5. Press **Esc** to return to the main view

### Project Behavior

- **Automatic Management**: When you select a project, all currently running port forwards stop, and all port forwards in the selected project start
- **Visual Indication**: The UI shows which project is currently active
- **Filtering**: When a project is active, only its port forwards are displayed

## ⌨️ Keyboard Shortcuts

### Main View
| Key | Action |
|-----|--------|
| **↑/↓** or **j/k** | Navigate through port forwards |
| **PgUp/PgDn**, **Home/End** | Page through / jump to start or end of the list |
| **Space** | Toggle individual port forward on/off |
| **e** | Edit the local port of the selected forward |
| **o** | Open HTTP URL in browser (running forwards only) |
| **g** | Toggle between grouped/ungrouped view |
| **/** | Enter filter mode |
| **S** | Stop all running port forwards |
| **Ctrl+P** | Open project selector |
| **Ctrl+R** | Restart running and errored port forwards |
| **q** | Quit application |
| **Esc** | Clear active filter |

### Filter Mode
| Key | Action |
|-----|--------|
| **Type** | Enter filter text |
| **Enter** | Apply filter and exit filter mode |
| **Esc** | Cancel filter and exit filter mode |

### Project Selector
| Key | Action |
|-----|--------|
| **↑/↓** or **j/k** | Navigate through projects |
| **Enter** | Select project and return to main view |
| **Esc** | Cancel and return to main view |

### Group Headers (Grouped View Only)
| Key | Action |
|-----|--------|
| **Space** | Expand/collapse group |

## 🔧 Features (Detailed)

### 1. Real-time Status Display
- **Running** (green): Port forward is active
- **Stopped** (grey): Port forward is not running
- **Error** (red): Port forward failed to start or exited unexpectedly (e.g. VPN drop, pod restart, broken tunnel)
- Status refreshes automatically every couple of seconds, including forwards that died or whose tunnel went down on their own
- Select an **Error** row to see the failure reason (kubectl's message) in the footer; full details are written to the log file

### 2. Browser Integration
- Press **o** on any running HTTP service to open it in your default browser
- Automatically constructs the URL as `http://localhost:[local_port]`
- Works on macOS (open), Linux (xdg-open), and Windows (rundll32)
- Shows success/error messages

### 3. Context Grouping
- Port forwards are automatically grouped by Kubernetes context
- Toggle between grouped and flat view with **g**
- Expand/collapse groups with **Space** when on a group header

### 4. Smart Filtering
- Filter by any field: context, namespace, service, ports
- Case-insensitive search
- Works with both grouped and ungrouped views
- Respects active project filtering

### 5. Port Forward Restart & Auto-Restart
- Press **Ctrl+R** to restart all running **and errored** port forwards
- Useful when network connectivity is lost (e.g., VPN disconnect)
- Shows a summary of restarted forwards and any errors
- **Automatic restart**: forwards that were running and then broke (VPN drop, pod restart, tunnel reset) are retried automatically with exponential backoff, up to 5 attempts. A failed row shows `(auto-retry n/5)` in the footer while it recovers.
- Initial-start failures (e.g. a misconfigured service) are *not* auto-retried — they stay in **Error** for a manual **Ctrl+R**, so kprtfwd never spins on a permanent failure

### 6. Error Handling
- Failed forwards are marked **Error** with the reason shown in the footer when selected, and recorded in the log file
- Detects forwards whose kubectl process exited or whose tunnel went dead (via a TCP health probe)
- Port conflicts detection
- Invalid configuration warnings
- Kubernetes connectivity issues

## 🐛 Troubleshooting

### Common Issues

#### Port Already in Use
**Error**: `Cannot start service: port 8080 already in use`
**Solution**: 
- Check what's using the port: `lsof -i :8080` (macOS/Linux)
- Change the local port in the TUI (press e on the selected service)
- Stop the conflicting process

#### Kubernetes Connection Issues
**Error**: `Cannot connect to context 'staging'`
**Solution**:
- Verify kubectl access: `kubectl --context staging get pods`
- Check your kubeconfig: `kubectl config get-contexts`
- Ensure the context name matches exactly

#### Service Not Found
**Error**: `Service 'api-service' not found in namespace 'default'`
**Solution**:
- List services: `kubectl --context staging get svc -n default`
- Verify namespace and service name in config
- Check if the service exists in a different namespace

#### Browser Won't Open
**Error**: `Failed to open browser: exec: "#": executable file not found`
**Solution**:
- Install required tools:
  - Linux: `sudo apt-get install xdg-utils`
  - Ensure your system has a default browser configured

### Debug Mode

Enable debug logging for troubleshooting:

```bash
DEBUG=1 kprtfwd
```

This will create detailed logs in `~/.kprtfwd/logs/kprtfwd.log` showing:
- Port forward startup/shutdown events  
- Configuration loading and parsing
- Kubernetes API interactions
- UI state changes

### Configuration Validation

The application validates your configuration on startup. Common validation errors:

- **Duplicate IDs**: Each port forward must have a unique `id`
- **Missing Required Fields**: All fields are required except `projects`
- **Invalid Project References**: Project forwards must reference existing port forward IDs
- **Port Conflicts**: Multiple forwards can't use the same local port

## 📄 Data Storage

kprtfwd persists your port forwards and projects in a local SQLite database at `~/.kprtfwd/kprtfwd.db`. Manage everything from within the TUI.

## 🤝 Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Test thoroughly
5. Submit a pull request

## 📝 License

This project is licensed under the MIT License - see the LICENSE file for details.

---

**Happy Port Forwarding! 🚀**

For issues and feature requests, please create an issue in the repository.
