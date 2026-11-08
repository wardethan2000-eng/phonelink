// phonelink-gm is PhoneRelay's Google Messages link (libgm): the lasting
// `serve` connection (serve.go), linking (pair.go), and deleting chats.
// The session lives at ~/.local/share/phonelink/gm/session.json; a
// pairing gmcli made earlier is copied there once.
//
// KDE Connect can only read texts; only the default SMS app (Google
// Messages) can delete them. PhoneRelay runs this helper when a chat is
// deleted on the desktop.
//
//	phonelink-gm status
//	phonelink-gm serve            (serve.go)
//	phonelink-gm pair | unpair    (pair.go)
//	phonelink-gm delete [--dry-run] CHAT...
//	phonelink-gm members          (debugging: every chat's members, JSON lines)
//
// Each CHAT is the chat's other members' numbers, comma-separated
// ("+13165551234" or "+13165551234,+13165555678"), or a business chat's
// address ("name_agent@rbm.goog"). It must match exactly one
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

// defaultSession: PhoneRelay's own session file. A pairing gmcli made
// before is copied in once (a marker remembers it, so unlinking stays
// unlinked).
func defaultSession() string {
	if p := os.Getenv("PHONELINK_GM_SESSION"); p != "" {
		return p
	}
	own := ownSession()
	if _, err := os.Stat(own); err == nil {
		return own
	}
	marker := filepath.Join(filepath.Dir(own), ".from-gmcli")
	if _, err := os.Stat(marker); err == nil {
		return own
	}
	if b, err := os.ReadFile(gmcliSession()); err == nil {
		if os.MkdirAll(filepath.Dir(own), 0o700) == nil && writePrivate(own, b) == nil {
			_ = os.WriteFile(marker, nil, 0o600)
		}
	}
	return own
}

func ownSession() string {
	data := os.Getenv("XDG_DATA_HOME")
	if data == "" {
		home, _ := os.UserHomeDir()
		data = filepath.Join(home, ".local", "share")
	}
	return filepath.Join(data, "phonelink", "gm", "session.json")
}

