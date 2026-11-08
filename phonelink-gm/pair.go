package main

// Linking and unlinking from inside PhoneRelay.
//
//	phonelink-gm pair [--browser PATH]   sign in with Google, then tap the emoji on the phone
//	phonelink-gm unpair                  forget this computer on the phone and delete the session
//
// Both print JSON lines and exit 0 on success:
//
//	{"event":"browser"}                 a sign-in window opened (a temporary browser profile)
//	{"event":"checking"}                signed in; checking with Google Messages
//	{"event":"emoji", "emoji":"🦊"}     tap this emoji on the phone
//	{"event":"paired", "account":EMAIL} done; the session is saved
//	{"event":"unpaired"}                done (unpair)
//	{"event":"error", "error":KIND, "message"}
//	    KIND: no_browser, no_display, cancelled, wrong_emoji, timeout, busy, failed
//
// The browser part follows gmcli's (github.com/fdsouvenir/gmcli,
// internal/browserauth, AGPL-3.0): a visible Chromium-based browser on a
// throwaway profile, never the user's own, with Google's device-bound
// sessions off for that profile so the cookies can be reused.

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/chromedp/cdproto/network"
	"github.com/chromedp/cdproto/page"
	"github.com/chromedp/chromedp"
	"go.mau.fi/mautrix-gmessages/pkg/libgm"
)

const (
	gaiaLoginURL  = "https://accounts.google.com/AccountChooser?continue=https://messages.google.com/web/config"
	gaiaConfigURL = "https://messages.google.com/web/config"
)

// The Google cookies libgm needs, and their domains.
var gaiaCookies = map[string]string{
	"SID": "google.com", "HSID": "google.com", "SSID": "google.com",
	"APISID": "google.com", "SAPISID": "google.com",
	"OSID": "messages.google.com", "__Secure-1PSIDTS": "google.com",
}

type pairError struct{ kind, msg string }

func (e *pairError) Error() string { return e.msg }

func pairFail(kind, format string, a ...any) error {
	return &pairError{kind, fmt.Sprintf(format, a...)}
}

func say(v map[string]any) { _ = json.NewEncoder(os.Stdout).Encode(v) }

func sayError(err error) int {
	kind := "failed"
	var pe *pairError
	if errors.As(err, &pe) {
		kind = pe.kind
	}
	say(map[string]any{"event": "error", "error": kind, "message": err.Error()})
	return 1
}

// pair signs in with Google in a throwaway browser window, pairs with the
// phone by emoji, and saves the session at `session`.
func pair(session, browser string) int {
	if os.Getenv("DISPLAY") == "" && os.Getenv("WAYLAND_DISPLAY") == "" {
		return sayError(pairFail("no_display", "signing in needs a desktop display"))
	}
	path, err := findBrowser(browser)
	if err != nil {
		return sayError(err)
	}
	if err := os.MkdirAll(filepath.Dir(session), 0o700); err != nil {
		return sayError(err)
	}
	lk, err := lock(session)
	if err != nil {
		return sayError(pairFail("busy", "%v", err))
	}
	defer lk.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	say(map[string]any{"event": "browser"})
	cookies, err := signIn(ctx, path)
	if err != nil {
		return sayError(err)
	}
	say(map[string]any{"event": "checking"})

	auth := libgm.NewAuthData()
	auth.SetCookies(cookies)
	l := newLink(auth, session)
	defer l.cli.Disconnect()
	if err := l.cli.FetchConfig(ctx); err != nil {
		return sayError(pairFail("failed", "Google didn't accept the sign-in: %v", err))
	}
	account := l.cli.Config.GetDeviceInfo().GetEmail()
	// StartGaiaPairing doesn't watch ctx while it waits; don't hang on it.
	type started struct {
		emoji string
		ps    *libgm.PairingSession
		err   error
	}
	got := make(chan started, 1)
	go func() {
		e, ps, err := l.cli.StartGaiaPairing(ctx)
		got <- started{e, ps, err}
	}()
	var st started
	select {
	case st = <-got:
	case <-ctx.Done():
		return sayError(pairFail("timeout", "the phone didn't answer in time"))
	}
	if st.err != nil {
		return sayError(gaiaError(st.err))
	}
	say(map[string]any{"event": "emoji", "emoji": st.emoji})
	if _, err := l.cli.FinishGaiaPairing(ctx, st.ps); err != nil {
		return sayError(gaiaError(err))
	}
	if err := saveAuth(session, auth); err != nil {
		return sayError(pairFail("failed", "saving the link: %v", err))
	}
	say(map[string]any{"event": "paired", "account": account})
	return 0
}

func gaiaError(err error) error {
	switch {
	case errors.Is(err, libgm.ErrIncorrectEmoji):
		return pairFail("wrong_emoji", "a different emoji was tapped on the phone")
	case errors.Is(err, libgm.ErrPairingCancelled):
		return pairFail("cancelled", "linking was cancelled on the phone")
	case errors.Is(err, libgm.ErrPairingTimeout), errors.Is(err, context.DeadlineExceeded):
		return pairFail("timeout", "the emoji wasn't tapped in time")
	case errors.Is(err, libgm.ErrNoDevicesFound):
		return pairFail("failed", "no phone with Google Messages is signed in to this Google account (on the phone: Google Messages → Device pairing)")
	}
	return pairFail("failed", "%v", err)
}

