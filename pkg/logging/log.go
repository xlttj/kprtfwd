package logging

import (
	"fmt"
	"os"
	"sync"
	"time"
)

var (
	logFile  *os.File
	logMutex sync.Mutex
)

func init() {
	var err error
	logFile, err = os.OpenFile("prtfwd.log", os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0644)
	if err != nil {
		panic("failed to open log file: " + err.Error())
	}
}

func log(level, msg string) {
	logMutex.Lock()
	defer logMutex.Unlock()
	timestamp := time.Now().Format("2006-01-02 15:04:05")
	fmt.Fprintf(logFile, "%s [%s] %s\n", timestamp, level, msg)
	logFile.Sync()
}

func LogDebug(format string, args ...interface{}) {
	log("DEBUG", fmt.Sprintf(format, args...))
}

func LogError(format string, args ...interface{}) {
	log("ERROR", fmt.Sprintf(format, args...))
}
