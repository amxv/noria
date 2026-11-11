package app

import (
	"errors"
	"fmt"
	"github.com/amxv/noria/internal/config"

	"github.com/amxv/noria/internal/service"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
)

func preferencesCommand(home string, args []string, out io.Writer) error {
	asJSON := false
	filtered := []string{}
	for _, v := range args {
		if v == "--json" {
			asJSON = true
		} else {
			filtered = append(filtered, v)
		}
	}
	args = filtered
	if len(args) > 0 && isHelp(args[0]) {
		fmt.Fprintln(out, "Usage: noria preferences [start-at-login|menubar-at-login|tray-at-login|auto-update on|off] [--json]\n       noria preferences quota-refresh-minutes on-open|0|2|5|10|15|30|60|120|180 [--json]\n0 (on-open) is the default: refresh on menu open or manually, with no periodic frontend refresh.\nAll preferences are owned by the CLI. Quitting the desktop UI does not stop routing.")
		return nil
	}
	if e := config.Init(home); e != nil {
		return e
	}
	if len(args) > 0 {
		if args[0] == "quota-refresh-minutes" {
			if len(args) != 2 {
				return errors.New("usage: noria preferences quota-refresh-minutes on-open|0|2|5|10|15|30|60|120|180")
			}
			minutes := 0
			if args[1] != "on-open" {
				var err error
				minutes, err = strconv.Atoi(args[1])
				if err != nil {
					return errors.New("quota refresh interval must be on-open, 0, 2, 5, 10, 15, 30, 60, 120, or 180")
				}
			}
			if !config.ValidQuotaRefreshMinutes(minutes) {
				return errors.New("quota refresh interval must be on-open, 0, 2, 5, 10, 15, 30, 60, 120, or 180")
			}
			if err := config.Change(home, func(c *config.Config) error {
				c.QuotaRefreshMinutes = minutes
				return nil
			}); err != nil {
				return err
			}
		} else {
			if len(args) != 2 || args[1] != "on" && args[1] != "off" {
				return errors.New("specify a preference followed by on or off")
			}
			enabled := args[1] == "on"
			exe, e := os.Executable()
			if e != nil {
				return e
			}
			switch args[0] {
			case "start-at-login":
				if e = service.Login(home, exe, enabled); e != nil {
					return e
				}
			case "menubar-at-login":
				if runtime.GOOS != "darwin" {
					return errors.New("the native menu bar is available on macOS")
				}
				if e = menuLogin(home, exe, enabled); e != nil {
					return e
				}
			case "tray-at-login":
				if runtime.GOOS == "darwin" {
					return errors.New("the native tray app is available on Windows and Linux")
				}
				if e = trayLogin(home, exe, enabled); e != nil {
					return e
				}
			case "auto-update":
			default:
				return errors.New("unknown preference")
			}
			if e = config.Change(home, func(c *config.Config) error {
				switch args[0] {
				case "start-at-login":
					c.StartAtLogin = enabled
				case "menubar-at-login":
					c.MenuBarAtLogin = enabled
				case "tray-at-login":
					c.MenuBarAtLogin = enabled
				case "auto-update":
					c.AutoUpdate = enabled
				}
				return nil
			}); e != nil {
				return e
			}
			if args[0] == "auto-update" {
				if e = syncUpdateAgent(home, exe); e != nil {
					return fmt.Errorf("automatic update scheduling failed: %w", e)
				}
			}
		}
	}
	c, e := config.Load(home)
	if e != nil {
		return e
	}
	v := map[string]any{"start_at_login": c.StartAtLogin, "menubar_at_login": c.MenuBarAtLogin, "auto_update": c.AutoUpdate, "quota_refresh_minutes": c.QuotaRefreshMinutes}
	if runtime.GOOS != "darwin" {
		v["tray_at_login"] = c.MenuBarAtLogin
	}
	if asJSON {
		return printJSON(out, v)
	}
	name := "Menu bar"
	if runtime.GOOS != "darwin" {
		name = "Tray"
	}
	fmt.Fprintf(out, "Start at login: %t\n%s at login: %t\nAutomatic updates: %t\n", c.StartAtLogin, name, c.MenuBarAtLogin, c.AutoUpdate)
	if c.QuotaRefreshMinutes == 0 {
		fmt.Fprintln(out, "Usage refresh: on menu open only (no automatic refresh)")
	} else {
		fmt.Fprintf(out, "Usage refresh: every %d minutes (and on menu open)\n", c.QuotaRefreshMinutes)
	}
	return nil
}
func menuPath(exe string) string { return filepath.Join(filepath.Dir(exe), "Noria.app") }
