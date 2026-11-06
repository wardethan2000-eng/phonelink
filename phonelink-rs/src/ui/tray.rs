//! The tray icon (port of `phonelink/tray_sni.py`): a StatusNotifierItem
//! and a two-item DBusMenu ("Show Phone Link", "Quit") exported over gio on
//! the app's own session connection. Left click toggles the window.
//! `_tray.py`, the GTK3 / XApp fallback for desktops without a
//! StatusNotifierWatcher, is not ported: without a watcher there is no tray,
//! and closing the window quits instead of hiding it.

use std::rc::Rc;

use gio::prelude::*;
use glib::Variant;

use crate::core::settings::APPLICATION_ID;

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";

const SNI_XML: &str = r#"<node><interface name="org.kde.StatusNotifierItem">
  <property name="Category" type="s" access="read"/>
  <property name="Id" type="s" access="read"/>
  <property name="Title" type="s" access="read"/>
  <property name="Status" type="s" access="read"/>
  <property name="IconName" type="s" access="read"/>
  <property name="IconPixmap" type="a(iiay)" access="read"/>
  <property name="ItemIsMenu" type="b" access="read"/>
  <property name="Menu" type="o" access="read"/>
  <method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
  <method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
  <method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
  <method name="Scroll"><arg type="i" direction="in"/><arg type="s" direction="in"/></method>
  <signal name="NewIcon"/>
  <signal name="NewStatus"><arg type="s"/></signal>
  <signal name="NewTitle"/>
</interface></node>"#;

const MENU_XML: &str = r#"<node><interface name="com.canonical.dbusmenu">
  <property name="Version" type="u" access="read"/>
  <property name="Status" type="s" access="read"/>
  <method name="GetLayout">
    <arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/>
    <arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/>
  </method>
  <method name="GetGroupProperties">
    <arg type="ai" direction="in"/><arg type="as" direction="in"/><arg type="a(ia{sv})" direction="out"/>
  </method>
  <method name="GetProperty">
    <arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/>
  </method>
  <method name="Event">
    <arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/><arg type="u" direction="in"/>
  </method>
  <method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
  <signal name="LayoutUpdated"><arg type="u"/><arg type="i"/></signal>
</interface></node>"#;

const SHOW_ID: i32 = 1;
const QUIT_ID: i32 = 2;

/// Keeps the exported objects and the watcher watch alive; undoes both on
/// drop.
pub struct Tray {
    conn: gio::DBusConnection,
    ids: Vec<gio::RegistrationId>,
    unwatch: Option<Box<dyn FnOnce()>>,
}

impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(u) = self.unwatch.take() {
            u();
        }
        for id in self.ids.drain(..) {
            let _ = self.conn.unregister_object(id);
        }
    }
}

fn props(label: &str) -> Variant {
    let d = glib::VariantDict::new(None);
    d.insert_value("label", &label.to_variant());
    d.insert_value("enabled", &true.to_variant());
    d.insert_value("visible", &true.to_variant());
    d.end()
}

/// `(id, properties, children)`
fn menu_node(id: i32, props: Variant, children: Vec<Variant>) -> Variant {
    Variant::tuple_from_iter([id.to_variant(), props, Variant::array_from_iter_with_type(glib::VariantTy::VARIANT, children)])
}

fn layout() -> Variant {
    let root = glib::VariantDict::new(None);
    root.insert_value("children-display", &"submenu".to_variant());
    let items = [(SHOW_ID, "Show Phone Link"), (QUIT_ID, "Quit")]
        .iter()
        .map(|(id, label)| Variant::from_variant(&menu_node(*id, props(label), Vec::new())))
        .collect();
    let tree = menu_node(0, root.end(), items);
    Variant::tuple_from_iter([1u32.to_variant(), tree])
}

/// The bundled 64 px icon as SNI pixmaps (ARGB32, network byte order), so
/// the tray shows it even before the icon is installed in a theme.
fn icon_pixmaps() -> Variant {
    let entries: Vec<Variant> = [64, 128]
        .iter()
        .filter_map(|size| {
            let path = format!("/io/github/wardethan2000_eng/phonelink/icons/{size}x{size}/apps/{APPLICATION_ID}.png");
            let pb = gtk::gdk_pixbuf::Pixbuf::from_resource(&path).ok()?;
            let pb = if pb.has_alpha() { pb } else { pb.add_alpha(false, 0, 0, 0).ok()? };
            let (w, h, stride) = (pb.width(), pb.height(), pb.rowstride() as usize);
            let bytes = pb.read_pixel_bytes();
            let mut argb = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let p = &bytes[y * stride + x * 4..y * stride + x * 4 + 4];
                    argb.extend_from_slice(&[p[3], p[0], p[1], p[2]]);
                }
            }
            Some(Variant::tuple_from_iter([w.to_variant(), h.to_variant(), argb.to_variant()]))
        })
        .collect();
    Variant::array_from_iter_with_type(glib::VariantTy::new("(iiay)").unwrap(), entries)
}