func gmcliSession() string {
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

// phoneKey: last 10 digits (or all when shorter), like PhoneRelay's.
// Business chats (RCS agents such as "name_agent@rbm.goog") and email
// addresses have no phone number: they are matched by the whole address.
func phoneKey(s string) string {
	if strings.Contains(s, "@") {
		return strings.ToLower(strings.TrimSpace(s))
	}
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

// myKeys: the keys of my own numbers, as Google Messages marks them.
func myKeys(convs []*gmproto.Conversation) map[string]bool {
	mine := map[string]bool{}
	for _, c := range convs {
		for _, p := range c.GetParticipants() {
			if p.GetIsMe() {
				if k := phoneKey(p.GetID().GetNumber()); k != "" {
					mine[k] = true
				}
			}
		}
	}
	return mine
}

// withoutMe drops my own numbers from a chat key (callers may not know
// them), unless nothing else is left.
func withoutMe(key string, mine map[string]bool) string {
	var keep []string
	for _, k := range strings.Split(key, ",") {
		if !mine[k] {
			keep = append(keep, k)
		}
	}
	if len(keep) == 0 {
		return key
	}
	return strings.Join(keep, ",")
}

// pick finds the one conversation whose members are exactly `want`.
func pick(convs []*gmproto.Conversation, want string) (*gmproto.Conversation, string) {
	want = withoutMe(want, myKeys(convs))
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

// link is one libgm connection with the session kept up to date. Extra
// events (serve mode) go to onEvent.
type link struct {
	cli     *libgm.Client
	auth    *libgm.AuthData
	session string
	logger  zerolog.Logger
	ready   chan []*gmproto.Conversation
	fatal   chan error
	onEvent func(evt any)
}

func newLink(auth *libgm.AuthData, session string) *link {
	l := &link{
		auth:    auth,
		session: session,
		logger:  zerolog.New(redactWriter{os.Stderr}).With().Timestamp().Logger().Level(zerolog.WarnLevel),
		ready:   make(chan []*gmproto.Conversation, 1),
		fatal:   make(chan error, 1),
	}
	l.cli = libgm.NewClient(auth, nil, l.logger)
	var once sync.Once
	l.cli.SetEventHandler(func(evt any) {
		switch e := evt.(type) {
		case *events.AuthTokenRefreshed:
			if err := saveAuth(l.session, l.auth); err != nil {
				l.logger.Error().Err(err).Msg("save refreshed session")
			}
		case *events.ClientReady:
			once.Do(func() { l.ready <- e.Conversations })
		case *events.ListenFatalError:
			l.die(e.Error)
		case *events.GaiaLoggedOut:
			l.die(errLoggedOut)
		}
		if l.onEvent != nil {
			l.onEvent(evt)
		}
	})
	return l
}

var errLoggedOut = errors.New("Google account signed out of Google Messages")

func (l *link) die(err error) {
	select {
	case l.fatal <- err:
	default:
	}
}

// connect opens the long poll the way gmcli does, then makes this the
// active session so the phone answers requests.
func (l *link) connect(ctx context.Context) error {
	if err := l.cli.Connect(); err != nil {
		return fmt.Errorf("connect: %w", err)
	}
	for !l.cli.IsConnected() {
		select {
		case err := <-l.fatal:
			return err
		case <-ctx.Done():
			return errors.New("the phone did not answer in time")
		case <-time.After(100 * time.Millisecond):
		}
	}
	if err := l.cli.SetActiveSession(ctx); err != nil {
		return fmt.Errorf("set active session: %w", err)
	}
	return nil
}

// listAll: every chat in the inbox and the archive.
func (l *link) listAll(ctx context.Context) ([]*gmproto.Conversation, error) {
	var convs []*gmproto.Conversation
	for _, folder := range []gmproto.ListConversationsRequest_Folder{
		gmproto.ListConversationsRequest_INBOX, gmproto.ListConversationsRequest_ARCHIVE,
	} {
		resp, err := l.cli.ListConversations(ctx, 1000, folder)
		if err != nil {
			return nil, fmt.Errorf("list conversations: %w", err)
		}
		convs = append(convs, resp.GetConversations()...)
	}
	return convs, nil
}

// deleteChats deletes (or, dry, finds) each chat key's conversation. find
// is asked first; refetch is called once if a chat isn't found.
func deleteChats(ctx context.Context, cli *libgm.Client, keys []string, dry bool, find func() []*gmproto.Conversation, refetch func()) result {
	out := result{OK: true, DryRun: dry}
	refetched := false
	for _, want := range keys {
		conv, why := pick(find(), want)
		if conv == nil && !refetched {
			refetched = true
			refetch()
			conv, why = pick(find(), want)
		}
		r := chatResult{Chat: want}
		switch {
		case conv == nil:
			r.Error, r.Message = why, fmt.Sprintf("%s: no single Google Messages chat with exactly %s", why, want)
		case dry:
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
	return out
}

// chatKeys turns CHAT arguments into match keys.
func chatKeys(args []string) ([]string, error) {
	var keys []string
	for _, arg := range args {
		k := keySet(strings.Split(arg, ","))
		if k == "" {
			return nil, fmt.Errorf("chat %q has no phone number or address", arg)
		}
		keys = append(keys, k)
	}
	return keys, nil
}

func main() {
	if len(os.Args) < 2 {
		fail("usage", "usage: phonelink-gm status | serve | pair | unpair | members | delete [--dry-run] CHAT...")
	}
	cmd := os.Args[1]
	fs := flag.NewFlagSet(cmd, flag.ContinueOnError)
	session := fs.String("session", defaultSession(), "gmcli session.json")
	dryRun := fs.Bool("dry-run", false, "find the conversation but do not delete it")
	timeout := fs.Duration("timeout", 60*time.Second, "give up after this long (serve: connecting only)")
	browser := fs.String("browser", "", "pair: the Chromium-based browser to sign in with")
	if err := fs.Parse(os.Args[2:]); err != nil {
		fail("usage", "%v", err)
	}

	switch cmd {
	case "pair":
		os.Exit(pair(*session, *browser))
	case "unpair":
		os.Exit(unpair(*session))
	}
	auth, err := loadAuth(*session)
	if cmd == "status" {
		finish(result{OK: true, Paired: err == nil})
	}
	if cmd != "delete" && cmd != "members" && cmd != "serve" {
		fail("usage", "unknown command %q", cmd)
	}
	if err != nil {
		fail("not_paired", "Google Messages is not paired: %v", err)
	}
	var chats []string
	if cmd == "delete" {
		if chats, err = chatKeys(fs.Args()); err != nil {
			fail("usage", "%v", err)
		}
		if len(chats) == 0 {
			fail("usage", "no chats given")
		}
	}

	lk, err := lock(*session)
	if err != nil {
		fail("busy", "%v", err)
	}
	defer lk.Close()

	l := newLink(auth, *session)
	if cmd == "serve" {
		os.Exit(serve(l, *timeout))
	}
	ctx, cancel := context.WithTimeout(context.Background(), *timeout)
	defer cancel()
	defer l.cli.Disconnect()
	if err := l.connect(ctx); err != nil {
		fail("connect_failed", "%v", err)
	}
	var convs []*gmproto.Conversation
	select {
	case convs = <-l.ready: // the newest chats, if the phone sends them quickly
	case <-time.After(3 * time.Second):
	}
	// ClientReady carries only the newest chats; ask for more if needed.
	fetchAll := func() {
		all, err := l.listAll(ctx)
		if err != nil {
			fail("connect_failed", "%v", err)
		}
		convs = append(convs, all...)
	}
	if cmd == "members" {
		fetchAll()
		enc := json.NewEncoder(os.Stdout)
		for _, c := range convs {
			var ps []map[string]any
			for _, p := range c.GetParticipants() {
				ps = append(ps, map[string]any{"number": p.GetID().GetNumber(), "formatted": p.GetFormattedNumber(), "me": p.GetIsMe(), "pid": p.GetID().GetParticipantID()})
			}
			_ = enc.Encode(map[string]any{"id": c.GetConversationID(), "name": c.GetName(), "unread": c.GetUnread(), "status": c.GetStatus().String(), "others": others(c), "participants": ps})
		}
		os.Exit(0)
	}
	finish(deleteChats(ctx, l.cli, chats, *dryRun, func() []*gmproto.Conversation { return convs }, fetchAll))
}
