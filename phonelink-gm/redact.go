package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
)

// payloadFields are log fields in which libgm puts raw, decrypted traffic
// (event_handler.go "decrypted_data", longpoll.go "resp_body"). They can
// hold message text, so they never reach stderr, which ends up in the
// journal.
var payloadFields = map[string]bool{
	"decrypted_data": true,
	"resp_body":      true,
	"data":           true,
	"body":           true,
	"raw":            true,
}

// maxFieldLen caps any other string field: real diagnostics are short, while
// a long field is most likely payload.
const maxFieldLen = 300

// redactWriter passes zerolog's JSON lines through with payload removed.
type redactWriter struct{ w io.Writer }

func (r redactWriter) Write(p []byte) (int, error) {
	if _, err := r.w.Write(redact(p)); err != nil {
		return 0, err
	}
	return len(p), nil
}

func redact(line []byte) []byte {
	var fields map[string]any
	if err := json.Unmarshal(bytes.TrimSpace(line), &fields); err != nil {
		return []byte(fmt.Sprintf("{\"level\":\"warn\",\"message\":\"unreadable log line hidden (%d bytes)\"}\n", len(line)))
	}
	for k, v := range fields {
		if payloadFields[k] {
			fields[k] = fmt.Sprintf("[hidden %s]", sizeOf(v))
			continue
		}
		if s, ok := v.(string); ok && len(s) > maxFieldLen {
			fields[k] = fmt.Sprintf("[hidden %d bytes]", len(s))
		} else if _, ok := v.(string); !ok && k != "level" && k != "time" {
			if b, _ := json.Marshal(v); len(b) > maxFieldLen {
				fields[k] = fmt.Sprintf("[hidden %d bytes]", len(b))
			}
		}
	}
	out, err := json.Marshal(fields)
	if err != nil {
		return []byte("{\"level\":\"warn\",\"message\":\"log line hidden\"}\n")
	}
	return append(out, '\n')
}

func sizeOf(v any) string {
	b, _ := json.Marshal(v)
	return fmt.Sprintf("%d bytes", len(b))
}
