package main

// Messages and chats as the app sees them (serve mode's `message` and
// `conversation` events, and `fetch` replies). Pure conversions from
// libgm's protobufs, tested in messages_test.go.

import (
	"strings"

	"go.mau.fi/mautrix-gmessages/pkg/libgm/gmproto"
)

type member struct {
	Number string `json:"number"`
	Name   string `json:"name,omitempty"`
}

// convEvent is one chat. Chat is the member key (as delete and mark_read
// take); Conv is Google's id, which the newer requests take.
type convEvent struct {
	Event    string   `json:"event"`
	Chat     string   `json:"chat"`
	Conv     string   `json:"conv"`
	Name     string   `json:"name,omitempty"`
	Members  []member `json:"members"`
	Group    bool     `json:"group"`
	RCS      bool     `json:"rcs"`
	ReadOnly bool     `json:"read_only,omitempty"`
	Archived bool     `json:"archived,omitempty"`
	Unread   bool     `json:"unread"`
	LastTSms int64    `json:"last_ts_ms"`
	Preview  string   `json:"preview,omitempty"`
}

type attachment struct {
	ID   string `json:"id"`
	Mime string `json:"mime,omitempty"`
	Name string `json:"name,omitempty"`
	Size int64  `json:"size,omitempty"`
}

// msgEvent is one message. Status is one of sending, sent, delivered,
// read, failed (mine) or received (theirs). Old marks messages the phone
// replays on connect rather than new ones.
type msgEvent struct {
	Event       string       `json:"event,omitempty"`
	Conv        string       `json:"conv"`
	Msg         string       `json:"msg"`
	TmpID       string       `json:"tmp_id,omitempty"`
	TSms        int64        `json:"ts_ms"`
	FromMe      bool         `json:"from_me"`
	From        string       `json:"from,omitempty"`
	Name        string       `json:"name,omitempty"`
	Text        string       `json:"text"`
	Status      string       `json:"status"`
	RCS         bool         `json:"rcs"`
	Deleted     bool         `json:"deleted,omitempty"`
	Old         bool         `json:"old,omitempty"`
	Attachments []attachment `json:"attachments,omitempty"`
}

func participantNumber(p *gmproto.Participant) string {
	if n := p.GetID().GetNumber(); n != "" {
		return n
	}
	return p.GetFormattedNumber()
}

func members(conv *gmproto.Conversation) []member {
	mine := map[string]bool{}
	for _, p := range conv.GetParticipants() {
		if p.GetIsMe() {
			if k := phoneKey(participantNumber(p)); k != "" {
				mine[k] = true
			}
		}
	}
	out := []member{}
	seen := map[string]bool{}
	for _, p := range conv.GetParticipants() {
		n := participantNumber(p)
		k := phoneKey(n)
		if p.GetIsMe() || n == "" || mine[k] || seen[k] {
			continue
		}
		seen[k] = true
		name := p.GetFullName()
		if name == n || name == p.GetFormattedNumber() {
			name = ""
		}
		out = append(out, member{Number: n, Name: name})
	}
	return out
}

func conversationEvent(conv *gmproto.Conversation) (convEvent, bool) {
	key := keySet(others(conv))
	if key == "" || conv.GetStatus() == gmproto.ConversationStatus_DELETED {
		return convEvent{}, false
	}
	ms := members(conv)
	name := conv.GetName()
	if !conv.GetIsGroupChat() && len(ms) == 1 && (name == ms[0].Number || name == "") {
		name = ""
	}
	return convEvent{
		Event:    "conversation",
		Chat:     key,
		Conv:     conv.GetConversationID(),
		Name:     name,
		Members:  ms,
		Group:    conv.GetIsGroupChat(),
		RCS:      conv.GetType() == gmproto.ConversationType_RCS,
		ReadOnly: conv.GetReadOnly(),
		Archived: conv.GetStatus() == gmproto.ConversationStatus_ARCHIVED,
		Unread:   conv.GetUnread(),
		// Google's timestamps are microseconds.
		LastTSms: conv.GetLastMessageTimestamp() / 1000,
		Preview:  conv.GetLatestMessage().GetDisplayContent(),
	}, true
}

// statusName reduces libgm's message status to the few the app shows.
// skip: states that are not a message yet (an MMS still downloading) or
// not a message at all (tombstones such as "group created").
func statusName(s gmproto.MessageStatusType) (name string, fromMe, deleted, skip bool) {
	switch {
	case s == gmproto.MessageStatusType_INCOMING_AUTO_DOWNLOADING,
		s == gmproto.MessageStatusType_INCOMING_RETRYING_AUTO_DOWNLOAD:
		return "", false, false, true
	case s >= 200:
		return "", false, false, true
	case s == gmproto.MessageStatusType_OUTGOING_DELETED:
		return "sent", true, true, false
	case s == gmproto.MessageStatusType_INCOMING_DELETED:
		return "received", false, true, false
	case s >= 100:
		return "received", false, false, false
	}
	switch s {
	case gmproto.MessageStatusType_OUTGOING_COMPLETE:
		return "sent", true, false, false
	case gmproto.MessageStatusType_OUTGOING_DELIVERED:
		return "delivered", true, false, false
	case gmproto.MessageStatusType_OUTGOING_DISPLAYED:
		return "read", true, false, false
	case gmproto.MessageStatusType_OUTGOING_FAILED_GENERIC,
		gmproto.MessageStatusType_OUTGOING_FAILED_EMERGENCY_NUMBER,
		gmproto.MessageStatusType_OUTGOING_CANCELED,
		gmproto.MessageStatusType_OUTGOING_FAILED_TOO_LARGE,
		gmproto.MessageStatusType_OUTGOING_FAILED_RECIPIENT_LOST_RCS,
		gmproto.MessageStatusType_OUTGOING_FAILED_NO_RETRY_NO_FALLBACK,
		gmproto.MessageStatusType_OUTGOING_FAILED_RECIPIENT_DID_NOT_DECRYPT,
		gmproto.MessageStatusType_OUTGOING_FAILED_RECIPIENT_LOST_ENCRYPTION,
		gmproto.MessageStatusType_OUTGOING_FAILED_RECIPIENT_DID_NOT_DECRYPT_NO_MORE_RETRY,
		gmproto.MessageStatusType_OUTGOING_FAILED_RECIPIENT_NEGATIVE_DELIVERY,
		gmproto.MessageStatusType_MESSAGE_STATUS_OUTGOING_FAILED_EMERGENCY_PROTOCOL_DETERMINATION_MESSAGE,
		gmproto.MessageStatusType_OUTGOING_RESTRICTED,
		gmproto.MessageStatusType_OUTGOING_FAILED_TO_ENCRYPT:
		return "failed", true, false, false
	case gmproto.MessageStatusType_STATUS_UNKNOWN:
		return "received", false, false, false
	}
	return "sending", true, false, false
}

