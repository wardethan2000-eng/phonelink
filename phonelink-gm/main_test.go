package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"go.mau.fi/mautrix-gmessages/pkg/libgm/gmproto"
)

func part(number string, me bool) *gmproto.Participant {
	return &gmproto.Participant{ID: &gmproto.SmallInfo{Number: number}, IsMe: me}
}

func conv(id string, parts ...*gmproto.Participant) *gmproto.Conversation {
	return &gmproto.Conversation{ConversationID: id, Participants: parts, Status: gmproto.ConversationStatus_ACTIVE}
}

func TestOthersDropsMyNumberListedTwice(t *testing.T) {
	c := conv("21", part("+13165550000", true), part("+13165551111", false), part("+13165550000", false))
	if got := keySet(others(c)); got != "3165551111" {
		t.Fatalf("others = %q", got)
	}
}

func TestPickNeedsExactlyOneChatWithExactlyThoseMembers(t *testing.T) {
	me := part("+13165550000", true)
	convs := []*gmproto.Conversation{
		conv("1", me, part("+13165551111", false)),
		conv("2", me, part("+13165551111", false), part("+13165552222", false)),
		conv("3", me, part("3165552222", false), part("+1 316 555 1111", false)),
	}
	if c, why := pick(convs, keySet([]string{"+13165551111"})); c == nil || c.GetConversationID() != "1" {
		t.Fatalf("1:1 pick = %v %s", c, why)
	}
	if c, why := pick(convs, keySet([]string{"3165551111", "3165552222"})); c != nil || why != "ambiguous" {
		t.Fatalf("duplicate group pick = %v %s", c, why)
	}
	if c, why := pick(convs, keySet([]string{"3165553333"})); c != nil || why != "not_found" {
		t.Fatalf("missing pick = %v %s", c, why)
	}
	deleted := conv("4", me, part("3165554444", false))
	deleted.Status = gmproto.ConversationStatus_DELETED
	if c, _ := pick([]*gmproto.Conversation{deleted}, "3165554444"); c != nil {
		t.Fatal("picked a deleted chat")
	}
}

func TestBusinessChatsMatchByAddress(t *testing.T) {
	me := part("+13165550000", true)
	convs := []*gmproto.Conversation{
		conv("1", me, part("shop_ab3cd_agent@rbm.goog", false)),
		conv("2", me, part("other_xy3z_agent@rbm.goog", false)),
		conv("3", me, part("3", false)),
	}
	if c, why := pick(convs, keySet([]string{" Shop_AB3CD_agent@rbm.goog"})); c == nil || c.GetConversationID() != "1" {
		t.Fatalf("business pick = %v %s", c, why)
	}
	// The digits inside an agent address are not a phone number.
	if got := keySet([]string{"other_xy3z_agent@rbm.goog"}); got == "3" {
		t.Fatal("agent address keyed by its digits")
	}
}

func TestConversationEventsUseMillisecondsAndSkipDeleted(t *testing.T) {
	c := conv("9", part("+13165550000", true), part("+13165551111", false))
	c.LastMessageTimestamp = 1_790_000_000_123_456
	ev, ok := conversationEvent(c)
	if !ok || ev.Chat != "3165551111" || ev.LastTSms != 1_790_000_000_123 || ev.Unread {
		t.Fatalf("event = %+v %v", ev, ok)
	}
	c.Status = gmproto.ConversationStatus_DELETED
	if _, ok := conversationEvent(c); ok {
		t.Fatal("event for a deleted chat")
	}
}

func TestPickIgnoresMyNumberInTheRequest(t *testing.T) {
	me := part("+13165550000", true)
	convs := []*gmproto.Conversation{conv("1", me, part("+13165551111", false), part("+13165552222", false))}
	if c, why := pick(convs, keySet([]string{"3165550000", "3165551111", "3165552222"})); c == nil {
		t.Fatalf("pick with me included = %s", why)
	}
}

func TestLogsNeverCarryPayload(t *testing.T) {
	line := []byte(`{"level":"warn","decrypted_data":"SGVsbG8gZnJvbSBNb20=","resp_body":"x","error":"unknown event","message":"Got unknown event type"}` + "\n")
	out := string(redact(line))
	for _, secret := range []string{"SGVsbG8", `"resp_body":"x"`} {
		if strings.Contains(out, secret) {
			t.Fatalf("payload leaked: %s", out)
		}
	}
	if !strings.Contains(out, "Got unknown event type") || !strings.Contains(out, "unknown event") {
		t.Fatalf("diagnostics lost: %s", out)
	}
	long := []byte(`{"level":"warn","note":"` + strings.Repeat("a", 900) + `"}`)
	if strings.Contains(string(redact(long)), strings.Repeat("a", 400)) {
		t.Fatal("long field not hidden")
	}
	if strings.Contains(string(redact([]byte("not json Mom says hi"))), "Mom") {
		t.Fatal("non-JSON line leaked")
	}
}

func TestGmcliPairingIsCopiedInOnce(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("PHONELINK_GM_SESSION", "")
	t.Setenv("XDG_DATA_HOME", filepath.Join(dir, "data"))
	t.Setenv("XDG_STATE_HOME", filepath.Join(dir, "state"))
	own := filepath.Join(dir, "data", "phonelink", "gm", "session.json")
	if got := defaultSession(); got != own {
		t.Fatalf("session = %s", got)
	}
	if _, err := os.Stat(own); !os.IsNotExist(err) {
		t.Fatal("nothing to copy yet")
	}
	gm := filepath.Join(dir, "state", "gmcli", "session.json")
	_ = os.MkdirAll(filepath.Dir(gm), 0o700)
	_ = os.WriteFile(gm, []byte(`{"x":1}`), 0o600)
	defaultSession()
	if b, err := os.ReadFile(own); err != nil || string(b) != `{"x":1}` {
		t.Fatalf("copy = %q %v", b, err)
	}
	if fi, _ := os.Stat(own); fi.Mode().Perm() != 0o600 {
		t.Fatalf("mode %v", fi.Mode())
	}
	// Unlinked (the file removed): gmcli's isn't copied back.
	_ = os.Remove(own)
	defaultSession()
	if _, err := os.Stat(own); !os.IsNotExist(err) {
		t.Fatal("unlinking must stay unlinked")
	}
}
