package config

import (
	"os"
	"path/filepath"
	"testing"
)

func TestQuotaRefreshDefaultExistingConfigAndValidation(t *testing.T) {
	home := t.TempDir()
	if err := Init(home); err != nil {
		t.Fatal(err)
	}
	c, err := Load(home)
	if err != nil || c.QuotaRefreshMinutes != 0 {
		t.Fatalf("new config default: %+v %v", c, err)
	}
	old := []byte(`{"listen":"127.0.0.1:43119","accounts":[],"auto_update":true}`)
	if err := Write(filepath.Join(home, "config.json"), old); err != nil {
		t.Fatal(err)
	}
	c, err = Load(home)
	if err != nil || c.QuotaRefreshMinutes != 0 {
		t.Fatalf("legacy config must default to on-open: %+v %v", c, err)
	}
	for _, minutes := range QuotaRefreshIntervals {
		c.QuotaRefreshMinutes = minutes
		if err := Save(home, c); err != nil {
			t.Fatal(err)
		}
		got, err := Load(home)
		if err != nil || got.QuotaRefreshMinutes != minutes {
			t.Fatalf("roundtrip %d failed: %+v %v", minutes, got, err)
		}
		prefs, err := ReadPreferences(home)
		if err != nil || prefs.QuotaRefreshMinutes != minutes {
			t.Fatalf("preferences lost %d: %+v %v", minutes, prefs, err)
		}
	}
	for _, invalid := range []int{-1, 1, 3, 20, 61, 181} {
		c.QuotaRefreshMinutes = invalid
		if err := Save(home, c); err != nil {
			t.Fatal(err)
		}
		if _, err := Load(home); err == nil {
			t.Fatalf("config accepted invalid interval %d", invalid)
		}
		if err := os.Remove(filepath.Join(home, "config.json")); err != nil {
			t.Fatal(err)
		}
		c.QuotaRefreshMinutes = 0
		if err := Save(home, c); err != nil {
			t.Fatal(err)
		}
	}
}

func TestSavePreferencesPreservesRefreshInterval(t *testing.T) {
	home := t.TempDir()
	if err := Init(home); err != nil {
		t.Fatal(err)
	}
	prefs, err := ReadPreferences(home)
	if err != nil {
		t.Fatal(err)
	}
	prefs.QuotaRefreshMinutes = 120
	if err := SavePreferences(home, prefs); err != nil {
		t.Fatal(err)
	}
	c, err := Load(home)
	if err != nil || c.QuotaRefreshMinutes != 120 {
		t.Fatalf("save preferences did not persist interval: %+v %v", c, err)
	}
	prefs.QuotaRefreshMinutes = 3
	if err := SavePreferences(home, prefs); err == nil {
		t.Fatal("invalid preference was stored")
	}
	c, err = Load(home)
	if err != nil || c.QuotaRefreshMinutes != 120 {
		t.Fatalf("invalid preference changed persisted value: %+v %v", c, err)
	}
}
