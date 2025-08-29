# kprtfwd - Kubernetes Port Forward Manager

A terminal-based UI application for managing Kubernetes port forwards with project support and browser integration.

## 🚀 Features

- **Interactive Terminal UI** - Navigate port forwards with keyboard shortcuts
- **Service Discovery** - Automatically discover and generate configurations from Kubernetes services
- **Project Management** - Group port forwards into projects for easy activation
- **Browser Integration** - Open HTTP URLs directly from running port forwards
- **Context Grouping** - Organize port forwards by Kubernetes context
- **Real-time Status** - See which port forwards are actually running
- **Smart Filtering** - Search and filter port forwards by any field
- **Configuration Reload** - Hot-reload config changes without restart
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

### Homebrew (macOS) - Recommended

```bash
brew tap xlttj/kprtfwd
brew install kprtfwd
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

- No YAML files are used anymore for configuration.
- Use Ctrl+R in the TUI to refresh the view; data is already persisted.

## 🔍 Service Discovery

Service discovery automatically scans your Kubernetes cluster to find services and generates port-forward configurations for you. This eliminates the need to manually create configurations and ensures you don't miss any services.

### Basic Discovery

```bash
# Discover all services in the current context
kprtfwd discover

# Discover services with namespace filtering
kprtfwd discover --namespace 'my-app-*'

# Auto-accept all services and save to file
kprtfwd discover --namespace 'production-*' -y -o production-config.yaml
```

### Command Options

| Flag | Description | Example |
|------|-------------|----------|
| `--namespace` | Namespace filter with wildcard support | `'my-app-*'`, `'*production*'`, `'staging'` |
| `--context` | Kubernetes context to use | `staging`, `production` |
| `-o` | Output file (stdout if not specified) | `config.yaml`, `services.yaml` |
| `-y` | Accept all services without prompting | |
| `-v` | Verbose output showing discovery details | |

### Interactive Selection

When running without `-y`, you'll be prompted to select services:

- **Enter** or **y**: Include the service
- **n**: Skip the service  
- **a**: Accept this and all remaining services
- **q**: Quit without saving

### Smart Service Detection

The tool automatically:

1. **Detects Service Types**: Uses labels and naming patterns to identify databases, APIs, web services, etc.
2. **Generates Human-Readable IDs**: Creates IDs like `orbstack.mysql.main-db` or `staging.api.user-service`
3. **Shows Rich Information**: Displays ports, labels, service types, and namespaces
4. **Visual Indicators**: Uses emojis to quickly identify service types (🗃️ MySQL, 🟥 Redis, 🐘 PostgreSQL)

### Generated ID Format

Service IDs follow the pattern: `<context>.<service-type>.<discriminator>`

- **Context**: Sanitized Kubernetes context name
- **Service Type**: Detected from labels or service name (mysql, redis, api, etc.)
- **Discriminator**: Service name, optionally with port name

**Examples:**
- `orbstack.mysql.main-database`
- `production.api.user-service` 
- `staging.redis.cache-cluster`
- `local.web.frontend-app`

### Discovery Examples

#### Example 1: Development Environment Setup

```bash
# Discover all services in local development
kprtfwd discover --context docker-desktop --namespace 'dev-*' -v
```

This might generate:
```yaml
port_forwards:
  - id: "docker-desktop.mysql.dev-database"
    context: "docker-desktop"
    namespace: "dev-mysql"
    service: "mysql"
    port_remote: 3306
    port_local: 3306
  - id: "docker-desktop.redis.dev-cache"
    context: "docker-desktop" 
    namespace: "dev-redis"
    service: "redis"
    port_remote: 6379
    port_local: 6379
```

#### Example 2: Production Analysis

```bash
# Auto-discover production services for monitoring
kprtfwd discover --context prod --namespace 'monitoring-*' -y -o monitoring.yaml
```

#### Example 3: Selective Discovery

```bash
# Interactively select from staging services
kprtfwd discover --context staging --namespace 'app-*'
```

### Exporting Discovered Services

You can export discovered services to a JSON file for review or sharing:

```bash
# Export new services
kprtfwd discover --namespace 'new-services-*' -o new-services.json

# The TUI can add services directly to your local database from its discovery flow
```

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

Add projects to your config file:

```yaml
projects:
  - name: "local.development"
    forwards:
      - "local.redis"
      - "local.postgres"
      - "local.elasticsearch"
      
  - name: "staging.api-testing"
    forwards:
      - "staging.api"
      - "staging.database"
