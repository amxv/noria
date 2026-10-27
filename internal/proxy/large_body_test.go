package proxy

import (
	"crypto/sha256"
	"io"
	"net/http"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

func TestImageHistoryAbove64MiBForwardedVerbatim(t *testing.T) {
	for _, tc := range []struct {
		name, provider, route, prefix, suffix string
	}{
		{"codex", "codex", "/codex/v1/responses", `{"model":"test","input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,`, `"}]}]}`},
		{"codex-compact", "codex", "/codex/v1/responses/compact", `{"model":"test","input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,`, `"}]}]}`},
		{"claude", "claude", "/claude/v1/messages", `{"model":"test","messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"`, `"}}]}],"max_tokens":1}`},
	} {
		t.Run(tc.name, func(t *testing.T) {
			body := tc.prefix + strings.Repeat("A", 65<<20) + tc.suffix
			want := sha256.Sum256([]byte(body))
			_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				digest := sha256.New()
				n, err := io.Copy(digest, r.Body)
				if err != nil || n != int64(len(body)) || string(digest.Sum(nil)) != string(want[:]) {
					t.Error("large image history changed in transit")
				}
				w.Header().Set("Content-Type", "application/json")
				_, _ = io.WriteString(w, `{"id":"ok"}`)
			}), tc.provider, 1)
			response := post(t, local.URL+tc.route, body)
			got := read(t, response)
			if response.StatusCode != http.StatusOK || got != `{"id":"ok"}` {
				t.Fatalf("large image history rejected: %d %s", response.StatusCode, got)
			}
		})
	}
}

func TestWebSocketImageHistoryAbove64MiBForwardedVerbatim(t *testing.T) {
	body := `{"type":"response.create","model":"test","input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,` + strings.Repeat("A", 65<<20) + `"}]}]}`
	want := sha256.Sum256([]byte(body))
	_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := (&websocket.Upgrader{}).Upgrade(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.Close()
		_ = conn.SetReadDeadline(time.Now().Add(15 * time.Second))
		_, message, err := conn.NextReader()
		if err != nil {
			t.Error(err)
			return
		}
		digest := sha256.New()
		n, err := io.Copy(digest, message)
		if err != nil || n != int64(len(body)) || string(digest.Sum(nil)) != string(want[:]) {
			t.Error("large WebSocket image history changed in transit")
			return
		}
		_ = conn.WriteMessage(websocket.TextMessage, []byte(`{"type":"response.completed","response":{"id":"ok"}}`))
	}), "codex", 1)
	conn, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(local.URL, "http")+"/codex/v1/responses", http.Header{"Authorization": {"Bearer local-secret"}})
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	_ = conn.SetReadDeadline(time.Now().Add(15 * time.Second))
	if err := conn.WriteMessage(websocket.TextMessage, []byte(body)); err != nil {
		t.Fatal(err)
	}
	_, got, err := conn.ReadMessage()
	if err != nil || !strings.Contains(string(got), `"response.completed"`) {
		t.Fatalf("large WebSocket image history rejected: %v %s", err, got)
	}
}
