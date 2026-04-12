#!/usr/bin/env python3
"""Phone Link for Linux — launch script."""
import sys


def main():
    try:
        import gi

        gi.require_version("Gtk", "4.0")
        gi.require_version("Adw", "1")
    except ValueError as e:
        print(f"Missing dependency: {e}")
        print(
            "Install with: sudo apt install python3-gi gir1.2-gtk-4.0 gir1.2-adw-1"
        )
        sys.exit(1)

    from phonelink.app import PhoneLinkApp

    app = PhoneLinkApp()
    return app.run(sys.argv)


if __name__ == "__main__":
    sys.exit(main())