```

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
| **Space** | Toggle individual port forward on/off |
| **o** | Open HTTP URL in browser (running forwards only) |
| **g** | Toggle between grouped/ungrouped view |
| **/** | Enter filter mode |
| **Ctrl+P** | Open project selector |
| **Ctrl+R** | Reload configuration |
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
- **Running** (green): Port forward is active and healthy
- **Stopped** (red): Port forward is not running
- Status updates automatically when you start/stop forwards

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

### 5. Configuration Hot-reload
- Press **Ctrl+R** to reload config without restarting
- Automatically handles additions, removals, and changes
- Smart synchronization keeps running forwards that haven't changed
- Shows detailed summary of changes

### 6. Error Handling
- Clear error messages for common issues
- Port conflicts detection
- Invalid configuration warnings
- Kubernetes connectivity issues

## 📚 Examples

### Example 1: Local Development Setup

```yaml
port_forwards:
  - id: "local.redis"
    context: "docker-desktop"
    namespace: "cache"
    service: "redis"
    port_remote: 6379
    port_local: 6379
    
  - id: "local.postgres"
    context: "docker-desktop" 
    namespace: "database"
    service: "postgres"
    port_remote: 5432
    port_local: 5432
    
  - id: "local.api"
    context: "docker-desktop"
    namespace: "default"
    service: "api-service"
    port_remote: 8080
    port_local: 8080

projects:
  - name: "local.full-stack"
    forwards:
      - "local.redis"
      - "local.postgres" 
      - "local.api"
```

**Workflow:**
1. Run `kprtfwd`
2. Press **Ctrl+P** to open projects
3. Select "local.full-stack" and press **Enter**
4. All three services start automatically
5. Navigate to the API service and press **o** to open in browser

### Example 2: Multi-environment Testing

```yaml
port_forwards:
  # Staging Environment
  - id: "staging.api"
    context: "staging-cluster"
    namespace: "default"
    service: "api-service"
    port_remote: 8080
    port_local: 8080
    
  - id: "staging.db"
    context: "staging-cluster"
    namespace: "database"
    service: "postgres"
    port_remote: 5432
    port_local: 5432
    
  # Production Environment (different local ports)
  - id: "prod.api"
    context: "prod-cluster"
    namespace: "default"
    service: "api-service"
    port_remote: 8080
    port_local: 8081
    
  - id: "prod.db"
    context: "prod-cluster"
    namespace: "database"
    service: "postgres"
    port_remote: 5432
    port_local: 5433

projects:
  - name: "staging.testing"
    forwards:
      - "staging.api"
      - "staging.db"
      
  - name: "production.analysis"
    forwards:
      - "prod.api"
      - "prod.db"
```

**Workflow:**
1. Start with staging: **Ctrl+P** → "staging.testing" → **Enter**
2. Test your application against staging
3. Switch to production: **Ctrl+P** → "production.analysis" → **Enter**
4. Compare behavior between environments

### Example 3: Microservices Development

```yaml
port_forwards:
  - id: "user.service"
    context: "minikube"
    namespace: "services"
    service: "user-service"
    port_remote: 8080
    port_local: 8080
    
  - id: "order.service"
    context: "minikube"
    namespace: "services"
    service: "order-service"
    port_remote: 8081
    port_local: 8081
    
  - id: "payment.service"
    context: "minikube"
    namespace: "services"
    service: "payment-service"
    port_remote: 8082
    port_local: 8082
    
  - id: "shared.redis"
    context: "minikube"
    namespace: "cache"
    service: "redis"
    port_remote: 6379
    port_local: 6379

projects:
  - name: "backend.all-services"
    forwards:
      - "user.service"
      - "order.service"
      - "payment.service"
      - "shared.redis"
      
  - name: "backend.user-only"
    forwards:
      - "user.service"
      - "shared.redis"
```

**Workflow:**
1. Start all services for integration testing
2. Use browser integration to quickly test each API
3. Switch to single service mode for focused debugging
4. Use filtering to quickly find specific services

## 🐛 Troubleshooting

### Common Issues

#### Port Already in Use
**Error**: `Cannot start service: port 8080 already in use`
**Solution**: 
- Check what's using the port: `lsof -i :8080` (macOS/Linux)
- Change the local port in your config
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
**Error**: `Failed to open browser: exec: "xdg-open": executable file not found`
**Solution**:
- Install required tools:
  - Linux: `sudo apt-get install xdg-utils`
  - Ensure your system has a default browser configured

### Debug Mode

Enable debug logging for troubleshooting:

```bash
DEBUG=1 kprtfwd
```

This will create detailed logs in `./kprtfwd.log` showing:
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

kprtfwd persists your port forwards and projects in a local SQLite database at `~/.kprtfwd/kprtfwd.db`. You can manage all entries from within the TUI. The CLI discovery command can export discovered services to JSON for review, but configuration is no longer loaded from YAML files.

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
