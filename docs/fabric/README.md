# The Fabric panel — pull files from your other devices

phonelink's **Fabric** tab is a window into [Loom](https://github.com/ethanward/loom), the accountless
personal/family device fabric. It lists files across your devices — including files whose bytes live
**only on another device** — and lets you **Open** one, which pulls it here (verified against its
content hash) and caches it, so the file follows you. No server holds your data; devices talk to each
other directly.

phonelink stays fully usable **without** Loom: if the SDK isn't installed or `loomd` isn't running,
the tab shows a short setup hint instead of an error.

## How it fits together

```
phonelink Fabric panel ──▶ loom_sdk.Loom ──▶ loomd control socket ──▶ the fabric (Iroh)
   (ui/fabric_panel.py)     (optional dep)     ~/.local/state/loom/     content + catalog
```

- `phonelink/ui/fabric_panel.py` — the GTK view.
- `phonelink/loom_bridge.py` — imports `loom_sdk` lazily; drives blocking calls off the UI thread.
- `loom_sdk` — the thin client (`loom` repo, `sdk/python`).
- `loomd` — the per-device daemon that serves + fetches content (`loom` repo).

## Setup

1. **Build + run `loomd`** on this device (from the `loom` repo):
   ```bash
   cargo build --release
   ./target/release/loomd
   #   device address : 6cae…383a0@100.x.y.z:PORT   ← copy this
   #   control socket : ~/.local/state/loom/control.sock
   ```
2. **Make the SDK importable** by phonelink (either is fine):
   ```bash
   pip install -e /path/to/loom/sdk/python        # installs the `loom_sdk` package
   # …or add /path/to/loom/sdk/python to PYTHONPATH before launching phonelink
   ```
3. Launch phonelink and open the **Fabric** tab. It refreshes automatically.

## Pull a file that lives only on another device (the M5 proof)

On **device A** (say your laptop), publish a file — its bytes stay on A:
```bash
loom  # (from the loom repo) — or use the Python SDK: Loom().add("docs/invoice.pdf", data)
```
On **device B** (this one), tell loomd how to reach A, then sync:
```bash
loom peers add 6cae…383a0@100.x.y.z:PORT   # A's device address from its startup line
loom sync
```
Now open the **Fabric** tab in phonelink: `docs/invoice.pdf` appears, listed as held by A. Click its
**Open** (download) button — phonelink pulls the bytes from A, verifies them, saves the file to your
Downloads folder, and shows a toast naming the source device. Device B is now a holder too: the file
opens even if A later goes offline.

> Over a real network this works from anywhere once both devices are on your WireGuard mesh — the mesh
> just supplies A's address. See `deploy/RUNBOOK.md` in the loom repo.

## Limits at M5

- **Peers are added explicitly** (no auto-discovery yet).
- **Read + open** only. Sharing, permissions, and encryption arrive in Loom's Phase 1 (M6–M8).
