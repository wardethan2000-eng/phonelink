package main

// serve: one lasting Google Messages connection for PhoneRelay.
//
// JSON lines. The app writes requests to stdin, and the helper answers on
// stdout with the same id ({"id", "ok": true, ...} or {"id", "ok": false,
// "error", "message"}):
//
//	{"id":1, "op":"mark_read", "chat":KEY}                 or "conv":ID (and "msg":ID, else the newest)
//	{"id":2, "op":"delete", "chats":[KEY], "dry_run":false} -> "results":[...] as the delete command
//	{"id":3, "op":"list"}                                  every chat again, as conversation events
//	{"id":4, "op":"fetch", "conv":ID, "count":50, "before_id":MSG, "before_ts_ms":T}
//	                                                       -> "messages":[...], "cursor":{"id","ts_ms"} (older page, if any)
//	{"id":5, "op":"send_text", "conv":ID, "text":"hi", "tmp_id":"tmp_123"}
//	                                                       -> ok when the phone took it; the message then
//	                                                          arrives as a message event with this tmp_id
//	{"id":8, "op":"send_media", "conv":ID, "path":"/tmp/a.jpg", "mime", "name", "text", "tmp_id"}
//	                                                       -> as send_text, with the file (text optional)
//	                                                       send_text and send_media take "reply_to":MSG (an RCS reply)
//	{"id":10, "op":"react", "conv":ID, "msg":MSG, "emoji":"👍", "action":"add"|"remove"|"switch"}
//	{"id":9, "op":"download", "media":ID, "key":KEY, "path":"/x/a.jpg"}
//	                                                       -> ok once the decrypted file is at path (0600)
//	{"id":11, "op":"typing", "conv":ID}                   "I'm typing" (the phone shows it for a few seconds)
//	{"id":12, "op":"update_chat", "conv":ID, "change":"archive"|"unarchive"|"mute"|"unmute"|"block"|"spam"|"unblock"}
//	                                                       -> ok when the phone took it; the chat's conversation
//	                                                          event follows ("spam" blocks and reports it)
//	{"id":6, "op":"set_active"}                            take the web session back from another browser
//	{"id":7, "op":"open_conversation", "numbers":["+13165551234"]}
//	                                                       -> "conv":ID of the chat with exactly them (made if new);
//	                                                          its conversation event comes first
//
// Events have no id:
//
//	{"event":"ready", "me":["3165550000"]}   connected (my own number keys); every chat follows, then
//	                                         any messages that arrived while connecting
//	{"event":"conversation", "chat":KEY, "conv":ID, "name", "members":[{"number","name"}],
//	 "group", "rcs", "read_only", "archived", "pinned", "blocked", "unread", "last_ts_ms", "preview"}
//	{"event":"message", "conv":ID, "msg":ID, "tmp_id", "ts_ms", "from_me", "from", "name",
//	 "text", "status", "rcs", "deleted", "old", "attachments":[{"id","mime","name","size","key","thumb","part"}],
//	 "reply_to":MSG, "reactions":[{"emoji","count","mine","by":[numbers]}]}
//	                                         (a reaction changing sends the message again; so does a
//	                                         picture whose full size ("thumb": only a preview) arrives)
//	{"event":"typing", "conv":ID, "from":NUMBER, "typing":true}   someone else in the chat started (or stopped) typing
//	{"event":"session", "state":"inactive"}  another browser took the web session
//	{"event":"phone", "responding":false}
//
// status is sending, sent, delivered, read, failed (mine) or received.
// A chat KEY is the delete command's match key (the other members' last 10
// digits, or a business chat's address, sorted, joined with commas); ID is
// Google's conversation id. Timestamps are ms. On a fatal error the helper
// prints {"event":"error", "error":"connect_failed"|"logged_out", "message"}
// and exits 1; the app restarts it. When stdin closes it exits 0.
//
// The helper never logs message text.

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"math/big"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"

	"go.mau.fi/mautrix-gmessages/pkg/libgm"
	"go.mau.fi/mautrix-gmessages/pkg/libgm/events"
	"go.mau.fi/mautrix-gmessages/pkg/libgm/gmproto"
)

