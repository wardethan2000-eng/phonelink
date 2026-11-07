package main

import (
	"testing"

	"go.mau.fi/mautrix-gmessages/pkg/libgm/gmproto"
)

func text(s string) *gmproto.MessageInfo {
	return &gmproto.MessageInfo{Data: &gmproto.MessageInfo_MessageContent{MessageContent: &gmproto.MessageContent{Content: s}}}
}

func msg(id string, status gmproto.MessageStatusType, participant string, infos ...*gmproto.MessageInfo) *gmproto.Message {
	return &gmproto.Message{
		MessageID:      id,
		ConversationID: "7",
		ParticipantID:  participant,
		Timestamp:      1_790_000_000_123_456,
		MessageStatus:  &gmproto.MessageStatus{Status: status},
		MessageInfo:    infos,
		Type:           4,
	}
}

func TestStatusNames(t *testing.T) {
	cases := []struct {
		s            gmproto.MessageStatusType
		name         string
		fromMe, skip bool
	}{
		{gmproto.MessageStatusType_OUTGOING_SENDING, "sending", true, false},
		{gmproto.MessageStatusType_OUTGOING_YET_TO_SEND, "sending", true, false},
		{gmproto.MessageStatusType_OUTGOING_COMPLETE, "sent", true, false},
		{gmproto.MessageStatusType_OUTGOING_DELIVERED, "delivered", true, false},
		{gmproto.MessageStatusType_OUTGOING_DISPLAYED, "read", true, false},
		{gmproto.MessageStatusType_OUTGOING_FAILED_GENERIC, "failed", true, false},
		{gmproto.MessageStatusType_INCOMING_COMPLETE, "received", false, false},
		{gmproto.MessageStatusType_INCOMING_DISPLAYED, "received", false, false},
		{gmproto.MessageStatusType_INCOMING_AUTO_DOWNLOADING, "", false, true},
		{gmproto.MessageStatusType_TOMBSTONE_RCS_GROUP_CREATED, "", false, true},
	}
	for _, c := range cases {
		name, fromMe, _, skip := statusName(c.s)
		if name != c.name || fromMe != c.fromMe || skip != c.skip {
			t.Errorf("%s: got %q %v %v", c.s, name, fromMe, skip)
		}
	}
}

func TestMessageEvent(t *testing.T) {
	alice := &gmproto.Participant{ID: &gmproto.SmallInfo{Number: "+13165551111", ParticipantID: "5"}, FullName: "Alice", FormattedNumber: "(316) 555-1111"}
	c := &gmproto.Conversation{ConversationID: "7", Participants: []*gmproto.Participant{alice, part("+13165550000", true)}, Type: gmproto.ConversationType_RCS}

	ev, ok := messageEvent(msg("m1", gmproto.MessageStatusType_INCOMING_COMPLETE, "5", text("hello"), text("there")), c)
	if !ok || ev.FromMe || ev.From != "+13165551111" || ev.Name != "Alice" || ev.Text != "hello\nthere" || ev.TSms != 1_790_000_000_123 || !ev.RCS || ev.Status != "received" {
		t.Fatalf("incoming = %+v", ev)
	}

	// Participant "1" is me even if the status looks incoming.
	ev, _ = messageEvent(msg("m2", gmproto.MessageStatusType_STATUS_UNKNOWN, "1", text("mine")), c)
	if !ev.FromMe || ev.Status != "sent" || ev.From != "" {
		t.Fatalf("mine = %+v", ev)
	}

	m := msg("m3", gmproto.MessageStatusType_INCOMING_COMPLETE, "5")
	m.MessageInfo = []*gmproto.MessageInfo{{Data: &gmproto.MessageInfo_MediaContent{MediaContent: &gmproto.MediaContent{MediaID: "x", MimeType: "image/jpeg", MediaName: "a.jpg", Size: 9}}}}
	m.Type = 1
	ev, _ = messageEvent(m, c)
	if len(ev.Attachments) != 1 || ev.Attachments[0].Mime != "image/jpeg" || ev.Text != "" || ev.RCS {
		t.Fatalf("media = %+v", ev)
	}

	if _, ok := messageEvent(msg("m4", gmproto.MessageStatusType_TOMBSTONE_PARTICIPANT_JOINED, "5"), c); ok {
		t.Fatal("tombstones are not messages")
	}
}

func TestConversationEvent(t *testing.T) {
	alice := &gmproto.Participant{ID: &gmproto.SmallInfo{Number: "+13165551111"}, FullName: "Alice"}
	c := &gmproto.Conversation{ConversationID: "7", Name: "Alice", Participants: []*gmproto.Participant{alice, part("+13165550000", true)},
		Type: gmproto.ConversationType_RCS, Unread: true, LastMessageTimestamp: 5_000_000, Status: gmproto.ConversationStatus_ACTIVE}
	ev, ok := conversationEvent(c)
	if !ok || ev.Chat != "3165551111" || ev.Conv != "7" || !ev.RCS || !ev.Unread || ev.LastTSms != 5000 || len(ev.Members) != 1 || ev.Members[0].Name != "Alice" {
		t.Fatalf("conversation = %+v", ev)
	}
	c.Status = gmproto.ConversationStatus_DELETED
	if _, ok := conversationEvent(c); ok {
		t.Fatal("deleted chats are skipped")
	}
}

func TestSendRequestUsesTheChatsSIM(t *testing.T) {
	sim := &gmproto.SIMCard{
		SIMParticipant: &gmproto.SIMParticipant{ID: "9"},
		SIMData:        &gmproto.SIMData{SIMPayload: &gmproto.SIMPayload{SIMNumber: 1}},
		RCSChats:       &gmproto.RCSChats{Enabled: true},
	}
	c := &gmproto.Conversation{ConversationID: "7", DefaultOutgoingID: "9", Type: gmproto.ConversationType_RCS, SendMode: gmproto.ConversationSendMode_SEND_MODE_AUTO}
	req := sendRequest(c, []*gmproto.SIMCard{sim}, "hi", "tmp_1")
	if !req.ForceRCS || req.SIMPayload.GetSIMNumber() != 1 || req.MessagePayload.ParticipantID != "9" ||
		req.MessagePayload.MessageInfo[0].GetMessageContent().GetContent() != "hi" || req.TmpID != "tmp_1" {
		t.Fatalf("send = %+v", req)
	}
	// No SIMs known: the legacy form, which the phone accepts too.
	req = sendRequest(c, nil, "hi", "tmp_2")
	if req.SIMPayload != nil || req.MessagePayload.MessagePayloadContent.GetMessageContent().GetContent() != "hi" {
		t.Fatalf("legacy send = %+v", req)
	}
}
