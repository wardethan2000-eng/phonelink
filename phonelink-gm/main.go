// phonelink-gm deletes a conversation on the phone through the Google
// Messages web link (libgm), using the session paired by gmcli.
//
// KDE Connect can only read texts; only the default SMS app (Google
// Messages) can delete them. Phone Link runs this helper when a chat is
// deleted on the desktop.
//
//	phonelink-gm status
//	phonelink-gm delete [--dry-run] CHAT...
//
// Each CHAT is the chat's other members' numbers, comma-separated
// ("+13165551234" or "+13165551234,+13165555678"). It must match exactly one
// Google Messages conversation, or it is left alone.
//
// Prints one JSON object; "results" has one entry per CHAT, in order. Exit 0
// when every chat was deleted. "error" is one of: not_paired, busy,
// connect_failed (whole run), not_found, ambiguous, delete_failed (per chat).
//
// Licensed AGPL-3.0-or-later because it links libgm (mautrix-gmessages).
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/rs/zerolog"
	"go.mau.fi/mautrix-gmessages/pkg/libgm"
	"go.mau.fi/mautrix-gmessages/pkg/libgm/events"
	"go.mau.fi/mautrix-gmessages/pkg/libgm/gmproto"
)

type chatResult struct {
	Chat           string `json:"chat"`
	OK             bool   `json:"ok"`
	Error          string `json:"error,omitempty"`
	Message        string `json:"message,omitempty"`
	ConversationID string `json:"conversation_id,omitempty"`
}

type result struct {
	OK      bool         `json:"ok"`
	Paired  bool         `json:"paired,omitempty"`
	Error   string       `json:"error,omitempty"`
	Message string       `json:"message,omitempty"`
	DryRun  bool         `json:"dry_run,omitempty"`
	Results []chatResult `json:"results,omitempty"`
}

func finish(r result) {
	_ = json.NewEncoder(os.Stdout).Encode(r)
	if r.OK {
		os.Exit(0)
	}
	os.Exit(1)
}

func fail(code, format string, a ...any) {
	finish(result{Error: code, Message: fmt.Sprintf(format, a...)})
}

func defaultSession() string {
	if p := os.Getenv("PHONELINK_GM_SESSION"); p != "" {
		return p
	}
	state := os.Getenv("XDG_STATE_HOME")
	if state == "" {
		home, _ := os.UserHomeDir()
		state = filepath.Join(home, ".local", "state")
	}
	return filepath.Join(state, "gmcli", "session.json")
}

func loadAuth(path string) (*libgm.AuthData, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var auth libgm.AuthData
	if err := json.Unmarshal(b, &auth); err != nil {
		return nil, err
	}
	if auth.Browser == nil {
		return nil, errors.New("session has no paired device")
	}
	return &auth, nil
}

