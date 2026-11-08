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
	m.MessageInfo = []*gmproto.MessageInfo{{Data: &gmproto.MessageInfo_MediaContent{MediaContent: &gmproto.MediaContent{MediaID: "x", MimeType: "image/jpeg", MediaName: "a.jpg", Size: 9, DecryptionKey: []byte{1, 2}}}}}
	m.Type = 1
	ev, _ = messageEvent(m, c)
	if len(ev.Attachments) != 1 || ev.Attachments[0].Mime != "image/jpeg" || string(ev.Attachments[0].Key) != "\x01\x02" || ev.Text != "" || ev.RCS {
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
	self := &gmproto.Conversation{ConversationID: "8", Participants: []*gmproto.Participant{part("+13165550000", true)}, Status: gmproto.ConversationStatus_ACTIVE}
	if ev, ok := conversationEvent(self); !ok || ev.Chat != "3165550000" || len(ev.Members) != 1 || ev.Members[0].Number != "+13165550000" {
		t.Fatalf("chat with myself = %+v %v", ev, ok)
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
	req := sendRequest(c, []*gmproto.SIMCard{sim}, "hi", nil, "tmp_1", "")
	if !req.ForceRCS || req.SIMPayload.GetSIMNumber() != 1 || req.MessagePayload.ParticipantID != "9" ||
		req.MessagePayload.MessageInfo[0].GetMessageContent().GetContent() != "hi" || req.TmpID != "tmp_1" {
		t.Fatalf("send = %+v", req)
	}
	// No SIMs known: the legacy form, which the phone accepts too.
	req = sendRequest(c, nil, "hi", nil, "tmp_2", "")
	if req.SIMPayload != nil || req.MessagePayload.MessagePayloadContent.GetMessageContent().GetContent() != "hi" {
		t.Fatalf("legacy send = %+v", req)
	}
	// A picture goes first, then its caption.
	pic := &gmproto.MediaContent{MediaID: "m1", MimeType: "image/jpeg"}
	req = sendRequest(c, []*gmproto.SIMCard{sim}, "look", pic, "tmp_3", "")
	infos := req.MessagePayload.MessageInfo
	if len(infos) != 2 || infos[0].GetMediaContent().GetMediaID() != "m1" || infos[1].GetMessageContent().GetContent() != "look" {
		t.Fatalf("media send = %+v", infos)
	}
	// Without a caption, only the picture.
	if infos := sendRequest(c, nil, "", pic, "tmp_4", "").MessagePayload.MessageInfo; len(infos) != 1 {
		t.Fatalf("bare picture = %+v", infos)
	}
}

func person(number, pid string, me bool) *gmproto.Participant {
	return &gmproto.Participant{ID: &gmproto.SmallInfo{Number: number, ParticipantID: pid}, IsMe: me}
}

func TestMessagesCarryReactionsAndReplies(t *testing.T) {
	c := conv("7", person("+13165550000", "5", true), person("+13165551111", "8", false), person("+13165552222", "9", false))
	c.DefaultOutgoingID = "5"
	m := msg("m2", gmproto.MessageStatusType_INCOMING_COMPLETE, "8", text("see you"))
	m.ReplyMessage = &gmproto.ReplyMessage{MessageID: "m1"}
	m.Reactions = []*gmproto.ReactionEntry{
		{Data: &gmproto.ReactionData{Type: gmproto.EmojiType_LIKE}, ParticipantIDs: []string{"5", "9"}},
		{Data: &gmproto.ReactionData{Type: gmproto.EmojiType_CUSTOM, Unicode: "🎉"}, ParticipantIDs: []string{"9"}},
		{Data: &gmproto.ReactionData{Type: gmproto.EmojiType_EMOTIFY}, ParticipantIDs: []string{"9"}},
	}
	got, ok := messageEvent(m, c)
	if !ok || got.ReplyTo != "m1" || len(got.Reactions) != 2 {
		t.Fatalf("message = %+v", got)
	}
	if like := got.Reactions[0]; like.Emoji != "👍" || like.Count != 2 || !like.Mine || len(like.By) != 1 || like.By[0] != "+13165552222" {
		t.Fatalf("like = %+v", like)
	}
	if party := got.Reactions[1]; party.Emoji != "🎉" || party.Count != 1 || party.Mine {
		t.Fatalf("custom = %+v", party)
	}
	// No chat known: nothing is mine but participant 1.
	if got, _ := messageEvent(m, nil); got.Reactions[0].Mine {
		t.Fatalf("mine without the chat: %+v", got.Reactions)
	}
}

func TestRepliesAndReactionsAsTheBridgeSendsThem(t *testing.T) {
	c := conv("7", person("+13165550000", "5", true), person("+13165551111", "8", false))
	c.DefaultOutgoingID = "5"
	sim := &gmproto.SIMCard{SIMParticipant: &gmproto.SIMParticipant{ID: "5"}, SIMData: &gmproto.SIMData{SIMPayload: &gmproto.SIMPayload{SIMNumber: 1}}}
	if r := sendRequest(c, []*gmproto.SIMCard{sim}, "on my way", nil, "tmp_9", "m1").GetReply().GetMessageID(); r != "m1" {
		t.Fatalf("reply = %q", r)
	}
	if sendRequest(c, nil, "hi", nil, "tmp_9", "").GetReply() != nil {
		t.Fatal("a plain text carried a reply")
	}
	add := reactionRequest(c, []*gmproto.SIMCard{sim}, "m1", "😂", gmproto.SendReactionRequest_ADD)
	if add.GetMessageID() != "m1" || add.GetReactionData().GetType() != gmproto.EmojiType_LAUGH || add.GetSIMPayload().GetSIMNumber() != 1 {
		t.Fatalf("add = %v", add)
	}
	if rm := reactionRequest(c, []*gmproto.SIMCard{sim}, "m1", "😂", gmproto.SendReactionRequest_REMOVE); rm.GetSIMPayload() != nil {
		t.Fatalf("remove carried a SIM: %v", rm)
	}
	if _, ok := reactionAction("explode"); ok {
		t.Fatal("unknown reaction action accepted")
	}
}

func TestPreviewOnlyPicturesAskForTheFullSize(t *testing.T) {
	c := conv("7", part("+13165551111", false), part("+13165550000", true))
	media := func(mc *gmproto.MediaContent, part string) *gmproto.MessageInfo {
		return &gmproto.MessageInfo{ActionMessageID: &part, Data: &gmproto.MessageInfo_MediaContent{MediaContent: mc}}
	}
	m := msg("m1", gmproto.MessageStatusType_INCOMING_COMPLETE, "5",
		media(&gmproto.MediaContent{ThumbnailMediaID: "th", ThumbnailDecryptionKey: []byte{1}, MimeType: "image/jpeg"}, "p1"),
		media(&gmproto.MediaContent{MediaID: "full", DecryptionKey: []byte{2}, ThumbnailMediaID: "th2", MimeType: "image/png"}, "p2"),
		media(&gmproto.MediaContent{MimeType: "video/mp4"}, "p3"))
	ev, _ := messageEvent(m, c)
	if len(ev.Attachments) != 2 {
		t.Fatalf("attachments = %+v (one still uploading is left out)", ev.Attachments)
	}
	a, b := ev.Attachments[0], ev.Attachments[1]
	if a.ID != "th" || !a.Thumb || a.Key[0] != 1 || a.Part != "p1" || b.ID != "full" || b.Thumb || b.Key[0] != 2 {
		t.Fatalf("attachments = %+v", ev.Attachments)
	}
	if got := thumbParts(ev); len(got) != 1 || got[0] != "p1" {
		t.Fatalf("thumbParts = %v", got)
	}
}

func TestChatChanges(t *testing.T) {
	for change, want := range map[string]string{
		"archive": "status:ARCHIVED", "unarchive": "status:ACTIVE", "mute": "mute:MUTE", "unmute": "mute:UNMUTE",
		"block": "action:BLOCK", "spam": "action:BLOCK_AND_REPORT", "unblock": "action:UNBLOCK",
	} {
		req, ok := chatChange("7", change)
		if !ok || req.GetConversationID() != "7" {
			t.Fatalf("%s: %v %v", change, req, ok)
		}
		var got string
		d := req.GetUpdateData()
		switch {
		case d.GetData() != nil && d.GetConversationID() != "7":
			t.Fatalf("%s: data for chat %q", change, d.GetConversationID())
		case d != nil && d.GetData() != nil:
			if _, isMute := d.GetData().(*gmproto.UpdateConversationData_Mute); isMute {
				got = "mute:" + d.GetMute().String()
			} else {
				got = "status:" + d.GetStatus().String()
			}
		default:
			got = "action:" + req.GetAction().String()
		}
		if got != want {
			t.Errorf("%s = %s, want %s", change, got, want)
		}
	}
	if _, ok := chatChange("7", "pin"); ok {
		t.Fatal("pin isn't something the phone takes from the web")
	}
}

func TestConversationEventPinnedAndBlocked(t *testing.T) {
	c := conv("7", part("+13165551111", false), part("+13165550000", true))
	c.Pinned = true
	c.Status = gmproto.ConversationStatus_SPAM_FOLDER
	ev, ok := conversationEvent(c)
	if !ok || !ev.Pinned || !ev.Blocked || ev.Archived {
		t.Fatalf("event = %+v", ev)
	}
	c.Status = gmproto.ConversationStatus_KEEP_ARCHIVED
	if ev, _ := conversationEvent(c); !ev.Archived || ev.Blocked {
		t.Fatalf("event = %+v", ev)
	}
}

func TestSignInWaitsForEveryNeededCookie(t *testing.T) {
	all := map[string]string{"SID": "a", "HSID": "b", "SSID": "c", "APISID": "d", "SAPISID": "e", "OSID": "f", "OTHER": "x"}
	got, ok := needCookies(all)
	if !ok || len(got) != 6 || got["OTHER"] != "" {
		t.Fatalf("got %v %v", got, ok)
	}
	delete(all, "OSID")
	if _, ok := needCookies(all); ok {
		t.Fatal("OSID is required")
	}
	for raw, want := range map[string]bool{
		"https://messages.google.com/web/config":          true,
		"https://messages.google.com/web/config?x=1":      true,
		"https://messages.google.com.evil.com/web/config": false,
		"http://messages.google.com/web/config":           false,
		"https://messages.google.com/web/conversations":   false,
		"https://user@messages.google.com/web/config":     false,
	} {
		if isConfigURL(raw) != want {
			t.Errorf("isConfigURL(%q) != %v", raw, want)
		}
	}
}
