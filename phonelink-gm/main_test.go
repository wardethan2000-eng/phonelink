package main

import (
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