// saveAuth writes refreshed tokens back the way gmcli does (temp + rename),
// so gmcli and this helper keep sharing one valid session.
func saveAuth(path string, auth *libgm.AuthData) error {
	f, err := os.CreateTemp(filepath.Dir(path), ".session-*.tmp")
	if err != nil {
		return err
	}
	tmp := f.Name()
	defer os.Remove(tmp)
	enc := json.NewEncoder(f)
	enc.SetIndent("", "  ")
	if err := enc.Encode(auth); err != nil {
		f.Close()
		return err
	}
	if err := f.Chmod(0o600); err != nil {
		f.Close()
		return err
	}
	if err := f.Close(); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

var nonDigit = regexp.MustCompile(`\D`)

// phoneKey: last 10 digits (or all when shorter), like Phone Link's.
func phoneKey(s string) string {
	d := nonDigit.ReplaceAllString(s, "")
	if len(d) > 10 {
		d = d[len(d)-10:]
	}
	return d
}

func keySet(numbers []string) string {
	var keys []string
	seen := map[string]bool{}
	for _, n := range numbers {
		k := phoneKey(n)
		if k == "" || seen[k] {
			continue
		}
		seen[k] = true
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return strings.Join(keys, ",")
}

// others: the numbers of everyone in the conversation but me. Google
// Messages can list my own number a second time without isMe (RCS), so any
// participant with one of my numbers is me too.
func others(conv *gmproto.Conversation) []string {
	number := func(p *gmproto.Participant) string {
		if n := p.GetID().GetNumber(); n != "" {
			return n
		}
		return p.GetFormattedNumber()
	}
	mine := map[string]bool{}
	for _, p := range conv.GetParticipants() {
		if p.GetIsMe() {
			if k := phoneKey(number(p)); k != "" {
				mine[k] = true
			}
		}
	}
	var out []string
	for _, p := range conv.GetParticipants() {
		n := number(p)
		if p.GetIsMe() || n == "" || mine[phoneKey(n)] {
			continue
		}
		out = append(out, n)
	}
	return out
}

// pick finds the one conversation whose members are exactly `want`.
func pick(convs []*gmproto.Conversation, want string) (*gmproto.Conversation, string) {
	var found *gmproto.Conversation
	for _, c := range convs {
		if c.GetStatus() == gmproto.ConversationStatus_DELETED {
			continue
		}
		if keySet(others(c)) != want {
			continue
		}
		if found != nil && found.GetConversationID() != c.GetConversationID() {
			return nil, "ambiguous"
		}
		found = c
	}
	if found == nil {
		return nil, "not_found"
	}
	return found, ""
}

func lock(sessionPath string) (*os.File, error) {
	f, err := os.OpenFile(filepath.Join(filepath.Dir(sessionPath), ".phonelink-gm.lock"), os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, err
	}
	deadline := time.Now().Add(20 * time.Second)
	for {
		if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err == nil {
			return f, nil
		}
		if time.Now().After(deadline) {
			f.Close()
			return nil, errors.New("another phonelink-gm is running")
		}
		time.Sleep(250 * time.Millisecond)
	}
}

func main() {
	if len(os.Args) < 2 {
		fail("usage", "usage: phonelink-gm status | delete [--dry-run] NUMBER...")
	}
	cmd := os.Args[1]
	fs := flag.NewFlagSet(cmd, flag.ContinueOnError)
	session := fs.String("session", defaultSession(), "gmcli session.json")
	dryRun := fs.Bool("dry-run", false, "find the conversation but do not delete it")
	timeout := fs.Duration("timeout", 60*time.Second, "give up after this long")
	if err := fs.Parse(os.Args[2:]); err != nil {
		fail("usage", "%v", err)
	}

	auth, err := loadAuth(*session)
	if cmd == "status" {
		finish(result{OK: true, Paired: err == nil})
	}
	if cmd != "delete" {
		fail("usage", "unknown command %q", cmd)
	}
	if err != nil {
		fail("not_paired", "Google Messages is not paired: %v", err)
	}
	var chats []string
	for _, arg := range fs.Args() {
		k := keySet(strings.Split(arg, ","))
		if k == "" {
			fail("usage", "chat %q has no phone number", arg)
		}
		chats = append(chats, k)
	}
	if len(chats) == 0 {
		fail("usage", "no chats given")
	}

	lk, err := lock(*session)
	if err != nil {
		fail("busy", "%v", err)
	}
	defer lk.Close()

	logger := zerolog.New(os.Stderr).Level(zerolog.WarnLevel)
	cli := libgm.NewClient(auth, nil, logger)
	ready := make(chan []*gmproto.Conversation, 1)
	fatal := make(chan error, 1)
	var once sync.Once
	cli.SetEventHandler(func(evt any) {
		switch e := evt.(type) {
		case *events.AuthTokenRefreshed:
			if err := saveAuth(*session, auth); err != nil {
				logger.Error().Err(err).Msg("save refreshed session")
			}
		case *events.ClientReady:
			once.Do(func() { ready <- e.Conversations })
		case *events.ListenFatalError:
			select {
			case fatal <- e.Error:
			default:
			}
		case *events.GaiaLoggedOut:
			select {
			case fatal <- errors.New("Google account signed out of Google Messages"):
			default:
			}
		}
	})
	ctx, cancel := context.WithTimeout(context.Background(), *timeout)
	defer cancel()
	if err := cli.Connect(); err != nil {
		fail("connect_failed", "connect: %v", err)
	}
	defer cli.Disconnect()

	// Wait for the long-poll the way gmcli does, then make this the
	// active session so the phone answers requests.
	for !cli.IsConnected() {
		select {
		case err := <-fatal:
			fail("connect_failed", "%v", err)
		case <-ctx.Done():
			fail("connect_failed", "the phone did not answer in %s", *timeout)
		case <-time.After(100 * time.Millisecond):
		}
	}
	if err := cli.SetActiveSession(ctx); err != nil {
		fail("connect_failed", "set active session: %v", err)
	}
	var convs []*gmproto.Conversation
	select {
	case convs = <-ready: // the newest chats, if the phone sends them quickly
	case <-time.After(3 * time.Second):
	}
	// ClientReady carries only the newest chats; ask for more if needed.
	fetched := false
	fetchAll := func() {
		if fetched {
			return
		}
		fetched = true
		for _, folder := range []gmproto.ListConversationsRequest_Folder{
			gmproto.ListConversationsRequest_INBOX, gmproto.ListConversationsRequest_ARCHIVE,
		} {
			resp, err := cli.ListConversations(ctx, 1000, folder)
			if err != nil {
				fail("connect_failed", "list conversations: %v", err)
			}
			convs = append(convs, resp.GetConversations()...)
		}
	}
	out := result{OK: true, DryRun: *dryRun}
	for _, want := range chats {
		conv, why := pick(convs, want)
		if conv == nil {
			fetchAll()
			conv, why = pick(convs, want)
		}
		r := chatResult{Chat: want}
		switch {
		case conv == nil:
			r.Error, r.Message = why, fmt.Sprintf("%s: no single Google Messages chat with exactly %s", why, want)
		case *dryRun:
			r.OK, r.ConversationID = true, conv.GetConversationID()
		default:
			r.ConversationID = conv.GetConversationID()
			if err := cli.DeleteConversation(ctx, conv.GetConversationID(), others(conv)[0]); err != nil {
				r.Error, r.Message = "delete_failed", err.Error()
			} else {
				r.OK = true
			}
		}
		out.OK = out.OK && r.OK
		out.Results = append(out.Results, r)
	}
	finish(out)
}
