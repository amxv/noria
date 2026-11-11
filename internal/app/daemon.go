package app

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"sync"
	"syscall"
	"time"

	"github.com/amxv/noria/internal/config"
	"github.com/amxv/noria/internal/core"
	"github.com/amxv/noria/internal/integration"
	"github.com/amxv/noria/internal/model"
	"github.com/amxv/noria/internal/provider"
	"github.com/amxv/noria/internal/proxy"
	"github.com/amxv/noria/internal/service"
)

type runtimeInfo struct {
	Address string `json:"address"`
}

func daemonAddress(home, fallback string) string {
	var v runtimeInfo
	b, e := os.ReadFile(filepath.Join(home, "daemon.json"))
	if e == nil && json.Unmarshal(b, &v) == nil && config.ValidateListen(v.Address) == nil {
		return v.Address
	}
	return fallback
}

type refresher struct {
	mu   sync.Mutex
	home string
	s    *core.Scheduler
	p    *provider.Manager
}
type refreshResult struct {
	Account string `json:"account"`
	OK      bool   `json:"ok"`
	Error   string `json:"error,omitempty"`
}

func (r *refresher) refresh(ctx context.Context, id string) ([]refreshResult, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	c, e := config.Load(r.home)
	if e != nil {
		return nil, e
	}
	if id != "" {
		if _, e = findAccount(c, id); e != nil {
			return nil, e
		}
	}
	if e = r.s.SetAccounts(c.Accounts); e != nil {
		return nil, e
	}

	accounts := []model.Account{}
	for _, a := range c.Accounts {
		if id != "" && a.ID != id {
			continue
		}
		if !a.Enabled && !a.Authenticated && id == "" {
			continue
		}
		accounts = append(accounts, a)
	}
	results := make([]refreshResult, len(accounts))
	slots := make(chan struct{}, 4)
	var group sync.WaitGroup
	for i, a := range accounts {
		if ctx.Err() != nil {
			group.Wait()
			return nil, ctx.Err()
		}
		group.Add(1)
		go func(i int, a model.Account) {
			defer group.Done()
			select {
			case slots <- struct{}{}:
			case <-ctx.Done():
				results[i] = refreshResult{Account: a.ID, Error: "refresh cancelled"}
				return
			}
			defer func() { <-slots }()
			deadline, cancel := context.WithTimeout(ctx, 25*time.Second)
			defer cancel()
			if a.Email == "" {
				if identity, e := r.p.Identity(deadline, a); e == nil && identity.Authenticated && identity.Email != "" {
					_ = config.Change(r.home, func(current *config.Config) error {
						for index := range current.Accounts {
							if current.Accounts[index].ID == a.ID {
								current.Accounts[index].Email = identity.Email
								current.Accounts[index].Authenticated = true
							}
						}
						return nil
					})
				}
			}
			snap, e := r.p.Refresh(deadline, a)
			if e == nil {
				e = r.s.Update(snap)
			}
			result := refreshResult{Account: a.ID, OK: e == nil}
			if e != nil {
				result.Error = e.Error()
			}
			results[i] = result
		}(i, a)
	}
	group.Wait()
	if current, e := config.Load(r.home); e == nil {
		_ = r.s.SetAccounts(current.Accounts)
	}
	if err := recordQuotaRefresh(r.home, results); err != nil {
		return results, err
	}

	return results, nil
}

func serve(home string, args []string, out, errOut io.Writer) error {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	return serveWithContext(ctx, home, args, out, errOut)
}

