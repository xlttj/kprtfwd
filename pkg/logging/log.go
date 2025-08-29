package logging

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sync"
	"time"
)

var (
	logFile   *os.File
	logMutex  sync.Mutex
	debugMode bool
)

func init() {
	debugMode = os.Getenv("DEBUG") != ""
	// Prepare private log directory
	home, err := os.UserHomeDir()
	if err != nil {
		// If home cannot be determined, disable logging gracefully
		return
	}
	logDir := filepath.Join(home, ".kprtfwd", "logs")
	_ = os.MkdirAll(logDir, 0700)
	logPath := filepath.Join(logDir, "kprtfwd.log")

	// Simple size-based rotation: if file > ~5MB, rotate to .1
	if fi, err := os.Stat(logPath); err == nil {
		if fi.Size() > 5*1024*1024 {
			_ = rotateOnce(logPath)
		}
	}

	f, err := os.OpenFile(logPath, os.O_APPEND|os.O_CREATE|os.O_WRONLY, fs.FileMode(0600))
	if err != nil {
		return
	}
	logFile = f
}

func rotateOnce(path string) error {
	_ = os.Remove(path + ".1")
	return os.Rename(path, path+".1")
}

func log(level, msg string) {
	if logFile == nil {
		return
	}
	logMutex.Lock()
	defer logMutex.Unlock()
	timestamp := time.Now().Format("2006-01-02 15:04:05")
	fmt.Fprintf(logFile, "%s [%s] %s\n", timestamp, level, msg)
	_ = logFile.Sync()
}

func LogDebug(format string, args ...interface{}) {
	if !debugMode {
		return
	}
	log("DEBUG", fmt.Sprintf(format, args...))
}

func LogError(format string, args ...interface{}) {
	log("ERROR", fmt.Sprintf(format, args...))
}