type request struct {
	ID         int64    `json:"id"`
	Op         string   `json:"op"`
	Chat       string   `json:"chat,omitempty"`
	Chats      []string `json:"chats,omitempty"`
	DryRun     bool     `json:"dry_run,omitempty"`
	Conv       string   `json:"conv,omitempty"`
	Msg        string   `json:"msg,omitempty"`
	Text       string   `json:"text,omitempty"`
	TmpID      string   `json:"tmp_id,omitempty"`
	Count      int64    `json:"count,omitempty"`
	BeforeID   string   `json:"before_id,omitempty"`
	BeforeTSms int64    `json:"before_ts_ms,omitempty"`
	Numbers    []string `json:"numbers,omitempty"`
	Media      string   `json:"media,omitempty"`
	Key        []byte   `json:"key,omitempty"`
	Path       string   `json:"path,omitempty"`
	Mime       string   `json:"mime,omitempty"`
	Name       string   `json:"name,omitempty"`
	ReplyTo    string   `json:"reply_to,omitempty"`
	Emoji      string   `json:"emoji,omitempty"`
	Action     string   `json:"action,omitempty"`
	Change     string   `json:"change,omitempty"`
}

type cursor struct {
	ID   string `json:"id"`
	TSms int64  `json:"ts_ms"`
}

type reply struct {
	ID       int64        `json:"id"`
	OK       bool         `json:"ok"`
	Error    string       `json:"error,omitempty"`
	Message  string       `json:"message,omitempty"`
	Results  []chatResult `json:"results,omitempty"`
	Messages []msgEvent   `json:"messages,omitempty"`
	Cursor   *cursor      `json:"cursor,omitempty"`
	TmpID    string       `json:"tmp_id,omitempty"`
	Conv     string       `json:"conv,omitempty"`
}

// maxMedia is the largest file send_media takes (Google Messages' own limit
// for RCS files is about 100 MB).
const maxMedia = 100 << 20

// writePrivate writes data to path (mode 0600) through a temporary file, so
// a reader never sees half a picture.
func writePrivate(path string, data []byte) error {
	tmp := path + ".part"
	if err := os.WriteFile(tmp, data, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

func failed(id int64, kind string, format string, a ...any) reply {
	return reply{ID: id, Error: kind, Message: fmt.Sprintf(format, a...)}
}

// lines writes one JSON object per line; events come from libgm's
// goroutines, so writes are serialized. Events sent before open() are held
// and written right after the ready burst.
type lines struct {
	mu   sync.Mutex
	enc  *json.Encoder
	open bool
	held []any
}

func (w *lines) put(v any) {
	w.mu.Lock()
	defer w.mu.Unlock()
	_ = w.enc.Encode(v)
}

// event writes v now if the link is ready, else holds it.
func (w *lines) event(v any) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if !w.open {
		w.held = append(w.held, v)
		return
	}
	_ = w.enc.Encode(v)
}

// start writes first (the ready burst), then everything held, then opens.
func (w *lines) start(first []any) {
	w.mu.Lock()
	defer w.mu.Unlock()
	for _, v := range append(first, w.held...) {
		_ = w.enc.Encode(v)
	}
	w.held, w.open = nil, true
}

// chats is the helper's view of every conversation, kept current by
// conversation events, plus the phone's SIMs (for sending).
type chats struct {
	mu       sync.Mutex
	byID     map[string]*gmproto.Conversation
	sims     []*gmproto.SIMCard
	simsSeen chan struct{}
	inactive bool
}

func (c *chats) set(conv *gmproto.Conversation) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.byID[conv.GetConversationID()] = conv
}

func (c *chats) get(id string) *gmproto.Conversation {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.byID[id]
}