// messageEvent converts one message; conv (may be nil) names the sender.
func messageEvent(m *gmproto.Message, conv *gmproto.Conversation) (msgEvent, bool) {
	status, fromMe, deleted, skip := statusName(m.GetMessageStatus().GetStatus())
	if skip || m.GetMessageID() == "" {
		return msgEvent{}, false
	}
	if m.GetParticipantID() == "1" || m.GetSenderParticipant().GetIsMe() {
		fromMe = true
		if status == "received" {
			status = "sent"
		}
	}
	ev := msgEvent{
		Event:  "message",
		Conv:   m.GetConversationID(),
		Msg:    m.GetMessageID(),
		TmpID:  m.GetTmpID(),
		TSms:   m.GetTimestamp() / 1000,
		FromMe: fromMe,
		Status: status,
		// Message.Type: 1 = SMS, 2/3 = MMS, 4+ = RCS (libgm's notes).
		RCS:     m.GetType() >= 4 || (m.GetType() == 0 && conv.GetType() == gmproto.ConversationType_RCS),
		Deleted: deleted,
	}
	if !fromMe {
		sender := m.GetSenderParticipant()
		if sender == nil && conv != nil {
			for _, p := range conv.GetParticipants() {
				if p.GetID().GetParticipantID() == m.GetParticipantID() {
					sender = p
					break
				}
			}
		}
		if sender != nil {
			ev.From = participantNumber(sender)
			if n := sender.GetFullName(); n != ev.From && n != sender.GetFormattedNumber() {
				ev.Name = n
			}
		}
	}
	var text []string
	for _, info := range m.GetMessageInfo() {
		if c := info.GetMessageContent(); c != nil && c.GetContent() != "" {
			text = append(text, c.GetContent())
		}
		if mc := info.GetMediaContent(); mc != nil {
			ev.Attachments = append(ev.Attachments, attachment{ID: mc.GetMediaID(), Mime: mc.GetMimeType(), Name: mc.GetMediaName(), Size: mc.GetSize()})
		}
	}
	if s := m.GetSubject(); s != "" {
		text = append([]string{s}, text...)
	}
	ev.Text = strings.Join(text, "\n")
	return ev, true
}

// sendRequest builds a text send the way gmcli and the mautrix bridge do:
// from the chat's own SIM when the phone told us its SIMs, else the
// legacy form (no SIM, the phone picks).
func sendRequest(conv *gmproto.Conversation, sims []*gmproto.SIMCard, text, tmpID string) *gmproto.SendMessageRequest {
	id := conv.GetConversationID()
	var sim *gmproto.SIMCard
	outgoing := conv.GetDefaultOutgoingID()
	for _, s := range sims {
		if s.GetSIMParticipant().GetID() == outgoing {
			sim = s
		}
	}
	if outgoing == "" && len(sims) == 1 {
		sim, outgoing = sims[0], sims[0].GetSIMParticipant().GetID()
	}
	if sim == nil {
		return &gmproto.SendMessageRequest{
			ConversationID: id,
			TmpID:          tmpID,
			MessagePayload: &gmproto.MessagePayload{
				ConversationID:        id,
				TmpID:                 tmpID,
				TmpID2:                tmpID,
				MessagePayloadContent: &gmproto.MessagePayloadContent{MessageContent: &gmproto.MessageContent{Content: text}},
			},
		}
	}
	forceRCS := sim.GetRCSChats().GetEnabled() &&
		conv.GetSendMode() == gmproto.ConversationSendMode_SEND_MODE_AUTO &&
		(conv.GetType() == gmproto.ConversationType_RCS || conv.GetType() == gmproto.ConversationType_UNKNOWN_CONVERSATION_TYPE)
	return &gmproto.SendMessageRequest{
		ConversationID: id,
		TmpID:          tmpID,
		ForceRCS:       forceRCS,
		SIMPayload:     sim.GetSIMData().GetSIMPayload(),
		MessagePayload: &gmproto.MessagePayload{
			ConversationID: id,
			ParticipantID:  outgoing,
			TmpID:          tmpID,
			TmpID2:         tmpID,
			MessageInfo: []*gmproto.MessageInfo{{
				Data: &gmproto.MessageInfo_MessageContent{MessageContent: &gmproto.MessageContent{Content: text}},
			}},
		},
	}
}