func serveWithContext(parent context.Context, home string, args []string, out, errOut io.Writer) error {
	f := flags("serve", errOut)
	listen := f.String("listen", "", "numeric loopback address (default 127.0.0.1:43119)")
	if e := f.Parse(args); e != nil {
		if errors.Is(e, flag.ErrHelp) {
			return nil
		}
		return e
	}
	if f.NArg() != 0 {
		return fmt.Errorf("usage: noria serve [--listen address]")
	}
	c, e := config.Load(home)
	if e != nil {
		return e
	}
	if *listen == "" {
		*listen = c.Listen
	}
	if e = config.ValidateListen(*listen); e != nil {
		return e
	}
	secret, e := config.Secret(home)
	if e != nil {
		return e
	}
	unlock, e := service.Guard(home, "daemon")
	if e != nil {
		return e
	}
	defer unlock()
	s, e := core.New(statePath(home), c.Accounts)
	if e != nil {
		return e
	}
	p := provider.New(home)
	gateway := proxy.New(s, p, secret)
	ref := &refresher{home: home, s: s, p: p}
	ctx, stop := context.WithCancel(parent)
	defer stop()
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/_noria/refresh" && r.URL.Path != "/_noria/reload" && r.URL.Path != "/_noria/shutdown" {
			gateway.ServeHTTP(w, r)
			return
		}
		// Reuse the gateway's exact Host, Origin and authorization policy before
		// allowing a control mutation. Health does not spend quota or mutate state.
		probe := r.Clone(r.Context())
		probe.URL.Path = "/_noria/health"
		probe.Method = http.MethodGet
		check := &gateCheck{header: make(http.Header)}
		gateway.ServeHTTP(check, probe)
		if check.code != http.StatusOK {
			for k, v := range check.header {
				w.Header()[k] = v
			}
			w.WriteHeader(check.code)
			return
		}
		if r.Method != http.MethodPost {
			w.WriteHeader(http.StatusMethodNotAllowed)
			return
		}
		// The proxy gate has already compared credentials; keep a direct check here
		// so future public health endpoints cannot widen control authorization.
		if subtle.ConstantTimeCompare([]byte(r.Header.Get("Authorization")), []byte("Bearer "+secret)) != 1 {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		if r.URL.Path == "/_noria/shutdown" {
			w.Header().Set("Content-Type", "application/json")
			_, _ = io.WriteString(w, `{"ok":true}`)
			go func() { time.Sleep(50 * time.Millisecond); stop() }()
			return
		}
		if r.URL.Path == "/_noria/reload" {
			current, err := config.Load(home)
			if err == nil {
				err = s.SetAccounts(current.Accounts)
			}
			if err != nil {
				http.Error(w, "cannot reload accounts", 400)
				return
			}
			w.Header().Set("Content-Type", "application/json")
			_, _ = io.WriteString(w, `{"ok":true}`)
			return
		}
		body := http.MaxBytesReader(w, r.Body, 4096)
		defer body.Close()
		var v struct {
			Account string `json:"account"`
		}
		if json.NewDecoder(body).Decode(&v) != nil {
			http.Error(w, "invalid refresh request", 400)
			return
		}
		result, err := ref.refresh(r.Context(), v.Account)
		if err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(result)
	})
	l, e := net.Listen("tcp", *listen)
	if e != nil {
		return e
	}
	actual := l.Addr().String()
	b, _ := json.Marshal(runtimeInfo{Address: actual})
	if e = config.Write(filepath.Join(home, "daemon.json"), b); e != nil {
		l.Close()
		return e
	}
	defer os.Remove(filepath.Join(home, "daemon.json"))
	server := &http.Server{Handler: handler, ReadHeaderTimeout: 10 * time.Second, IdleTimeout: 90 * time.Second, MaxHeaderBytes: 64 << 10}
	errCh := make(chan error, 1)
	go func() {
		errCh <- server.Serve(&limitedListener{Listener: l, slots: make(chan struct{}, 128), done: ctx.Done()})
	}()
	var workers sync.WaitGroup
	workers.Add(1)
	go func() {
		defer workers.Done()
		ticker := time.NewTicker(time.Minute)
		defer ticker.Stop()
		for {
			_, _ = ref.refresh(ctx, "")
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
			}
		}
	}()
	exe, _ := service.Executable()
	if service.ReadIntent(home).Wanted || c.StartAtLogin {
		_ = service.SetIntent(home, exe, true)
		if err := integration.SetActive(home, exe, actual, true); err != nil {
			fmt.Fprintln(errOut, "Client integration needs attention:", err)
		}
	}
	if service.ReadIntent(home).Wanted {
		startAutomaticUpdates(ctx, home, &workers)
	}
	fmt.Fprintln(out, "Noria listening on http://"+actual)
	select {
	case <-ctx.Done():
	case e = <-errCh:
		if !errors.Is(e, http.ErrServerClosed) {
			stop()
		}
	}
	stop()
	shutdown, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_ = gateway.Shutdown(shutdown)
	_ = server.Shutdown(shutdown)
	_ = server.Close()
	workers.Wait()
	if e != nil && !errors.Is(e, http.ErrServerClosed) {
		return e
	}
	return nil
}

type gateCheck struct {
	header http.Header
	code   int
}

func (g *gateCheck) Header() http.Header { return g.header }
func (g *gateCheck) WriteHeader(s int)   { g.code = s }
func (g *gateCheck) Write(b []byte) (int, error) {
	if g.code == 0 {
		g.code = 200
	}
	return len(b), nil
}

type limitedListener struct {
	net.Listener
	slots chan struct{}
	done  <-chan struct{}
}

func (l *limitedListener) Accept() (net.Conn, error) {
	select {
	case l.slots <- struct{}{}:
	case <-l.done:
		return nil, net.ErrClosed
	}
	c, e := l.Listener.Accept()
	if e != nil {
		<-l.slots
		return nil, e
	}
	return &limitedConn{Conn: c, release: func() { <-l.slots }}, nil
}

type limitedConn struct {
	net.Conn
	once    sync.Once
	release func()
}

func (c *limitedConn) Close() error { e := c.Conn.Close(); c.once.Do(c.release); return e }
