package main

import (
    "fmt"
    "os"
    "time"
)

func main() {
    expected := os.Getenv("PANEL_EXPECT_ARGUMENT")
    if len(os.Args) != 2 || os.Args[1] != expected {
        panic("程序参数没有完整透传")
    }
    fmt.Println("GO_LAUNCH_OK")
    if os.Getenv("PANEL_PROBE_SLEEP") != "" { time.Sleep(30 * time.Second) }
}
