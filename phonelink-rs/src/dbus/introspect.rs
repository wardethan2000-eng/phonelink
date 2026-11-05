//! A small reader for D-Bus introspection XML: interfaces with their methods,
//! signals and properties, plus child node names. Enough for
//! `supports_method` and the `--dump-signatures` surface check; the XML that
//! Qt and GDBus emit is regular enough that a tag scanner is exact.

use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Member {
    pub name: String,
    /// Concatenated `in` argument types (all args, for a signal).
    pub args_in: String,
    /// Concatenated `out` argument types (methods only).
    pub args_out: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Interface {
    pub name: String,
    pub methods: Vec<Member>,
    pub signals: Vec<Member>,
    /// `(name, type)`.
    pub properties: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    pub interfaces: Vec<Interface>,
    pub children: Vec<String>,
}

impl Node {
    pub fn interface(&self, name: &str) -> Option<&Interface> {
        self.interfaces.iter().find(|i| i.name == name)
    }
}

impl Interface {
    pub fn method(&self, name: &str) -> Option<&Member> {
        self.methods.iter().find(|m| m.name == name)
    }
    pub fn signal(&self, name: &str) -> Option<&Member> {
        self.signals.iter().find(|m| m.name == name)
    }
    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties.iter().find(|p| p.0 == name).map(|p| p.1.as_str())
    }
}

static TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(/?)(node|interface|method|signal|property|arg)\b([^>]*)>").unwrap());
static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([\w:]+)\s*=\s*"([^"]*)""#).unwrap());

#[derive(PartialEq)]
enum Open {
    Method,
    Signal,
    None,
}

pub fn parse(xml: &str) -> Node {
    let mut node = Node::default();
    let mut depth_nodes = 0usize;
    let mut open = Open::None;
    for cap in TAG.captures_iter(xml) {
        let closing = &cap[1] == "/";
        let tag = &cap[2];
        let body = &cap[3];
        let self_closing = body.trim_end().ends_with('/');
        let attr = |key: &str| {
            ATTR.captures_iter(body)
                .find(|a| &a[1] == key)
                .map(|a| a[2].to_string())
                .unwrap_or_default()
        };
        match (tag, closing) {
            ("node", false) => {
                depth_nodes += 1;
                // The root is depth 1; direct children are depth 2.
                if depth_nodes == 2 {
                    let name = attr("name");
                    if !name.is_empty() {
                        node.children.push(name);
                    }
                }
                if self_closing {
                    depth_nodes -= 1;
                }
            }
            ("node", true) => depth_nodes = depth_nodes.saturating_sub(1),
            _ if depth_nodes > 1 => {} // a child node's inline description
            ("interface", false) => node.interfaces.push(Interface { name: attr("name"), ..Default::default() }),
            ("method" | "signal", false) => {
                let m = Member { name: attr("name"), ..Default::default() };
                if let Some(iface) = node.interfaces.last_mut() {
                    if tag == "method" {
                        iface.methods.push(m);
                    } else {
                        iface.signals.push(m);
                    }
                }
                open = match (self_closing, tag) {
                    (true, _) => Open::None,
                    (false, "method") => Open::Method,
                    _ => Open::Signal,
                };
            }
            ("method" | "signal", true) => open = Open::None,
            ("property", false) => {
                if let Some(iface) = node.interfaces.last_mut() {
                    iface.properties.push((attr("name"), attr("type")));
                }
            }
            ("arg", false) => {
                let Some(iface) = node.interfaces.last_mut() else { continue };
                let ty = attr("type");
                let out = attr("direction") == "out";
                let member = match open {
                    Open::Method => iface.methods.last_mut(),
                    Open::Signal => iface.signals.last_mut(),
                    Open::None => None,
                };
                if let Some(m) = member {
                    if out { m.args_out.push_str(&ty) } else { m.args_in.push_str(&ty) }
                }
            }
            _ => {}
        }
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.kde.kdeconnect.device.conversations">
    <method name="activeConversations">
      <arg type="av" direction="out"/>
    </method>
    <method name="requestConversation">
      <arg name="conversationID" type="x" direction="in"/>
      <arg name="start" type="i" direction="in"/>
      <arg name="end" type="i" direction="in"/>
    </method>
    <method name="requestAllConversationThreads"/>
    <signal name="conversationUpdated">
      <arg name="msg" type="v"/>
    </signal>
    <signal name="conversationLoaded">
      <arg name="threadID" type="x"/>
      <arg name="messageCount" type="t"/>
    </signal>
  </interface>
  <interface name="org.kde.kdeconnect.device">
    <property name="isReachable" type="b" access="read"/>
    <signal name="reachableChanged">
      <arg name="reachable" type="b"/>
    </signal>
  </interface>
  <node name="sms"/>
  <node name="battery">
    <interface name="ignored.inline"/>
  </node>
</node>"#;

    #[test]
    fn reads_members_and_children() {
        let n = parse(XML);
        let c = n.interface("org.kde.kdeconnect.device.conversations").unwrap();
        assert_eq!(c.method("activeConversations").unwrap().args_out, "av");
        assert_eq!(c.method("requestConversation").unwrap().args_in, "xii");
        assert!(c.method("requestAllConversationThreads").is_some());
        assert!(c.method("markConversationAsRead").is_none());
        assert_eq!(c.signal("conversationUpdated").unwrap().args_in, "v");
        assert_eq!(c.signal("conversationLoaded").unwrap().args_in, "xt");
        let d = n.interface("org.kde.kdeconnect.device").unwrap();
        assert_eq!(d.property("isReachable"), Some("b"));
        assert_eq!(d.signal("reachableChanged").unwrap().args_in, "b");
        assert_eq!(n.children, vec!["sms", "battery"]);
        assert!(n.interface("ignored.inline").is_none());
    }
}
