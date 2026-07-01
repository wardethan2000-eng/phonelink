"""Marshalling tests for the in-process StatusNotifierItem tray.

The live D-Bus registration can't be unit-tested, but the GLib.Variant builders
for the dbusmenu can — and a wrong nesting there (e.g. double-wrapping an a{sv}
value) silently makes the menu time out, so it is worth pinning down.
"""

import unittest

try:
    from gi.repository import GLib
    from phonelink import tray_sni
    _IMPORT_ERR = None
except Exception as _e:  # noqa: BLE001
    tray_sni = None
    _IMPORT_ERR = _e


@unittest.skipIf(tray_sni is None, f"gi unavailable: {_IMPORT_ERR}")
class TraySniMarshallingTests(unittest.TestCase):
    def test_menu_item_builds_expected_type(self):
        item = tray_sni._menu_item(1, "Show Phone Link")
        self.assertEqual(item.get_type_string(), "(ia{sv}av)")
        item_id, props, children = item.unpack()
        self.assertEqual(item_id, 1)
        self.assertEqual(props["label"], "Show Phone Link")
        self.assertTrue(props["enabled"])
        self.assertEqual(children, [])

    def test_getlayout_variant_builds(self):
        layout = (
            0,
            {"children-display": GLib.Variant("s", "submenu")},
            [tray_sni._menu_item(1, "Show Phone Link"), tray_sni._menu_item(2, "Quit")],
        )
        v = GLib.Variant("(u(ia{sv}av))", (1, layout))
        self.assertEqual(v.get_type_string(), "(u(ia{sv}av))")
        _rev, (root_id, _props, kids) = v.unpack()
        self.assertEqual(root_id, 0)
        self.assertEqual([k[0] for k in kids], [1, 2])

    def test_getgroupproperties_variant_builds(self):
        entries = [
            (1, {"label": GLib.Variant("s", "Show Phone Link")}),
            (2, {"label": GLib.Variant("s", "Quit")}),
        ]
        v = GLib.Variant("(a(ia{sv}))", (entries,))
        self.assertEqual(v.get_type_string(), "(a(ia{sv}))")

    def test_menu_interface_xml_parses_with_expected_signatures(self):
        from gi.repository import Gio
        info = Gio.DBusNodeInfo.new_for_xml(tray_sni._MENU_XML).interfaces[0]
        by_name = {m.name: m for m in info.methods}
        get_layout = by_name["GetLayout"]
        self.assertEqual(
            "".join(a.signature for a in get_layout.out_args), "u(ia{sv}av)"
        )


if __name__ == "__main__":
    unittest.main()
