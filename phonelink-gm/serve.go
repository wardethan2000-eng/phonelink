package main

// serve: one lasting Google Messages connection for PhoneRelay.
//
// JSON lines. The app writes requests to stdin:
//
//	{"id": 1, "op": "mark_read", "chat": "3165551234"}
//	{"id": 2, "op": "delete", "chats": ["3165551234"], "dry_run": false}
//
// and the helper answers on stdout with the same id:
//
//	{"id": 1, "ok": true}
//	{"id": 2, "ok": true, "results": [...]}      (as the delete command)
//	{"id": 3, "ok": false, "error": "not_found", "message": "..."}
//
// Events have no id:
//
//	{"event": "ready", "me": ["3165550000"]}        connected (my own number keys); every chat follows
//	{"event": "conversation", "chat": KEY, "unread": false, "last_ts_ms": 1790000000000}
//	{"event": "session", "state": "inactive"}       another browser took the web session
//	{"event": "phone", "responding": false}
//
// A chat KEY is the same match key the delete command uses (the other
// members' last 10 digits, or a business chat's address, sorted, joined
// with commas). On a fatal error the helper prints
// {"event": "error", "error": "connect_failed" | "logged_out", "message"}
// and exits 1; the app restarts it. When stdin closes it exits 0.

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"sort"
	"sync"
	"time"

	"go.mau.fi/mautrix-gmessages/pkg/libgm/events"
	"go.mau.fi/mautrix-gmessages/pkg/libgm/gmproto"
)

type request struct {
	ID     int64    `json:"id"`
	Op     string   `json:"op"`
	Chat   string   `json:"chat,omitempty"`
	Chats  []string `json:"chats,omitempty"`
	DryRun bool     `json:"dry_run,omitempty"`
}

type reply struct {
	ID      int64        `json:"id"`
	OK      bool         `json:"ok"`
	Error   string       `json:"error,omitempty"`
	Message string       `json:"message,omitempty"`
	Results []chatResult `json:"results,omitempty"`
}

type convEvent struct {
	Event    string `json:"event"`
	Chat     string `json:"chat"`
	Unread   bool   `json:"unread"`
	LastTSms int64  `json:"last_ts_ms"`
}

// lines writes one JSON object per line; events come from libgm's
// goroutines, so writes are serialized.
type lines struct {
	mu  sync.Mutex
	enc *json.Encoder
}

func (w *lines) put(v any) {
	w.mu.Lock()
	defer w.mu.Unlock()
	_ = w.enc.Encode(v)
}

// chats is the helper's view of every conversation, kept current by
// conversation events.
type chats struct {
	mu   sync.Mutex
	byID map[string]*gmproto.Conversation
}

func (c *chats) set(conv *gmproto.Conversation) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.byID[conv.GetConversationID()] = conv
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

func conversationEvent(conv *gmproto.Conversation) (convEvent, bool) {
	key := keySet(others(conv))
	if key == "" || conv.GetStatus() == gmproto.ConversationStatus_DELETED {
		return convEvent{}, false
	}
	// Google's timestamps are microseconds.
	return convEvent{Event: "conversation", Chat: key, Unread: conv.GetUnread(), LastTSms: conv.GetLastMessageTimestamp() / 1000}, true
}

func serve(l *link, timeout time.Duration) int {
	out := &lines{enc: json.NewEncoder(os.Stdout)}
	state := &chats{byID: map[string]*gmproto.Conversation{}}
	ready := make(chan struct{})
	l.onEvent = func(evt any) {
		switch e := evt.(type) {
		case *gmproto.Conversation:
			state.set(e)
			select {
			case <-ready: // before ready, the full list is sent instead
			default:
				return
			}
			if ev, ok := conversationEvent(e); ok {
				out.put(ev)
			}
		case *events.BrowserActive:
			out.put(map[string]any{"event": "session", "state": "inactive"})
		case *events.PhoneNotResponding:
			out.put(map[string]any{"event": "phone", "responding": false})
		case *events.PhoneRespondingAgain:
			out.put(map[string]any{"event": "phone", "responding": true})
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
	close(ready)
	var me []string
	for k := range myKeys(state.all()) {
		me = append(me, k)
	}
	sort.Strings(me)
	out.put(map[string]any{"event": "ready", "me": me})
	for _, c := range state.all() {
		if ev, ok := conversationEvent(c); ok {
			out.put(ev)
		}
	}

	refetch := func() {
		ctx, cancel := context.WithTimeout(context.Background(), timeout)
		defer cancel()
		if convs, err := l.listAll(ctx); err == nil {
			for _, c := range convs {
				state.set(c)
			}
		}
	}
	handle := func(req request) reply {
		ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
		defer cancel()
		switch req.Op {
		case "mark_read":
			keys, err := chatKeys([]string{req.Chat})
			if err != nil {
				return reply{ID: req.ID, Error: "usage", Message: err.Error()}
			}
			key := keys[0]
			conv, why := pick(state.all(), key)
			if conv == nil {
				return reply{ID: req.ID, Error: why, Message: fmt.Sprintf("%s: no single Google Messages chat with exactly %s", why, key)}
			}
			msg := conv.GetLatestMessageID()
			if msg == "" {
				return reply{ID: req.ID, Error: "no_message", Message: "the chat has no messages"}
			}
			if err := l.cli.MarkRead(ctx, conv.GetConversationID(), msg); err != nil {
				return reply{ID: req.ID, Error: "failed", Message: err.Error()}
			}
			return reply{ID: req.ID, OK: true}
		case "delete":
			keys, err := chatKeys(req.Chats)
			if err != nil || len(keys) == 0 {
				return reply{ID: req.ID, Error: "usage", Message: fmt.Sprint("no chats given ", err)}
			}
			r := deleteChats(ctx, l.cli, keys, req.DryRun, state.all, refetch)
			return reply{ID: req.ID, OK: r.OK, Results: r.Results}
		default:
			return reply{ID: req.ID, Error: "usage", Message: fmt.Sprintf("unknown op %q", req.Op)}
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