func (c *chats) all() []*gmproto.Conversation {
	c.mu.Lock()
	defer c.mu.Unlock()
	out := make([]*gmproto.Conversation, 0, len(c.byID))
	for _, conv := range c.byID {
		out = append(out, conv)
	}
	return out
}

func (c *chats) setSIMs(s *gmproto.Settings) {
	if s.GetSIMCards() == nil {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	first := c.sims == nil
	c.sims = s.GetSIMCards()
	if first {
		close(c.simsSeen)
	}
}

func (c *chats) getSIMs() []*gmproto.SIMCard {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.sims
}

func (c *chats) setInactive(v bool) (was bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	was, c.inactive = c.inactive, v
	return was
}

func newTmpID() string {
	n, _ := rand.Int(rand.Reader, big.NewInt(1_000_000_000_000))
	return fmt.Sprintf("tmp_%012d", n.Int64())
}

func serve(l *link, timeout time.Duration) int {
	out := &lines{enc: json.NewEncoder(os.Stdout)}
	state := &chats{byID: map[string]*gmproto.Conversation{}, simsSeen: make(chan struct{})}
	// askFullSize asks the phone, once per picture, for the full file of
	// one only a preview of is on Google's side (as the mautrix bridge does).
	var askedMu sync.Mutex
	asked := map[string]bool{}
	askFullSize := func(ev msgEvent) {
		for _, part := range thumbParts(ev) {
			askedMu.Lock()
			seen := asked[ev.Msg+"/"+part]
			asked[ev.Msg+"/"+part] = true
			askedMu.Unlock()
			if seen {
				continue
			}
			go func() {
				ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
				defer cancel()
				if _, err := l.cli.GetFullSizeImage(ctx, ev.Msg, part); err != nil {
					l.logger.Warn().Err(err).Msg("asking for a full-size picture")
				}
			}()
		}
	}
	l.onEvent = func(evt any) {
		switch e := evt.(type) {
		case *gmproto.Conversation:
			state.set(e)
			out.mu.Lock()
			open := out.open
			out.mu.Unlock()
			if !open {
				return // the ready burst sends every chat
			}
			if ev, ok := conversationEvent(e); ok {
				out.put(ev)
			}
		case *libgm.WrappedMessage:
			if ev, ok := messageEvent(e.Message, state.get(e.GetConversationID())); ok {
				ev.Old = e.IsOld
				out.event(ev)
				askFullSize(ev)
			}
		case *gmproto.TypingData:
			from := e.GetUser().GetNumber()
			if from == "" || myKeys(state.all())[phoneKey(from)] {
				return
			}
			out.event(map[string]any{"event": "typing", "conv": e.GetConversationID(), "from": from, "typing": e.GetType() == gmproto.TypingTypes_STARTED_TYPING})
		case *gmproto.Settings:
			state.setSIMs(e)
		case *events.BrowserActive:
			state.setInactive(true)
			out.event(map[string]any{"event": "session", "state": "inactive"})
		case *events.PhoneNotResponding:
			out.event(map[string]any{"event": "phone", "responding": false})
		case *events.PhoneRespondingAgain:
			out.event(map[string]any{"event": "phone", "responding": true})
		}
	}
	fatal := func(err error) int {
		kind := "connect_failed"
		if errors.Is(err, errLoggedOut) {
			kind = "logged_out"
		}
		out.put(map[string]any{"event": "error", "error": kind, "message": err.Error()})
		return 1
	}

	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	err := l.connect(ctx)
	if err == nil {
		var convs []*gmproto.Conversation
		if convs, err = l.listAll(ctx); err == nil {
			for _, c := range convs {
				state.set(c)
			}
		}
	}
	cancel()
	if err != nil {
		l.cli.Disconnect()
		return fatal(err)
	}
	var me []string
	for k := range myKeys(state.all()) {
		me = append(me, k)
	}
	sort.Strings(me)
	burst := []any{map[string]any{"event": "ready", "me": me}}
	for _, c := range state.all() {
		if ev, ok := conversationEvent(c); ok {
			burst = append(burst, ev)
		}
	}
	out.start(burst)

	refetch := func() {
		ctx, cancel := context.WithTimeout(context.Background(), timeout)
		defer cancel()
		if convs, err := l.listAll(ctx); err == nil {
			for _, c := range convs {
				state.set(c)
			}
		}
	}
	// conversation finds a chat by id, asking the phone if it's new.
	conversation := func(ctx context.Context, id string) *gmproto.Conversation {
		if c := state.get(id); c != nil {
			return c
		}
		if c, err := l.cli.GetConversation(ctx, id); err == nil && c != nil {
			state.set(c)
			return c
		}
		return nil
	}
	takeBack := func(ctx context.Context) error {
		if err := l.cli.SetActiveSession(ctx); err != nil {
			return err
		}
		state.setInactive(false)
		return nil
	}
	handle := func(req request) reply {
		ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
		defer cancel()
		switch req.Op {
		case "mark_read":
			var conv *gmproto.Conversation
			if req.Conv != "" {
				if conv = conversation(ctx, req.Conv); conv == nil {
					return failed(req.ID, "not_found", "no chat %s", req.Conv)
				}
			} else {
				keys, err := chatKeys([]string{req.Chat})
				if err != nil {
					return failed(req.ID, "usage", "%v", err)
				}
				var why string
				if conv, why = pick(state.all(), keys[0]); conv == nil {
					return failed(req.ID, why, "%s: no single Google Messages chat with exactly %s", why, keys[0])
				}
			}
			msg := req.Msg
			if msg == "" {
				msg = conv.GetLatestMessageID()
			}
			if msg == "" {
				return failed(req.ID, "no_message", "the chat has no messages")
			}
			if err := l.cli.MarkRead(ctx, conv.GetConversationID(), msg); err != nil {
				return failed(req.ID, "failed", "%v", err)
			}
			return reply{ID: req.ID, OK: true}
		case "delete":
			keys, err := chatKeys(req.Chats)
			if err != nil || len(keys) == 0 {
				return failed(req.ID, "usage", "no chats given %v", err)
			}
			r := deleteChats(ctx, l.cli, keys, req.DryRun, state.all, refetch)
			return reply{ID: req.ID, OK: r.OK, Results: r.Results}
		case "list":
			refetch()
			for _, c := range state.all() {
				if ev, ok := conversationEvent(c); ok {
					out.put(ev)
				}
			}
			return reply{ID: req.ID, OK: true}
		case "fetch":
			if req.Conv == "" {
				return failed(req.ID, "usage", "fetch needs conv")
			}
			count := req.Count
			if count <= 0 || count > 200 {
				count = 50
			}
			var cur *gmproto.Cursor
			if req.BeforeID != "" {
				// The cursor's timestamp is ms (seen on a real phone), unlike messages'.
				cur = &gmproto.Cursor{LastItemID: req.BeforeID, LastItemTimestamp: req.BeforeTSms}
			}
			resp, err := l.cli.FetchMessages(ctx, req.Conv, count, cur)
			if err != nil {
				return failed(req.ID, "failed", "%v", err)
			}
			conv := state.get(req.Conv)
			r := reply{ID: req.ID, OK: true, Messages: []msgEvent{}}
			for _, m := range resp.GetMessages() {
				if ev, ok := messageEvent(m, conv); ok {
					ev.Event = ""
					r.Messages = append(r.Messages, ev)
					askFullSize(ev)
				}
			}
			if c := resp.GetCursor(); c != nil && c.GetLastItemID() != "" {
				r.Cursor = &cursor{ID: c.GetLastItemID(), TSms: c.GetLastItemTimestamp()}
			}
			return r
		case "download":
			if req.Media == "" || len(req.Key) == 0 || req.Path == "" {
				return failed(req.ID, "usage", "download needs media, key and path")
			}
			data, err := l.cli.DownloadMedia(req.Media, req.Key)
			if err != nil {
				return failed(req.ID, "failed", "%v", err)
			}
			if err := writePrivate(req.Path, data); err != nil {
				return failed(req.ID, "failed", "%v", err)
			}
			return reply{ID: req.ID, OK: true}
		case "send_text", "send_media":
			if req.Conv == "" || (req.Text == "" && req.Op == "send_text") || (req.Path == "" && req.Op == "send_media") {
				return failed(req.ID, "usage", "send_text needs conv and text; send_media conv and path")
			}
			conv := conversation(ctx, req.Conv)
			if conv == nil {
				return failed(req.ID, "not_found", "no chat %s", req.Conv)
			}
			if conv.GetReadOnly() {
				return failed(req.ID, "read_only", "this chat can't be replied to")
			}
			// The user asked to send, so take the session back if another
			// browser has it.
			if state.setInactive(false) {
				if err := takeBack(ctx); err != nil {
					state.setInactive(true)
					return failed(req.ID, "inactive", "Google Messages is open in another browser: %v", err)
				}
			}
			// The phone sends its SIMs soon after connecting; wait a little.
			select {
			case <-state.simsSeen:
			case <-time.After(5 * time.Second):
			}
			tmp := req.TmpID
			if tmp == "" {
				tmp = newTmpID()
			}
			var media *gmproto.MediaContent
			if req.Op == "send_media" {
				data, err := os.ReadFile(req.Path)
				if err != nil {
					return failed(req.ID, "usage", "%v", err)
				}
				if len(data) > maxMedia {
					return failed(req.ID, "too_big", "%d MB is more than the phone takes", len(data)>>20)
				}
				name, mime := req.Name, req.Mime
				if name == "" {
					name = filepath.Base(req.Path)
				}
				if mime == "" {
					mime = http.DetectContentType(data)
				}
				if media, err = l.cli.UploadMedia(data, name, mime); err != nil {
					return failed(req.ID, "failed", "upload: %v", err)
				}
			}
			resp, err := l.cli.SendMessage(ctx, sendRequest(conv, state.getSIMs(), req.Text, media, tmp, req.ReplyTo))
			if err != nil {
				return failed(req.ID, "failed", "%v", err)
			}
			if resp.GetStatus() != gmproto.SendMessageResponse_SUCCESS {
				return failed(req.ID, "failed", "the phone answered %s", resp.GetStatus())
			}
			return reply{ID: req.ID, OK: true, TmpID: tmp}
		case "react":
			act, ok := reactionAction(req.Action)
			if req.Conv == "" || req.Msg == "" || req.Emoji == "" || !ok {
				return failed(req.ID, "usage", "react needs conv, msg, emoji and add/remove/switch")
			}
			conv := conversation(ctx, req.Conv)
			if conv == nil {
				return failed(req.ID, "not_found", "no chat %s", req.Conv)
			}
			if state.setInactive(false) {
				if err := takeBack(ctx); err != nil {
					state.setInactive(true)
					return failed(req.ID, "inactive", "Google Messages is open in another browser: %v", err)
				}
			}
			resp, err := l.cli.SendReaction(ctx, reactionRequest(conv, state.getSIMs(), req.Msg, req.Emoji, act))
			if err != nil {
				return failed(req.ID, "failed", "%v", err)
			}
			if !resp.GetSuccess() {
				return failed(req.ID, "refused", "the phone refused the reaction")
			}
			return reply{ID: req.ID, OK: true}
		case "typing":
			conv := conversation(ctx, req.Conv)
			if conv == nil {
				return failed(req.ID, "not_found", "no chat %s", req.Conv)
			}
			if state.setInactive(false) {
				if err := takeBack(ctx); err != nil {
					state.setInactive(true)
					return failed(req.ID, "inactive", "Google Messages is open in another browser: %v", err)
				}
			}
			sim, _ := chatSIM(conv, state.getSIMs())
			if err := l.cli.SetTyping(ctx, conv.GetConversationID(), sim.GetSIMData().GetSIMPayload()); err != nil {
				return failed(req.ID, "failed", "%v", err)
			}
			return reply{ID: req.ID, OK: true}
		case "update_chat":
			change, ok := chatChange(req.Conv, req.Change)
			if req.Conv == "" || !ok {
				return failed(req.ID, "usage", "update_chat needs conv and a change")
			}
			c := conversation(ctx, req.Conv)
			if c == nil {
				return failed(req.ID, "not_found", "no chat %s", req.Conv)
			}
			if state.setInactive(false) {
				if err := takeBack(ctx); err != nil {
					state.setInactive(true)
					return failed(req.ID, "inactive", "Google Messages is open in another browser: %v", err)
				}
			}
			tries := []*gmproto.UpdateConversationRequest{change}
			if change.GetAction() != gmproto.ConversationActionStatus_UNKNOWN_ACTION_STATUS {
				tries = blockTries(req.Conv, otherNumber(c), change.GetAction())
			}
			done := false
			for i, try := range tries {
				resp, err := l.cli.UpdateConversation(ctx, try)
				if err != nil {
					return failed(req.ID, "failed", "%v", err)
				}
				if resp.GetSuccess() {
					if len(tries) > 1 {
						fmt.Fprintf(os.Stderr, "update_chat %s: shape %d of %d accepted\n", req.Change, i+1, len(tries))
					}
					done = true
					break
				}
			}
			if !done {
				return failed(req.ID, "refused", "the phone refused that change (%d shapes tried)", len(tries))
			}
			// The phone usually sends the changed chat; send ours too.
			if c, err := l.cli.GetConversation(ctx, req.Conv); err == nil && c != nil {
				state.set(c)
				if ev, ok := conversationEvent(c); ok {
					out.put(ev)
				}
			}
			return reply{ID: req.ID, OK: true}
		case "open_conversation":
			if len(req.Numbers) == 0 {
				return failed(req.ID, "usage", "open_conversation needs numbers")
			}
			var nums []*gmproto.ContactNumber
			for _, n := range req.Numbers {
				// As the mautrix bridge starts chats.
				nums = append(nums, &gmproto.ContactNumber{MysteriousInt: 2, Number: n, Number2: n})
			}
			resp, err := l.cli.GetOrCreateConversation(ctx, &gmproto.GetOrCreateConversationRequest{Numbers: nums})
			if err != nil {
				return failed(req.ID, "failed", "%v", err)
			}
			conv := resp.GetConversation()
			if conv.GetConversationID() == "" {
				return failed(req.ID, "failed", "the phone made no chat (%s)", resp.GetStatus())
			}
			state.set(conv)
			if ev, ok := conversationEvent(conv); ok {
				out.put(ev)
			}
			return reply{ID: req.ID, OK: true, Conv: conv.GetConversationID()}
		case "set_active":
			if err := takeBack(ctx); err != nil {
				return failed(req.ID, "failed", "%v", err)
			}
			return reply{ID: req.ID, OK: true}
		default:
			return failed(req.ID, "usage", "unknown op %q", req.Op)
		}
	}

	stdin := make(chan request)
	go func() {
		sc := bufio.NewScanner(os.Stdin)
		sc.Buffer(make([]byte, 64*1024), 1024*1024)
		for sc.Scan() {
			var req request
			if err := json.Unmarshal(sc.Bytes(), &req); err != nil {
				out.put(reply{Error: "usage", Message: "bad request: " + err.Error()})
				continue
			}
			stdin <- req
		}
		close(stdin)
	}()
	for {
		select {
		case req, ok := <-stdin:
			if !ok {
				l.cli.Disconnect()
				return 0
			}
			go func() { out.put(handle(req)) }()
		case err := <-l.fatal:
			l.cli.Disconnect()
			return fatal(err)
		}
	}
}