/// Export the tray and register it with the watcher whenever one is on the
/// bus (also one that starts later, e.g. a panel that comes up after the
/// app at login). `shown(true/false)` reports whether a tray host has it;
/// without one the window quits on close.
pub fn start(conn: &gio::DBusConnection, toggle: Rc<dyn Fn()>, quit: Rc<dyn Fn()>, shown: Rc<dyn Fn(bool)>) -> Option<Tray> {
    let sni = gio::DBusNodeInfo::for_xml(SNI_XML).ok()?.lookup_interface("org.kde.StatusNotifierItem")?;
    let menu = gio::DBusNodeInfo::for_xml(MENU_XML).ok()?.lookup_interface("com.canonical.dbusmenu")?;
    let pixmaps = icon_pixmaps();
    let mut tray = Tray { conn: conn.clone(), ids: Vec::new(), unwatch: None };

    let on_toggle = toggle.clone();
    let id = conn
        .register_object(ITEM_PATH, &sni)
        .property(move |_, _, _, _, prop| match prop {
            "Category" => "ApplicationStatus".to_variant(),
            "Id" => "phonelink".to_variant(),
            "Title" => "Phone Link".to_variant(),
            "Status" => "Active".to_variant(),
            "IconName" => APPLICATION_ID.to_variant(),
            "IconPixmap" => pixmaps.clone(),
            "ItemIsMenu" => false.to_variant(),
            "Menu" => glib::variant::ObjectPath::try_from(MENU_PATH).unwrap().to_variant(),
            _ => "".to_variant(),
        })
        .method_call(move |_, _, _, _, method, _, inv| {
            if matches!(method, "Activate" | "SecondaryActivate") {
                let f = on_toggle.clone();
                glib::idle_add_local_once(move || f());
            }
            inv.return_value(None);
        })
        .build()
        .ok()?;
    tray.ids.push(id);

    let id = conn
        .register_object(MENU_PATH, &menu)
        .property(|_, _, _, _, prop| match prop {
            "Version" => 3u32.to_variant(),
            _ => "normal".to_variant(),
        })
        .method_call(move |_, _, _, _, method, params, inv| match method {
            "GetLayout" => inv.return_value(Some(&layout())),
            "GetGroupProperties" => {
                let entries: Vec<Variant> = [(SHOW_ID, "Show Phone Link"), (QUIT_ID, "Quit")]
                    .iter()
                    .map(|(id, label)| Variant::tuple_from_iter([id.to_variant(), props(label)]))
                    .collect();
                let arr = Variant::array_from_iter_with_type(glib::VariantTy::new("(ia{sv})").unwrap(), entries);
                inv.return_value(Some(&Variant::tuple_from_iter([arr])));
            }
            "GetProperty" => inv.return_value(Some(&(Variant::from_variant(&"".to_variant()),).to_variant())),
            "Event" => {
                let id = params.child_value(0).get::<i32>().unwrap_or_default();
                let event = params.child_value(1).get::<String>().unwrap_or_default();
                if event == "clicked" {
                    let f = if id == SHOW_ID { Some(toggle.clone()) } else if id == QUIT_ID { Some(quit.clone()) } else { None };
                    if let Some(f) = f {
                        glib::idle_add_local_once(move || f());
                    }
                }
                inv.return_value(None);
            }
            "AboutToShow" => inv.return_value(Some(&(false,).to_variant())),
            _ => inv.return_value(None),
        })
        .build()
        .ok()?;
    tray.ids.push(id);

    let name = conn.unique_name()?.to_string();
    let (c, gone) = (conn.clone(), shown.clone());
    let id = gio::bus_watch_name_on_connection(
        conn,
        WATCHER_NAME,
        gio::BusNameWatcherFlags::NONE,
        move |_, _, _| {
            let (c, name, shown) = (c.clone(), name.clone(), shown.clone());
            glib::spawn_future_local(async move {
                let reply = c
                    .call_future(
                        Some(WATCHER_NAME),
                        WATCHER_PATH,
                        WATCHER_NAME,
                        "RegisterStatusNotifierItem",
                        Some(&(name,).to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        3000,
                    )
                    .await;
                if let Err(e) = &reply {
                    log::warn!("tray: the StatusNotifierWatcher refused the icon: {}", e.message());
                }
                shown(reply.is_ok());
            });
        },
        move |_, _| {
            log::info!("no tray host (StatusNotifierWatcher); closing the window will quit");
            gone(false);
        },
    );
    tray.unwatch = Some(Box::new(move || gio::bus_unwatch_name(id)));
    Some(tray)
}