// unpair tells the phone to forget this computer, then deletes the session
// (even when the phone can't be reached, so the app is unlinked either way).
func unpair(session string) int {
	lk, err := lock(session)
	if err != nil {
		return sayError(pairFail("busy", "%v", err))
	}
	defer lk.Close()
	auth, err := loadAuth(session)
	if err == nil {
		l := newLink(auth, session)
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		if err = l.connect(ctx); err == nil {
			err = l.cli.Unpair(ctx)
		}
		cancel()
		l.cli.Disconnect()
	}
	if rmErr := os.Remove(session); rmErr != nil && !os.IsNotExist(rmErr) {
		return sayError(pairFail("failed", "deleting the session: %v", rmErr))
	}
	if err != nil {
		// Unlinked here; the phone still lists this computer until removed there.
		say(map[string]any{"event": "unpaired", "phone": false, "message": err.Error()})
		return 0
	}
	say(map[string]any{"event": "unpaired", "phone": true})
	return 0
}

// signIn opens a visible browser on a throwaway profile at Google's sign-in
// and waits until it reaches Google Messages' config page, then returns
// the cookies libgm needs.
func signIn(ctx context.Context, browser string) (map[string]string, error) {
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	port := listener.Addr().(*net.TCPAddr).Port
	listener.Close()
	profile, err := os.MkdirTemp("", "phonelink-signin-")
	if err != nil {
		return nil, err
	}
	defer os.RemoveAll(profile)
	opts := []chromedp.ExecAllocatorOption{
		chromedp.ExecPath(browser), chromedp.UserDataDir(profile),
		chromedp.Env("TMPDIR="+profile, "TMP="+profile, "TEMP="+profile),
		chromedp.NoFirstRun, chromedp.NoDefaultBrowserCheck,
		chromedp.Flag("disable-features", "DeviceBoundSessions,EnableBoundSessionCredentials"),
		chromedp.Flag("disable-sync", true), chromedp.Flag("disable-extensions", true),
		chromedp.Flag("remote-debugging-address", "127.0.0.1"),
		chromedp.Flag("remote-debugging-port", strconv.Itoa(port)),
		chromedp.Flag("window-size", "960,800"),
	}
	allocCtx, cancelAlloc := chromedp.NewExecAllocator(ctx, opts...)
	defer cancelAlloc()
	bctx, cancelBrowser := chromedp.NewContext(allocCtx,
		chromedp.WithLogf(func(string, ...any) {}), chromedp.WithErrorf(func(string, ...any) {}))
	defer func() {
		if bctx.Err() == nil {
			c, cc := context.WithTimeout(bctx, 3*time.Second)
			_ = chromedp.Cancel(c)
			cc()
		}
		cancelBrowser()
	}()
	if err := chromedp.Run(bctx, chromedp.Navigate(gaiaLoginURL)); err != nil {
		return nil, browserFailed(ctx, "couldn't open Google's sign-in page")
	}
	tick := time.NewTicker(time.Second)
	defer tick.Stop()
	for {
		at, got, err := snapshot(bctx)
		if err != nil {
			return nil, browserFailed(ctx, "the sign-in window was closed")
		}
		if isConfigURL(at) {
			if c, ok := needCookies(got); ok {
				return c, nil
			}
		}
		select {
		case <-ctx.Done():
			return nil, browserFailed(ctx, "")
		case <-tick.C:
		}
	}
}

func browserFailed(ctx context.Context, msg string) error {
	if errors.Is(ctx.Err(), context.DeadlineExceeded) {
		return pairFail("timeout", "signing in took too long")
	}
	if msg == "" {
		msg = "signing in stopped"
	}
	// Browser errors can carry URLs and headers: never pass them on.
	return pairFail("cancelled", "%s", msg)
}

func isConfigURL(raw string) bool {
	u, err := url.Parse(raw)
	return err == nil && u.Scheme == "https" && u.Hostname() == "messages.google.com" &&
		(u.Port() == "" || u.Port() == "443") && u.User == nil && u.Path == "/web/config"
}

func snapshot(ctx context.Context) (string, map[string]string, error) {
	var at string
	var cookies []*network.Cookie
	err := chromedp.Run(ctx, chromedp.ActionFunc(func(ctx context.Context) error {
		tree, err := page.GetFrameTree().Do(ctx)
		if err != nil {
			return err
		}
		at = tree.Frame.URL
		if !isConfigURL(at) {
			return nil
		}
		cookies, err = network.GetCookies().WithURLs([]string{gaiaConfigURL}).Do(ctx)
		return err
	}))
	values := map[string]string{}
	for _, c := range cookies {
		if domain, ok := gaiaCookies[c.Name]; ok && strings.TrimPrefix(c.Domain, ".") == domain {
			values[c.Name] = c.Value
		}
	}
	return at, values, err
}

// needCookies keeps only the cookies libgm uses; ok once every required one is there.
func needCookies(in map[string]string) (map[string]string, bool) {
	out := map[string]string{}
	for name := range gaiaCookies {
		if v := strings.TrimSpace(in[name]); v != "" {
			out[name] = v
		} else if name != "__Secure-1PSIDTS" {
			return nil, false
		}
	}
	return out, true
}

// findBrowser: the browser given, else the first Chromium-based one installed.
func findBrowser(requested string) (string, error) {
	if requested != "" {
		p, err := exec.LookPath(requested)
		if err != nil {
			return "", pairFail("no_browser", "%s isn't installed", requested)
		}
		return p, nil
	}
	for _, name := range []string{"google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "brave-browser", "microsoft-edge", "microsoft-edge-stable", "vivaldi"} {
		if p, err := exec.LookPath(name); err == nil {
			return p, nil
		}
	}
	return "", pairFail("no_browser", "linking needs Chrome, Chromium, Brave, Edge or Vivaldi installed")
}
